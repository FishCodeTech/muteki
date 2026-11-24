//! Emergency freeze holds processes, graph leases and the corresponding clocks.
use super::*;

const EFFECTS: &str = "run_freeze_effects";
fn save_phase(state: &AppState, journal: &Value) -> Result<(), String> {
    state
        .store
        .put(
            EFFECTS,
            journal["id"]
                .as_str()
                .ok_or("Freeze effect identity missing")?,
            journal,
        )
        .map_err(|e| e.to_string())
}

/// Intent and target identity are durable before the first physical signal.
pub(super) async fn apply(
    state: &Arc<AppState>,
    run_id: &str,
    command: &str,
    body: &Value,
    run: &mut Value,
) -> Result<Value, String> {
    let id = crate::runner::canonical_digest(json!([run_id, command, body["action"]]))?;
    if state
        .store
        .list(EFFECTS)
        .map_err(|e| e.to_string())?
        .iter()
        .any(|row| {
            row["run_id"] == run_id
                && row["generation"] == run["execution_generation"]
                && !matches!(row["phase"].as_str(), Some("settled" | "superseded"))
        })
    {
        return Err("A prior freeze transition still requires reconciliation".into());
    }
    if state
        .store
        .get(EFFECTS, &id)
        .map_err(|e| e.to_string())?
        .is_some()
    {
        return Err("Freeze transition already exists; inspect its control receipt while recovery settles it".into());
    }
    let mut journal = json!({"id":id,"run_id":run_id,"command_id":command,"generation":run["execution_generation"].as_u64().unwrap_or(1),"action":body["action"],"phase":"prepared","before":run,"before_paused":state.runs.handle(state,run_id).paused.load(Ordering::SeqCst),"targets":[],"observations":[],"started_at":now_secs()});
    save_phase(state, &journal)?;
    let result = apply_effect(state, run_id, command, body, run, &mut journal).await;
    match result {
        Ok(effect) => {
            journal["phase"] = json!(if effect["state"] == "partial" {
                "compensation_pending"
            } else {
                "effect_recorded"
            });
            journal["after"] = run.clone();
            journal["effect"] = effect.clone();
            save_phase(state, &journal)?;
            Ok(effect)
        }
        Err(error) => {
            journal["error"] = json!(error);
            if let Err(save_error) = save_phase(state, &journal) {
                tracing::error!(%save_error,"Freeze recovery journal update failed");
            }
            Err(error)
        }
    }
}

async fn signal_recorded(
    state: &Arc<AppState>,
    run_id: &str,
    id: &str,
    thread: &str,
    frozen: bool,
    journal: &mut Value,
) -> Result<bool, String> {
    journal["phase"] = json!("signalling");
    journal["pending_target"] = json!({"worker_id":id,"thread_id":thread,"frozen":frozen});
    save_phase(state, journal)?;
    let result = match signal(state, run_id, id, thread, frozen).await {
        Ok(false) => {
            recover_signal(
                state,
                run_id,
                journal["generation"].as_u64().unwrap_or(1),
                id,
                frozen,
                journal["command_id"]
                    .as_str()
                    .ok_or("Freeze command missing")?,
            )
            .await
        }
        other => other,
    };
    journal["observations"].as_array_mut().ok_or("Freeze observations malformed")?.push(match &result{Ok(confirmed)=>json!({"worker_id":id,"thread_id":thread,"frozen":frozen,"confirmed":confirmed,"checked_at":now_secs()}),Err(error)=>json!({"worker_id":id,"thread_id":thread,"frozen":frozen,"error":error,"checked_at":now_secs()})});
    journal["pending_target"] = Value::Null;
    save_phase(state, journal)?;
    result
}

