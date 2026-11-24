use super::{cronus, settings, store};
use crate::{AppState, runs};
use regex::Regex;
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    sync::{Arc, OnceLock},
    time::Duration,
};
use tokio::sync::oneshot;

struct WaitPauseGuard {
    state: Arc<AppState>,
    run_id: String,
    generation: u64,
    worker_id: String,
    active: bool,
}

impl WaitPauseGuard {
    fn new(state: Arc<AppState>, run_id: String, generation: u64, worker_id: String) -> Self {
        if let Ok(db) = runs::open_control_db(&state, &run_id) {
            let _ = store::begin_wait_pause(&db, generation, &worker_id);
        }
        Self {
            state,
            run_id,
            generation,
            worker_id,
            active: true,
        }
    }

    fn close(mut self) {
        if self.active {
            self.active = false;
            if let Ok(db) = runs::open_control_db(&self.state, &self.run_id) {
                let _ = store::end_wait_pause(&db, self.generation, &self.worker_id);
            }
        }
    }
}

impl Drop for WaitPauseGuard {
    fn drop(&mut self) {
        if self.active {
            self.active = false;
            if let Ok(db) = runs::open_control_db(&self.state, &self.run_id) {
                let _ = store::end_wait_pause(&db, self.generation, &self.worker_id);
            }
        }
    }
}

fn token_redact_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"(?i)\b(bearer\s+[a-z0-9._\-+=]+|sk-[a-z0-9_\-]{12,}|gh[pousr]_[a-z0-9_]{12,})\b",
        )
        .expect("redact regex")
    })
}

fn secret_header_key(key: &str) -> bool {
    let lower = key.to_ascii_lowercase();
    matches!(
        lower.as_str(),
        "authorization"
            | "proxy-authorization"
            | "x-api-key"
            | "cookie"
            | "set-cookie"
            | "x-auth-token"
    ) || lower.contains("token")
        || lower.contains("secret")
        || lower.contains("password")
}

pub(super) fn redact_text(s: &str, operator_secrets: &[String], max_bytes: usize) -> String {
    let mut masked = runs::worker::redact_operator_secrets(s, operator_secrets);
    masked = token_redact_re()
        .replace_all(&masked, "[REDACTED]")
        .into_owned();
    if masked.len() > max_bytes {
        let mut end = max_bytes;
        while end > 0 && !masked.is_char_boundary(end) {
            end -= 1;
        }
        masked.truncate(end);
        masked.push_str("…");
    }
    masked
}

pub(super) fn redact_value_with_limit(
    value: &Value,
    operator_secrets: &[String],
    max_str_bytes: usize,
) -> Value {
    match value {
        Value::String(s) => Value::String(redact_text(s, operator_secrets, max_str_bytes)),
        Value::Array(items) => Value::Array(
            items
                .iter()
                .take(24)
                .map(|v| redact_value_with_limit(v, operator_secrets, max_str_bytes))
                .collect(),
        ),
        Value::Object(map) => {
            let mut out = serde_json::Map::new();
            for (k, v) in map.iter().take(32) {
                if secret_header_key(k) {
                    out.insert(k.clone(), json!("[REDACTED]"));
                } else {
                    out.insert(
                        k.clone(),
                        redact_value_with_limit(v, operator_secrets, max_str_bytes),
                    );
                }
            }
            Value::Object(out)
        }
        _ => value.clone(),
    }
}

fn redact_value(value: &Value, operator_secrets: &[String]) -> Value {
    redact_value_with_limit(value, operator_secrets, 1536)
}

fn redacted_request_view(
    req: &Value,
    eval: &cronus::GuardEvaluation,
    operator_secrets: &[String],
) -> Value {
    let tool_name = req["tool_name"].as_str().unwrap_or("tool");
    let input_redacted = redact_value(&req["input"], operator_secrets);
    let command = if tool_name == "bash" {
        input_redacted["command"].as_str().map(str::to_owned)
    } else {
        None
    };
    let http_summary = if tool_name == "http_request" {
        let method = input_redacted["method"].as_str().unwrap_or("GET");
        let url = input_redacted["url"].as_str().unwrap_or("");
        Some(format!("{method} {url}"))
    } else {
        None
    };
    let script_summary = req["script_preview"].as_str().map(|preview| {
        redact_value(&Value::String(preview.to_owned()), operator_secrets)
            .as_str()
            .unwrap_or("")
            .to_owned()
    });
    let action_summary = command
        .clone()
        .or_else(|| http_summary.clone())
        .or_else(|| script_summary.clone())
        .unwrap_or_else(|| tool_name.to_owned());

    json!({
        "tool_call_id": req["tool_call_id"],
        "tool_name": tool_name,
        "action_summary": action_summary,
        "command": command,
        "http_summary": http_summary,
        "script_summary": script_summary,
        "args_redacted": input_redacted.to_string(),
        "worker_id": req["worker_id"],
        "worker_role": req["worker_role"],
        "created_at_ms": req["created_at_ms"],
        "fingerprint": eval.action_fingerprint,
    })
}

