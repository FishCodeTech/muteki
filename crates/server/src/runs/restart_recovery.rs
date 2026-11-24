//! A new Web process owns no coordinator, planner or Worker task from the previous epoch.
//! Runner commands and the Run workspace outlive it, so a still-running Run is re-attached by
//! evidence: Worker requests are drained and Runner exits are awaited. Only then does the Run
//! move on: a coordinator Run restarts its coordinator from the saved execution plan, which
//! reads back what is already proven from the Run graph, and any other Run is settled.
//! No Worker is ever replayed, and no coordinator is started while its old Workers still run.
use super::*;

const TABLE: &str = "runner_orphan_runs";

/// Record the Runs that were running when the previous Web process stopped.
pub(crate) fn mark_for(state:&AppState,module:Option<crate::business::Module>)->Result<(),String>{
    let epoch = state.chat.epoch();
    let remote: HashSet<String> = state.store.list("runner_controller_runs").map_err(|e| e.to_string())?
        .iter().filter_map(|row| row["run_id"].as_str().map(str::to_owned)).collect();
    for run in state.store.list("runs").map_err(|e| e.to_string())? {
        if module.is_some_and(|module|crate::business::Module::for_run(&run)!=module){continue;}
        let Some(id) = run["run_id"].as_str() else { continue };
        if run["running"] != true || remote.contains(id) || run["src_task_id"].is_string()
            || matches!(run["control_mode"].as_str(), Some("frozen" | "quiesced"))
            || runner_stop_recovery::pending(state, &run)?
            || queued_restart::pending(state, id)? { continue; }
        let command = state.store.list("run_execution_commands").map_err(|e| e.to_string())?.into_iter()
            .rev().find(|row| row["run_id"] == id && matches!(row["action"].as_str(), Some("start" | "resolve")));
        let coordinator = command.as_ref().is_some_and(|row| row["coordinator"] != false);
        let mut orphan = json!({"run_id": id, "generation": run["execution_generation"].as_u64().unwrap_or(1),
            "host_epoch": epoch, "state": "reattaching", "coordinator": coordinator, "started_at": now_secs(),
            "command": command.as_ref().and_then(|row| row["command_id"].as_str())});
        if let Some(prior) = state.store.get(TABLE, id).map_err(|e| e.to_string())?
            && prior["generation"] == orphan["generation"] && prior["state"] == "reattaching" {
            for key in ["finished_solvers", "last_stop_error"] {
                if let Some(value) = prior.get(key) { orphan[key] = value.clone(); }
            }
        }
        state.store.put(TABLE, id, &orphan)
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}

pub(super) fn spawn(state: Arc<AppState>) {
    tokio::spawn(async move {
        loop {
            if !state.admission.draining() {
                if let Err(error) = reconcile(&state).await { tracing::warn!(%error, "Run re-attachment is pending"); }
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    });
}

fn live_commands(state: &AppState, run: &str) -> Result<Vec<Value>, String> {
    Ok(state.store.list("runner_commands").map_err(|e| e.to_string())?.into_iter()
        .filter(|row| row["run_id"] == run && !crate::runner_execution::facts(row).stopped).collect())
}

async fn teardown(container: &str, generation: u64) -> Result<(), String> {
    if let Some(socket) = crate::runner::configured_socket() {
        crate::runner::exchange(&socket, &format!("/v1/containers/{container}/teardown"), Some(json!({"generation": generation}))).await.map(|_| ())
    } else { container_exec::teardown_run_container_checked(container).await }
}

/// Restart the coordinator of a Run whose pre-restart Workers have exited, from the execution
/// plan saved with its start command. Returns the seats restarted, or why it cannot be resumed.
fn resume_coordinator(state: &Arc<AppState>, id: &str, run: &Value, row: &Value) -> Result<usize, String> {
    if row["resumed_at"].is_number() { return Err("its coordinator was already restarted once".into()); }
    if matches!(run["control_mode"].as_str(), Some("frozen" | "quiesced")) { return Err("the Run is held by an operator".into()); }
    let generation = run["execution_generation"].as_u64().unwrap_or(1);
    let command = row["command"].as_str().ok_or("no start command was saved for it")?;
    let marker = state.store.get("run_execution_commands", command).map_err(|e| e.to_string())?
        .ok_or("its saved start command is gone")?;
    if marker["generation"].as_u64().unwrap_or(0) != generation {
        return Err("its saved execution plan belongs to another generation".into());
    }
    if marker["coordinator"] == false { return Err("its saved plan runs Workers without a coordinator".into()); }
    let specs: Vec<worker::WorkerSpec> = serde_json::from_value(marker["specs"].clone())
        .map_err(|error| format!("its saved execution plan cannot be read: {error}"))?;
    if specs.is_empty() { return Err("its saved execution plan has no Worker seat".into()); }
    if execution_budget::remaining(state, id, "execute")?.is_some_and(|left| left <= 0.) {
        return Err("its execution budget is spent".into());
    }
    usage::restore(state, id)?;
    coordinator::spawn_coordinator(state.clone(), id.to_owned(), specs.clone(), marker["challenge"].clone(),
        Some("A previous Web process stopped while this Run was executing and its Workers are gone. \
Read the Run graph for what is already proven, then continue from there.".to_owned()));
    Ok(specs.len())
}

async fn reconcile(state: &Arc<AppState>) -> Result<(), String> {
    for mut row in state.store.list(TABLE).map_err(|e| e.to_string())?.into_iter()
        .filter(|row| row["state"] == "remote_resume_pending") {
        let id = row["run_id"].as_str().ok_or("Remote recovery Run ID missing")?.to_owned();
        let Some(run) = state.store.get("runs", &id).map_err(|e| e.to_string())? else { continue; };
        if !crate::business::run_available(state,&run){continue;}
        if run["execution_generation"] != row["generation"] || run["control_generation"] != row["control_generation"]
            || run["solved"] == true || matches!(run["control_mode"].as_str(), Some("frozen" | "quiesced")) {
            row["state"] = json!("superseded");
        } else {
            let binding = state.store.get("runner_controller_runs", &format!("{}:{}", id, row["generation"].as_u64().unwrap_or(1)))
                .map_err(|e| e.to_string())?.ok_or("Remote recovery binding missing")?;
            if binding["state"] != "stopped" || binding["publication_pending"] == true { continue; }
            let response = resolve_run(State(state.clone()), Path(id.clone()), Some(Json(json!({
                "command_id":format!("remote-restart-{}-{}", id, row["generation"].as_u64().unwrap_or(1)),
                "expected_execution_generation":row["generation"],"expected_generation":row["control_generation"]
            })))).await;
            let status = response.status();
            let bytes = axum::body::to_bytes(response.into_body(), usize::MAX).await.map_err(|e| e.to_string())?;
            let receipt: Value = serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
            let resumed = status.is_success() && receipt["ok"] == true;
            row["http_status"] = json!(status.as_u16());
            row["receipt"] = receipt;
            row["state"] = json!(if resumed { "remote_resumed" } else { "resume_required" });
            emit_event(state, &id, None, "run.restart_recovery", json!({
                "state":if resumed { "coordinator_resumed" } else { "resume_required" },
                "coordinator_resumed":resumed,"http_status":status.as_u16(),"receipt":row["receipt"],
                "detail":if resumed { "Remote execution was stopped and archived; the saved Run continues in a new generation" }
                    else { "Automatic remote continuation was rejected; inspect the execution command receipt before continuing manually" }}));
        }
        state.store.put(TABLE, &id, &row).map_err(|e| e.to_string())?;
    }
    for mut row in state.store.list(TABLE).map_err(|e| e.to_string())?.into_iter()
        .filter(|row| row["state"] == "reattaching" && row["host_epoch"] == json!(state.chat.epoch())) {
        let id = row["run_id"].as_str().ok_or("Re-attached Run ID missing")?.to_owned();
        let handle = state.runs.handle(state, &id);
        let _guard = handle.delete_lock.lock().await;
        if handle.deleting.load(Ordering::SeqCst) { continue; }
        let finish = |row: &mut Value, outcome: &str| -> Result<(), String> {
            row["state"] = json!(outcome); row["settled_at"] = json!(now_secs());
            state.store.put(TABLE, &id, row).map_err(|e| e.to_string())
        };
        let Some(run) = state.store.get("runs", &id).map_err(|e| e.to_string())? else { finish(&mut row, "run_missing")?; continue };
        if !crate::business::run_available(state,&run){continue;}
        if run["running"] != true { finish(&mut row, "already_settled")?; continue; }
        if runner_stop_recovery::pending(state, &run)? { continue; }
        if state.runs.active_worker_count(&id) > 0 || run["execution_generation"].as_u64().unwrap_or(1) != row["generation"] {
            finish(&mut row, "superseded")?; continue;
        }
        if !state.runs.claim_execution_owner(state, &id)? { continue; }
        let generation = row["generation"].as_u64().unwrap_or(1);
        let live = live_commands(state, &id)?;
        let sdir = session_dir(state, &id);
        let solvers: HashSet<String> = state.store.list("runner_commands").map_err(|e| e.to_string())?.iter()
            .filter(|command| command["run_id"] == id).filter_map(|command| command["solver_id"].as_str().map(str::to_owned)).collect();
        let mut drained_solved = false;
        let mut finished_solvers: HashSet<String> = row["finished_solvers"].as_array().into_iter().flatten()
            .filter_map(|id| id.as_str().map(str::to_owned)).collect();
        for solver in &solvers {
            let ingress = sdir.join(".muteki-blackboard-ingress").join(solver);
            if ingress.is_dir() {
                let (solved, finished) = worker::drain_blackboard_ingress(state, &id, solver, &ingress, &sdir);
                drained_solved |= solved;
                if finished { finished_solvers.insert(solver.clone()); }
            }
        }
        if !finished_solvers.is_empty() {
            row["finished_solvers"] = json!(finished_solvers);
            state.store.put(TABLE, &id, &row).map_err(|e| e.to_string())?;
        }
        if let Some(socket) = crate::runner::configured_socket() {
            for command in &live {
                if (drained_solved || run["solved"] == true || command["solver_id"].as_str().is_some_and(|id| finished_solvers.contains(id)))
                    && let Some(worker) = command["result"]["worker_id"].as_str() {
                    if let Err(error) = crate::runner::exchange(&socket, "/v1/workers/signal", Some(json!({
                        "run_id":id,"generation":generation,"worker_id":worker,"signal":"TERM"
                    }))).await {
                        tracing::warn!(run_id=%id,worker_id=%worker,%error,"Orphan Worker TERM failed; retrying on the next reconciliation");
                        row["last_stop_error"] = json!(error);
                        state.store.put(TABLE, &id, &row).map_err(|e| e.to_string())?;
                    }
                }
            }
        }
        if !live.is_empty() {
            if row["live_commands"].as_u64() != Some(live.len() as u64) {
                row["live_commands"] = json!(live.len());
                state.store.put(TABLE, &id, &row).map_err(|e| e.to_string())?;
                emit_event(state, &id, None, "run.restart_recovery", json!({"state": "reattaching", "live_workers": live.len(),
                    "coordinator_resumed": false, "detail": "Workers kept running across the Web restart; their requests are still ingested"}));
            }
            continue;
        }
        let mut containers: Vec<(String, u64)> = Vec::new();
        for runtime in state.store.list("runner_runs").map_err(|e| e.to_string())? {
            let name = runtime["run_id"].as_str().unwrap_or("");
            if name.is_empty() || runtime["scope"] == "shared" || runtime["state"] != "active" { continue; }
            if name == id || runtime["owner_run_id"] == json!(id) { containers.push((name.to_owned(), runtime["generation"].as_u64().unwrap_or(generation))); }
        }
        let mut failures = Vec::new();
        for (name, generation) in containers { if let Err(error) = teardown(&name, generation).await { failures.push(format!("{name}: {error}")); } }
        if !failures.is_empty() {
            row["cleanup_pending"] = json!(failures);
            state.store.put(TABLE, &id, &row).map_err(|e| e.to_string())?;
            continue;
        }
        let run = state.store.get("runs", &id).map_err(|e| e.to_string())?.unwrap_or(run);
        let solved = run["solved"] == true || drained_solved;
        let coordinator = row["coordinator"] == true;
        let mut refused = String::new();
        if !solved && coordinator {
            let _ = worker_result::recover_projections(state, &id);
            match resume_coordinator(state, &id, &run, &row) {
                Ok(seats) => {
                    row["state"] = json!("coordinator_resumed");
                    row["resumed_at"] = json!(now_secs());
                    row["resumed_seats"] = json!(seats);
                    state.store.put(TABLE, &id, &row).map_err(|e| e.to_string())?;
                    emit_event(state, &id, None, "run.restart_recovery", json!({"state": "coordinator_resumed",
                        "coordinator_resumed": true, "seats": seats,
                        "detail": "The coordinator was restarted from its saved execution plan after the Web restart; proven Steps and facts were read back from the Run graph and unfinished Steps were reopened"}));
                    continue;
                }
                Err(reason) => refused = reason,
            }
        }
        let refusal = format!("The coordinator was not restarted after the Web restart because {refused}; Workers exited and no new work was dispatched");
        let detail = if solved { "Workers finished after the Web restart and their results were ingested" }
            else if coordinator { refusal.as_str() }
            else { "Workers exited after the Web restart without a result" };
        let _ = worker_result::settle_claims(state, &id, "interrupted", "process_restart");
        let _ = worker_result::recover_projections(state, &id);
        terminal::finish(state, &id, generation, None, solved, if solved { "solved_after_web_restart" } else { "interrupted_by_web_restart" }, Some(detail))?;
        row["outcome"] = json!({"solved": solved, "detail": detail});
        finish(&mut row, "settled")?;
    }
    Ok(())
}

/// Remote Workers outlive the Web process on their node. The controller relays their mailbox
/// into the Run workspace; this ingests it and reports whether a result solved the Run and
/// whether the Worker committed its final result.
pub(crate) fn drain_remote_ingress(state: &AppState, run: &str, solver: &str, workspace: &std::path::Path) -> (bool, bool) {
    let ingress = workspace.join(".muteki-blackboard-ingress").join(solver);
    if !ingress.is_dir() { return (false, false); }
    worker::drain_blackboard_ingress(state, run, solver, &ingress, &session_dir(state, run))
}

fn remote_run_open(state: &AppState, run: &str, generation: u64) -> Result<bool, String> {
    Ok(state.store.get("runs", run).map_err(|e| e.to_string())?.is_some_and(|row| row["running"] == true
        && row["execution_generation"].as_u64().unwrap_or(1) == generation)
        && state.runs.active_worker_count(run) == 0)
}

fn node_name(state: &AppState, node: &str) -> String {
    state.store.get("runner_nodes", node).ok().flatten()
        .and_then(|row| row["name"].as_str().map(str::to_owned)).unwrap_or_else(|| node.to_owned())
}

pub(crate) fn announce_remote_reattach(state: &AppState, run: &str, node_id: &str) {
    if !state.store.get("runs", run).ok().flatten().is_some_and(|row| row["running"] == true) { return; }
    let node = node_name(state, node_id);
    emit_event(state, run, None, "run.restart_recovery", json!({"state": "remote_reattaching", "node_id": node_id, "node_name": node,
        "coordinator_resumed": false,
        "detail": format!("The Web service restarted while Workers ran on execution node {node}; their requests are relayed again and the Run settles once the node confirms every exit")}));
}

/// Settle a remote Run whose pre-restart Workers have all exited. Unsolved coordinator Runs
/// continue through the normal resolve path after confirmed teardown. Returns whether the remote
/// execution scope may now be torn down: no process of this epoch owns it, including
/// auxiliary scopes such as a planner whose ID is not a Run.
pub(crate) fn settle_remote_run(state: &AppState, run: &str, generation: u64, node_id: &str, drained_solved: bool) -> Result<bool, String> {
    if !remote_run_open(state, run, generation)? { return Ok(true); }
    if !state.runs.claim_execution_owner(state, run)? { return Ok(false); }
    let current = state.store.get("runs", run).map_err(|e| e.to_string())?.ok_or("Remote Run missing")?;
    if runner_stop_recovery::pending(state, &current)? { return Ok(true); }
    let node = node_name(state, node_id);
    let solved = drained_solved || current["solved"] == true;
    let mut resume_pending = false;
    if !solved {
        let marker = state.store.list("run_execution_commands").map_err(|e| e.to_string())?
            .into_iter().rev().find(|row| row["run_id"] == run && row["generation"] == generation
                && matches!(row["action"].as_str(), Some("start" | "resolve")));
        if marker.is_some_and(|row| row["coordinator"] != false) {
            if !matches!(current["control_mode"].as_str(), Some("frozen" | "quiesced"))
                && execution_budget::remaining(state, run, "execute")?.is_none_or(|left| left > 0.) {
                state.store.put(TABLE, run, &json!({"run_id":run,"generation":generation,
                    "control_generation":current["control_generation"],"state":"remote_resume_pending"}))
                    .map_err(|e| e.to_string())?;
                resume_pending = true;
            }
        }
    }
    let detail = if solved {
        format!("Workers on execution node {node} finished across the Web restart and their results were ingested")
    } else if resume_pending {
        format!("Workers on execution node {node} exited across the Web restart; continuation is queued until their execution is stopped and artifacts are archived")
    } else {
        format!("Workers on execution node {node} exited across the Web restart and their artifacts were recovered; the coordinator is not restarted for remote Runs, so resolve the Run again to dispatch new work")
    };
    let _ = worker_result::settle_claims(state, run, "interrupted", "process_restart");
    let _ = worker_result::recover_projections(state, run);
    emit_event(state, run, None, "run.restart_recovery", json!({"state": "remote_settled", "node_id": node_id, "node_name": node,
        "coordinator_resumed": false, "solved": solved, "detail": detail}));
    terminal::finish(state, run, generation, None, solved, if solved { "solved_after_web_restart" } else { "interrupted_by_web_restart" }, Some(&detail))?;
    Ok(true)
}