/// A crash before the complete effect receipt compensates toward the original
/// state. A recorded effect only republishes its patch, never repeats signals.
pub(super) async fn recover_pending(state: &Arc<AppState>, run_id: &str) -> Result<(), String> {
    for mut row in state
        .store
        .list(EFFECTS)
        .map_err(|e| e.to_string())?
        .into_iter()
        .filter(|row| {
            row["run_id"] == run_id
                && !matches!(row["phase"].as_str(), Some("settled" | "superseded"))
        })
    {
        let command = row["command_id"]
            .as_str()
            .ok_or("Freeze command missing")?
            .to_owned();
        let conn = open_control_db(state, run_id)?;
        if row["phase"] == "effect_recorded" && control_journal::has_result(&conn, &command)? {
            row["phase"] = json!("settled");
            save_phase(state, &row)?;
            continue;
        }
        let Some(current) = state.store.get("runs", run_id).map_err(|e| e.to_string())? else {
            continue;
        };
        if current["execution_generation"].as_u64().unwrap_or(1)
            != row["generation"].as_u64().unwrap_or(1)
        {
            row["phase"] = json!("superseded");
            save_phase(state, &row)?;
            continue;
        }
        let control_generation: u64 = conn
            .query_row(
                "SELECT generation FROM control_commands WHERE command_id=?1",
                [&command],
                |r| r.get(0),
            )
            .map_err(|e| e.to_string())?;
        let mut after;
        let effect;
        if row["phase"] == "effect_recorded" {
            after = row["after"].clone();
            effect = row["effect"].clone();
        } else {
            let frozen = row["action"] != "freeze";
            let targets = row["targets"].as_array().cloned().unwrap_or_default();
            let mut failures = Vec::new();
            for target in targets {
                let id = target["worker_id"]
                    .as_str()
                    .ok_or("Freeze target missing")?;
                let thread = target["thread_id"]
                    .as_str()
                    .ok_or("Freeze thread missing")?;
                let generation = row["generation"].as_u64().unwrap_or(1);
                let result = signal(state, run_id, id, thread, frozen).await;
                let result = match result {
                    Ok(true) => Ok(true),
                    _ => recover_signal(state, run_id, generation, id, frozen, &command).await,
                };
                if let Err(error) = result {
                    failures.push(json!({"worker_id":id,"error":error}));
                }
            }
            if !failures.is_empty() {
                row["phase"] = json!("compensation_pending");
                row["recovery_failures"] = json!(failures);
                save_phase(state, &row)?;
                continue;
            }
            let graph = graph::SharedGraph::open(state, run_id)?;
            if row["action"] == "freeze" || current["running"] == false {
                graph.resume_control_leases(&command)?;
                if let Some(suspension) = row["before"]["freeze_control"]["suspension_id"].as_str()
                {
                    graph.resume_control_leases(suspension)?;
                }
            } else if let Some(suspension) =
                row["before"]["freeze_control"]["suspension_id"].as_str()
            {
                let ids = row["before"]["freeze_control"]["workers"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|w| w["worker_id"].as_str().map(str::to_owned))
                    .collect::<Vec<_>>();
                graph.suspend_control_leases(
                    suspension,
                    &ids,
                    row["before"]["freeze_control"]["run_wide"] == true,
                )?;
            }
            after = current.clone();
            state.runs.handle(state, run_id).paused.store(
                current["running"] != false && row["before_paused"] == true,
                Ordering::SeqCst,
            );
            effect = json!({"state":"failed","kind":"interrupted_freeze_compensated","action":row["action"],"targets":row["targets"]});
        }
        after["control_generation"] =
            json!(control_generation.max(current["control_generation"].as_u64().unwrap_or(0)));
        let response = json!({"ok":effect["state"]=="effect_observed","command_id":command,"generation":control_generation,"status":if effect["state"]=="effect_observed"{"applied"}else if effect["state"]=="failed"{"failed"}else{"partial"},"effect":effect});
        let before = if row["phase"] == "effect_recorded" {
            &row["before"]
        } else {
            &current
        };
        control_journal::finish(state, run_id, &conn, &command, before, &after, &response)?;
        row["phase"] = json!("settled");
        save_phase(state, &row)?;
    }
    Ok(())
}

async fn recover_signal(
    state: &Arc<AppState>,
    run: &str,
    generation: u64,
    worker: &str,
    frozen: bool,
    command: &str,
) -> Result<bool, String> {
    let commands = state
        .store
        .list("runner_commands")
        .map_err(|e| e.to_string())?;
    let row = commands.iter().find(|row| {
        row["run_id"] == run
            && row["generation"] == generation
            && (row["worker_id"] == worker || row["solver_id"] == worker)
    });
    if let Some(row) = row {
        if crate::runner_execution::facts(row).stopped {
            return Ok(false);
        }
        let actual = row["result"]["worker_id"]
            .as_str()
            .or_else(|| row["worker_id"].as_str())
            .ok_or("Recorded Worker runtime identity missing")?;
        if row["node_id"].as_str().is_some_and(|node| node != "local") {
            return crate::runner_remote::controller::signal_recorded(
                state,
                run,
                generation,
                actual,
                if frozen { "STOP" } else { "CONT" },
                command,
            )
            .await;
        }
        if let Some(socket) = crate::runner::configured_socket() {
            let response=crate::runner::exchange(&socket,"/v1/workers/signal",Some(json!({"run_id":run,"generation":generation,"worker_id":actual,"signal":if frozen{"STOP"}else{"CONT"}}))).await?;
            return if response["paused"] == frozen {
                Ok(true)
            } else {
                Err("Runner has not confirmed compensation state".into())
            };
        }
    }
    Err("No live owned runtime or durable exit evidence for freeze compensation".into())
}