fn sanitize_stem(call_id: &str) -> String {
    call_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect()
}

fn write_decision_file(
    guard_ipc_dir: &Path,
    stem: &str,
    call_id: &str,
    decision: &str,
    reason: &str,
    approval_id: Option<&str>,
    terminate: bool,
) -> Result<(), String> {
    let payload = json!({
        "tool_call_id": call_id,
        "decision": decision,
        "reason": reason,
        "approval_id": approval_id,
        "terminate": terminate,
    });
    let path = guard_ipc_dir.join(format!("res-{stem}.json"));
    runs::worker::write_atomic_json(&path, &payload).map_err(|e| e.to_string())
}

fn pending_approval_event_payload(
    call_id: &str,
    approval_id: &str,
    req_view: &Value,
    eval: &cronus::GuardEvaluation,
    metrics: &Value,
) -> Value {
    json!({
        "approval": {
            "approval_id": approval_id,
            "tool_call_id": call_id,
            "tool_name": req_view["tool_name"],
            "action_summary": req_view["action_summary"],
            "command": req_view["command"],
            "http_summary": req_view["http_summary"],
            "script_summary": req_view["script_summary"],
            "args_redacted": req_view["args_redacted"],
            "worker_id": req_view["worker_id"],
            "worker_role": req_view["worker_role"],
            "delivery_status": "not_sent",
            "status": "pending",
            "reason": eval.reason,
            "risk_level": eval.risk_level,
            "cronus": eval.to_frontend_json(),
            "created_at_ms": req_view["created_at_ms"],
        },
        "egress_state": "held",
        "metrics": metrics,
    })
}

fn settled_event_payload(
    call_id: &str,
    approval_id: Option<&str>,
    req_view: &Value,
    eval: &cronus::GuardEvaluation,
    decision: &str,
    delivery_status: &str,
    note: Option<&str>,
    metrics: &Value,
) -> Value {
    json!({
        "receipt": {
            "receipt_id": call_id,
            "tool_call_id": call_id,
            "approval_id": approval_id,
            "tool_name": req_view["tool_name"],
            "action_summary": req_view["action_summary"],
            "worker_id": req_view["worker_id"],
            "worker_role": req_view["worker_role"],
            "decision": decision,
            "delivery_status": delivery_status,
            "note": note.unwrap_or(&eval.reason),
            "cronus": eval.to_frontend_json(),
            "ts": req_view["created_at_ms"],
        },
        "metrics": metrics,
    })
}

fn process_settled_files(
    state: &AppState,
    run_id: &str,
    solver_id: &str,
    generation: u64,
    guard_ipc_dir: &Path,
) {
    let Ok(entries) = std::fs::read_dir(guard_ipc_dir) else {
        return;
    };
    let mut paths: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("settled-") && n.ends_with(".json"))
        })
        .collect();
    paths.sort();

    let Ok(db) = runs::open_control_db(state, run_id) else {
        return;
    };
    for path in paths {
        let Ok(raw) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(doc) = serde_json::from_str::<Value>(&raw) else {
            let _ = std::fs::remove_file(&path);
            continue;
        };
        let Some(call_id) = doc["tool_call_id"].as_str().filter(|s| !s.is_empty()) else {
            let _ = std::fs::remove_file(&path);
            continue;
        };
        let summary = doc["summary"].as_str().unwrap_or("");
        if let Ok(Some(mut view)) = store::settle_call(&db, generation, call_id, summary) {
            view["ts"] = doc["settled_at_ms"].clone();
            let metrics = store::metrics(&db, generation).unwrap_or(Value::Null);
            runs::emit_event(
                state,
                run_id,
                Some(solver_id),
                "guard.action.settled",
                json!({"receipt": view, "metrics": metrics}),
            );
        }
        let _ = std::fs::remove_file(&path);
    }
}

async fn handle_request_file(
    state: &Arc<AppState>,
    run_id: &str,
    solver_id: &str,
    worker_role: &str,
    step_goal: &str,
    generation: u64,
    guard_ipc_dir: &Path,
    req_path: &Path,
    contract: &Value,
    operator_secrets: &[String],
    shutdown: &mut oneshot::Receiver<()>,
) {
    let Ok(raw) = std::fs::read_to_string(req_path) else {
        return;
    };
    let Ok(req) = serde_json::from_str::<Value>(&raw) else {
        let _ = std::fs::remove_file(req_path);
        return;
    };
    let call_id = req["tool_call_id"].as_str().unwrap_or("").trim();
    if call_id.is_empty() {
        let _ = std::fs::remove_file(req_path);
        return;
    }
    let stem = sanitize_stem(call_id);
    if stem.is_empty() {
        let _ = std::fs::remove_file(req_path);
        return;
    }
    if req["worker_id"].as_str().is_some_and(|w| w != solver_id)
        || req["execution_generation"].as_u64() != Some(generation)
    {
        let _ = write_decision_file(
            guard_ipc_dir,
            &stem,
            call_id,
            "emergency_stop",
            "Stale execution generation or mismatched worker identity",
            None,
            true,
        );
        let _ = std::fs::remove_file(req_path);
        return;
    }

    let handle = state.runs.handle(state, run_id);
    let mut waited_for_sibling: Option<WaitPauseGuard> = None;

    let (decision, approval_id, eval, req_view, metrics) = loop {
        if shutdown.try_recv().is_ok() {
            waited_for_sibling.take().map(WaitPauseGuard::close);
            return;
        }
        let held_or_revoked = {
            let db = match runs::open_control_db(state, run_id) {
                Ok(db) => db,
                Err(_) => return,
            };
            store::state_tuple(&db, generation)
                .map(|(s, _)| s)
                .unwrap_or_else(|_| "revoked".into())
        };
        if held_or_revoked == "revoked" {
            waited_for_sibling.take().map(WaitPauseGuard::close);
            let _ = write_decision_file(
                guard_ipc_dir,
                &stem,
                call_id,
                "emergency_stop",
                "Run egress authorization revoked by emergency stop",
                None,
                true,
            );
            let _ = std::fs::remove_file(req_path);
            return;
        }
        if held_or_revoked == "held" {
            if waited_for_sibling.is_none() {
                waited_for_sibling = Some(WaitPauseGuard::new(
                    Arc::clone(state),
                    run_id.to_owned(),
                    generation,
                    solver_id.to_owned(),
                ));
            }
            tokio::select! {
                _ = handle.guard_notify.notified() => continue,
                _ = tokio::time::sleep(Duration::from_millis(100)) => continue,
                _ = &mut *shutdown => {
                    waited_for_sibling.take().map(WaitPauseGuard::close);
                    return;
                }
            }
        }
        waited_for_sibling.take().map(WaitPauseGuard::close);

        let _eval_guard = handle.guard_eval_lock.lock().await;
        let current_egress = runs::open_control_db(state, run_id)
            .ok()
            .and_then(|db| store::state_tuple(&db, generation).ok())
            .map(|(s, _)| s)
            .unwrap_or_else(|| "revoked".into());
        if current_egress == "held" {
            continue;
        }
        if current_egress == "revoked" {
            let _ = write_decision_file(
                guard_ipc_dir,
                &stem,
                call_id,
                "emergency_stop",
                "Run egress authorization revoked by emergency stop",
                None,
                true,
            );
            let _ = std::fs::remove_file(req_path);
            return;
        }

        let cfg = settings::load(state).unwrap_or_default();
        let tool_name = req["tool_name"].as_str().unwrap_or("unknown");
        let input = &req["input"];
        let script_preview = req["script_preview"].as_str();
        let fp = cronus::action_fingerprint(tool_name, input, script_preview);
        let previously_denied = runs::open_control_db(state, run_id)
            .ok()
            .and_then(|db| store::has_denied_fingerprint(&db, generation, &fp).ok())
            .unwrap_or(false);
        let eval = cronus::evaluate_action(
            &cfg,
            contract,
            worker_role,
            Some(step_goal),
            tool_name,
            input,
            script_preview,
            previously_denied,
            operator_secrets,
        )
        .await;
        let req_view = redacted_request_view(&req, &eval, operator_secrets);
        let db = match runs::open_control_db(state, run_id) {
            Ok(db) => db,
            Err(_) => return,
        };
        match store::record_evaluation(&db, generation, call_id, &req_view, &eval) {
            Ok((d, aid, moved_to_held, _inserted)) => {
                if d == "held_retry" {
                    continue;
                }
                if moved_to_held {
                    runs::emit_event(
                        state,
                        run_id,
                        None,
                        "guard.egress.state_changed",
                        json!({"egress_state": "held", "execution_generation": generation}),
                    );
                }
                let metrics = store::metrics(&db, generation).unwrap_or(Value::Null);
                break (d, aid, eval, req_view, metrics);
            }
            Err(_) => {
                let _ = write_decision_file(
                    guard_ipc_dir,
                    &stem,
                    call_id,
                    "deny",
                    "Guard persistence failure (fail-closed)",
                    None,
                    false,
                );
                let _ = std::fs::remove_file(req_path);
                return;
            }
        }
    };

    if decision == "emergency_stop" {
        let _ = write_decision_file(
            guard_ipc_dir,
            &stem,
            call_id,
            "emergency_stop",
            "Run egress authorization revoked by emergency stop",
            None,
            true,
        );
        let _ = std::fs::remove_file(req_path);
        return;
    }

    if decision == "deny" {
        let _ = write_decision_file(
            guard_ipc_dir,
            &stem,
            call_id,
            "deny",
            &eval.reason,
            None,
            false,
        );
        runs::emit_event(
            state,
            run_id,
            Some(solver_id),
            "guard.action.settled",
            settled_event_payload(
                call_id,
                None,
                &req_view,
                &eval,
                "deny",
                "not_sent",
                Some(&eval.reason),
                &metrics,
            ),
        );
        let _ = std::fs::remove_file(req_path);
        return;
    }

    if decision == "allow" {
        let dispatched = runs::open_control_db(state, run_id)
            .ok()
            .and_then(|db| store::consume_for_dispatch(&db, generation, call_id, None, true).ok())
            .unwrap_or(false);
        if !dispatched {
            let _ = write_decision_file(
                guard_ipc_dir,
                &stem,
                call_id,
                "emergency_stop",
                "Run egress authorization revoked before dispatch",
                None,
                true,
            );
        } else {
            let _ = write_decision_file(
                guard_ipc_dir,
                &stem,
                call_id,
                "allow",
                &eval.reason,
                None,
                false,
            );
            runs::emit_event(
                state,
                run_id,
                Some(solver_id),
                "guard.action.settled",
                settled_event_payload(
                    call_id,
                    None,
                    &req_view,
                    &eval,
                    "allow",
                    "sent",
                    Some(&eval.reason),
                    &metrics,
                ),
            );
        }
        let _ = std::fs::remove_file(req_path);
        return;
    }

    let Some(aid) = approval_id else {
        let _ = write_decision_file(
            guard_ipc_dir,
            &stem,
            call_id,
            "deny",
            "Missing approval identifier (fail-closed)",
            None,
            false,
        );
        let _ = std::fs::remove_file(req_path);
        return;
    };

    runs::emit_event(
        state,
        run_id,
        Some(solver_id),
        "guard.approval.requested",
        pending_approval_event_payload(call_id, &aid, &req_view, &eval, &metrics),
    );

    let pause_guard = WaitPauseGuard::new(
        Arc::clone(state),
        run_id.to_owned(),
        generation,
        solver_id.to_owned(),
    );
    let (action, note) = loop {
        if let Ok(db) = runs::open_control_db(state, run_id)
            && let Ok(Some(resolved)) = store::poll_decision(&db, generation, &aid)
        {
            break resolved;
        }
        tokio::select! {
            _ = handle.guard_notify.notified() => {}
            _ = tokio::time::sleep(Duration::from_millis(100)) => {}
            _ = &mut *shutdown => {
                pause_guard.close();
                return;
            }
        }
    };
    pause_guard.close();

    let Ok(db) = runs::open_control_db(state, run_id) else {
        let _ = write_decision_file(
            guard_ipc_dir,
            &stem,
            call_id,
            "emergency_stop",
            "Unable to open guard DB on resolution",
            Some(&aid),
            true,
        );
        let _ = std::fs::remove_file(req_path);
        return;
    };

    match action.as_str() {
        "allow_once" => {
            if store::consume_for_dispatch(&db, generation, call_id, Some(&aid), true)
                .unwrap_or(false)
            {
                let why = note
                    .clone()
                    .unwrap_or_else(|| "Approved once by operator".into());
                let _ = write_decision_file(
                    guard_ipc_dir,
                    &stem,
                    call_id,
                    "allow",
                    &why,
                    Some(&aid),
                    false,
                );
                let latest_metrics = store::metrics(&db, generation).unwrap_or(Value::Null);
                runs::emit_event(
                    state,
                    run_id,
                    Some(solver_id),
                    "guard.action.settled",
                    settled_event_payload(
                        call_id,
                        Some(&aid),
                        &req_view,
                        &eval,
                        "allow_once",
                        "sent",
                        note.as_deref(),
                        &latest_metrics,
                    ),
                );
            } else {
                let _ = write_decision_file(
                    guard_ipc_dir,
                    &stem,
                    call_id,
                    "emergency_stop",
                    "Approval consumption failed or revoked before dispatch",
                    Some(&aid),
                    true,
                );
            }
        }
        "deny_once" => {
            let _ = store::consume_for_dispatch(&db, generation, call_id, Some(&aid), false);
            let why = note
                .clone()
                .unwrap_or_else(|| "Denied once by operator".into());
            let _ = write_decision_file(
                guard_ipc_dir,
                &stem,
                call_id,
                "deny",
                &why,
                Some(&aid),
                false,
            );
            let latest_metrics = store::metrics(&db, generation).unwrap_or(Value::Null);
            runs::emit_event(
                state,
                run_id,
                Some(solver_id),
                "guard.action.settled",
                settled_event_payload(
                    call_id,
                    Some(&aid),
                    &req_view,
                    &eval,
                    "deny_once",
                    "not_sent",
                    note.as_deref(),
                    &latest_metrics,
                ),
            );
        }
        _ => {
            let why = note.unwrap_or_else(|| "Emergency stop revoked approval".into());
            let _ = write_decision_file(
                guard_ipc_dir,
                &stem,
                call_id,
                "emergency_stop",
                &why,
                Some(&aid),
                true,
            );
        }
    }
    let _ = std::fs::remove_file(req_path);
}