fn run_scope(scope: &str) -> bool {
    scope == "global" || scope.starts_with("run:") || scope.starts_with("challenge:")
}
fn selected(worker: &ActiveWorker, scope: &str) -> bool {
    if run_scope(scope) {
        return true;
    }
    let Some((kind, value)) = scope.split_once(':') else {
        return false;
    };
    match kind {
        "worker" | "solver" => worker.solver_id == value,
        "engine" => worker.engine == value,
        "intent" => worker.intent_id == value,
        "lane" => worker.lane == value,
        _ => false,
    }
}
async fn signal(
    state: &Arc<AppState>,
    run_id: &str,
    id: &str,
    thread: &str,
    frozen: bool,
) -> Result<bool, String> {
    let handle = state.runs.handle(state, run_id);
    if handle
        .workers
        .lock()
        .unwrap()
        .get(id)
        .is_some_and(|worker| worker.thread_id != thread)
    {
        return Err("Freeze target was replaced by a different runtime".into());
    }
    let container = handle.container_sessions.lock().unwrap().get(id).cloned();
    if let Some((link, worker)) = container {
        let result = link
            .signal(&worker, if frozen { "STOP" } else { "CONT" }, None, None)
            .await?;
        return if !result.error.is_empty() {
            Err(result.error)
        } else if result.paused != frozen {
            Err("Container did not confirm its pause state".into())
        } else {
            Ok(true)
        };
    }
    let Some(session) = state.agents.get(thread).await else {
        return Ok(false);
    };
    let Some(pid) = session.process_group() else {
        return Err("Runtime has no owned local process group".into());
    };
    let root = native_chat::state_dir(state);
    tokio::task::spawn_blocking(move || {
        crate::runtime_guardian::set_frozen(&root, pid, frozen).map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())?
}
pub(super) fn suspended_seconds(run: &Value) -> f64 {
    run["budget_suspended_seconds"].as_f64().unwrap_or(0.)
        + run["budget_suspend_started_at"]
            .as_f64()
            .map(|at| (now_secs() - at).max(0.))
            .unwrap_or(0.)
}
async fn apply_effect(
    state: &Arc<AppState>,
    run_id: &str,
    command: &str,
    body: &Value,
    run: &mut Value,
    journal: &mut Value,
) -> Result<Value, String> {
    let handle = state.runs.handle(state, run_id);
    let scope = body["scope"].as_str().unwrap_or("global");
    let wide = run_scope(scope);
    let frozen = body["action"] == "freeze";
    let prior = run["freeze_control"].clone();
    if prior.is_object() && prior["scope"] != scope && !(wide && !frozen) {
        return Ok(
            json!({"state":"failed","code":"overlapping_freeze_scope","message":"Thaw the active scope before changing freeze scope"}),
        );
    }
    if frozen && prior.is_object() {
        return Ok(
            json!({"state":"effect_observed","kind":"already_frozen","targets":prior["workers"]}),
        );
    }
    if !frozen && !prior.is_object() {
        return Ok(json!({"state":"effect_observed","kind":"already_thawed","targets":[]}));
    }
    let graph = graph::SharedGraph::open(state, run_id)?;
    let freeze_started = now_secs();
    let before_paused = handle.paused.load(Ordering::SeqCst);
    let mut targets = handle
        .workers
        .lock()
        .unwrap()
        .values()
        .filter(|worker| !worker.abort.is_finished() && selected(worker, scope))
        .map(|worker| (worker.solver_id.clone(), worker.thread_id.clone()))
        .collect::<Vec<_>>();
    targets.sort();
    if !frozen {
        for row in prior["workers"].as_array().into_iter().flatten() {
            if let (Some(id), Some(thread)) = (row["worker_id"].as_str(), row["thread_id"].as_str())
            {
                if !targets.iter().any(|(key, _)| key == id) {
                    targets.push((id.into(), thread.into()));
                }
            }
        }
    }
    if frozen && targets.is_empty() && !wide {
        return Ok(json!({"state":"unknown","kind":"no_matching_worker","targets":[]}));
    }
    journal["targets"] = json!(
        targets
            .iter()
            .map(|(id, thread)| json!({"worker_id":id,"thread_id":thread}))
            .collect::<Vec<_>>()
    );
    save_phase(state, journal)?;
    if wide {
        handle.paused.store(true, Ordering::SeqCst);
    }
    let mut confirmed = Vec::new();
    let mut failures = Vec::new();
    for (id, thread) in &targets {
        match signal_recorded(state, run_id, id, thread, frozen, journal).await {
            Ok(true) => confirmed.push((id.clone(), thread.clone())),
            Ok(false) if !frozen => {}
            Ok(false) => {
                failures.push(json!({"worker_id":id,"error":"Worker runtime is no longer active"}))
            }
            Err(error) => failures.push(json!({"worker_id":id,"error":error})),
        }
    }
    if frozen && !failures.is_empty() {
        let mut rollback = Vec::new();
        for (id, thread) in &targets {
            if signal_recorded(state, run_id, id, thread, false, journal)
                .await
                .is_err()
            {
                rollback.push(id.clone());
            }
        }
        if rollback.is_empty() {
            handle.paused.store(before_paused, Ordering::SeqCst);
            return Ok(
                json!({"state":"failed","kind":"freeze_rolled_back","failures":failures,"targets":[]}),
            );
        }
        for id in &rollback {
            let _ = state.runs.request_worker_cancel(run_id, "worker", id);
        }
        confirmed.retain(|(id, _)| rollback.contains(id));
        failures.push(json!({"code":"freeze_rollback_unconfirmed","workers":rollback}));
    }
    if !frozen && !failures.is_empty() {
        let mut rollback_failures = Vec::new();
        for (id, thread) in &confirmed {
            match signal_recorded(state, run_id, id, thread, true, journal).await {
                Ok(true) => {}
                Ok(false) => {}
                Err(error) => {
                    rollback_failures.push(json!({"worker_id":id,"error":error}));
                    let _ = state.runs.request_worker_cancel(run_id, "worker", id);
                }
            }
        }
        handle.paused.store(true, Ordering::SeqCst);
        run["freeze_control"]["recovery_required"] = json!(true);
        return Ok(
            json!({"state":"partial","kind":"thaw_rolled_back","failures":failures,"rollback_failures":rollback_failures,"targets":confirmed.iter().map(|v|&v.0).collect::<Vec<_>>()}),
        );
    }
    if frozen {
        let workers = confirmed
            .iter()
            .map(|row| row.0.clone())
            .collect::<Vec<_>>();
        let leases = match graph.suspend_control_leases(command, &workers, wide) {
            Ok(count) => count,
            Err(error) => {
                let mut rollback = Vec::new();
                for (id, thread) in &targets {
                    if signal_recorded(state, run_id, id, thread, false, journal)
                        .await
                        .is_err()
                    {
                        rollback.push(id.clone());
                    }
                }
                if rollback.is_empty() {
                    handle.paused.store(before_paused, Ordering::SeqCst);
                    return Err(format!(
                        "Lease suspension failed; processes thawed: {error}"
                    ));
                }
                confirmed.retain(|(id, _)| rollback.contains(id));
                for id in &rollback {
                    let _ = state.runs.request_worker_cancel(run_id, "worker", id);
                }
                failures.push(
                    json!({"code":"lease_guard_failed","error":error,"rollback_failures":rollback}),
                );
                0
            }
        };
        run["freeze_control"] = json!({"scope":scope,"suspension_id":command,"run_wide":wide,"started_at":freeze_started,"was_paused":before_paused,"workers":confirmed.iter().map(|(id,thread)|json!({"worker_id":id,"thread_id":thread})).collect::<Vec<_>>(),"lease_count":leases});
        if wide {
            run["control_mode"] = json!("frozen");
            run["budget_suspend_started_at"] = json!(freeze_started);
            run["awaiting_operator"] = json!("frozen");
        }
        Ok(
            json!({"state":if failures.is_empty(){"effect_observed"}else{"partial"},"kind":"runtime_frozen","targets":workers,"lease_count":leases,"failures":failures}),
        )
    } else {
        let suspension = prior["suspension_id"]
            .as_str()
            .ok_or("Freeze suspension ID missing")?;
        let leases = match graph.resume_control_leases(suspension) {
            Ok(count) => count,
            Err(error) => {
                let mut rollback_failures = Vec::new();
                for (id, thread) in &confirmed {
                    match signal_recorded(state, run_id, id, thread, true, journal).await {
                        Ok(true) => {}
                        Ok(false) => {}
                        Err(error) => {
                            rollback_failures.push(json!({"worker_id":id,"error":error}));
                            let _ = state.runs.request_worker_cancel(run_id, "worker", id);
                        }
                    }
                }
                handle.paused.store(true, Ordering::SeqCst);
                run["freeze_control"]["recovery_required"] = json!(true);
                return Ok(
                    json!({"state":"partial","kind":"lease_resume_failed_runtime_refrozen","error":error,"rollback_failures":rollback_failures}),
                );
            }
        };
        if prior["run_wide"] == true {
            run["budget_suspended_seconds"] = json!(suspended_seconds(run));
            run["budget_suspend_started_at"] = Value::Null;
            run["control_mode"] = json!("active");
            run["awaiting_operator"] = Value::Null;
            handle.paused.store(
                prior["was_paused"].as_bool().unwrap_or(false),
                Ordering::SeqCst,
            );
        }
        let duration = (now_secs() - prior["started_at"].as_f64().unwrap_or(now_secs())).max(0.);
        for row in prior["workers"].as_array().into_iter().flatten() {
            if let Some(id) = row["worker_id"].as_str() {
                let previous = run["worker_suspended_seconds"][id].as_f64().unwrap_or(0.);
                if !run["worker_suspended_seconds"].is_object() {
                    run["worker_suspended_seconds"] = json!({});
                }
                run["worker_suspended_seconds"][id] = json!(previous + duration);
            }
        }
        run["freeze_control"] = Value::Null;
        Ok(
            json!({"state":"effect_observed","kind":"runtime_thawed","targets":confirmed.iter().map(|row|&row.0).collect::<Vec<_>>(),"leases_restored":leases}),
        )
    }
}

pub(super) fn worker_budget(state: &AppState, run_id: &str, worker: &str) -> (f64, bool) {
    let run = state
        .store
        .get("runs", run_id)
        .ok()
        .flatten()
        .unwrap_or(Value::Null);
    let frozen = run["freeze_control"]["workers"]
        .as_array()
        .into_iter()
        .flatten()
        .any(|row| row["worker_id"] == worker);
    let freeze_total = run["worker_suspended_seconds"][worker]
        .as_f64()
        .unwrap_or(0.)
        + if frozen {
            (now_secs()
                - run["freeze_control"]["started_at"]
                    .as_f64()
                    .unwrap_or(now_secs()))
            .max(0.)
        } else {
            0.
        };
    let (guard_total, guard_paused) =
        super::execution_guard::worker_pause_budget(state, run_id, worker);
    (freeze_total + guard_total, frozen || guard_paused)
}

pub(super) fn settle_stopped(
    state: &AppState,
    run_id: &str,
    run: &mut Value,
) -> Result<(), String> {
    let prior = run["freeze_control"].clone();
    if !prior.is_object() {
        return Ok(());
    }
    let cleanup = if let Some(suspension) = prior["suspension_id"].as_str() {
        graph::SharedGraph::open(state, run_id)
            .and_then(|graph| graph.resume_control_leases(suspension).map(|_| ()))
    } else {
        Ok(())
    };
    if prior["run_wide"] == true {
        run["budget_suspended_seconds"] = json!(suspended_seconds(run));
    }
    run["budget_suspend_started_at"] = Value::Null;
    let duration = (now_secs() - prior["started_at"].as_f64().unwrap_or(now_secs())).max(0.);
    if !run["worker_suspended_seconds"].is_object() {
        run["worker_suspended_seconds"] = json!({});
    }
    for worker in prior["workers"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|row| row["worker_id"].as_str())
    {
        let previous = run["worker_suspended_seconds"][worker]
            .as_f64()
            .unwrap_or(0.);
        run["worker_suspended_seconds"][worker] = json!(previous + duration);
    }
    run["freeze_control"] = Value::Null;
    if let Err(error) = cleanup {
        run["stopped_lease_cleanup"] =
            json!({"suspension_id":prior["suspension_id"],"error":error,"status":"pending"});
        return Err(error);
    }
    run["stopped_lease_cleanup"] = Value::Null;
    Ok(())
}

pub(super) fn recover_stopped(state: &AppState, run_id: &str) -> Result<(), String> {
    let Some(before) = state.store.get("runs", run_id).map_err(|e| e.to_string())? else {
        return Ok(());
    };
    let pending = &before["stopped_lease_cleanup"];
    if pending["status"] != "pending" {
        return Ok(());
    }
    let suspension = pending["suspension_id"]
        .as_str()
        .ok_or("Stopped graph suspension identity is missing")?;
    graph::SharedGraph::open(state, run_id)?.resume_control_leases(suspension)?;
    let mut after = before.clone();
    after["stopped_lease_cleanup"] = Value::Null;
    if !state
        .store
        .put_many_if(
            &[("runs", run_id, Some(&before))],
            &[("runs", run_id, &after)],
        )
        .map_err(|e| e.to_string())?
    {
        return Err("Stopped graph cleanup record changed".into());
    }
    Ok(())
}