pub(in crate::runs) struct WorkerGuardHandle {
    shutdown_tx: Option<oneshot::Sender<()>>,
    abort: tokio::task::AbortHandle,
}

impl WorkerGuardHandle {
    pub(in crate::runs) fn stop(mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
        self.abort.abort();
    }
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
) -> WorkerGuardHandle {
    let (shutdown_tx, mut shutdown_rx) = oneshot::channel();
    let run_for_handle = run_id.clone();
    let state_for_handle = Arc::clone(&state);
    let task = tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_millis(50));
        loop {
            tokio::select! {
                _ = &mut shutdown_rx => {
                    process_settled_files(&state, &run_id, &solver_id, generation, &guard_ipc_dir);
                    break;
                }
                _ = tick.tick() => {
                    process_settled_files(&state, &run_id, &solver_id, generation, &guard_ipc_dir);
                    let Ok(entries) = std::fs::read_dir(&guard_ipc_dir) else {
                        continue;
                    };
                    let mut reqs: Vec<PathBuf> = entries
                        .flatten()
                        .map(|e| e.path())
                        .filter(|p| {
                            p.file_name()
                                .and_then(|n| n.to_str())
                                .is_some_and(|n| n.starts_with("req-") && n.ends_with(".json"))
                        })
                        .collect();
                    reqs.sort();
                    for req_path in reqs {
                        handle_request_file(
                            &state,
                            &run_id,
                            &solver_id,
                            &worker_role,
                            &step_goal,
                            generation,
                            &guard_ipc_dir,
                            &req_path,
                            &contract,
                            &operator_secrets,
                            &mut shutdown_rx,
                        )
                        .await;
                        process_settled_files(&state, &run_id, &solver_id, generation, &guard_ipc_dir);
                    }
                }
            }
        }
    });
    let abort = task.abort_handle();
    state_for_handle
        .runs
        .handle(&state_for_handle, &run_for_handle)
        .worker_tasks
        .lock()
        .unwrap()
        .push(abort.clone());
    WorkerGuardHandle {
        shutdown_tx: Some(shutdown_tx),
        abort,
    }
}
