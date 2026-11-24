//! Re-observe an admitted stop and settle its existing receipt under the original generation fence.
use super::*;

pub(super) fn pending(state: &AppState, run: &Value) -> Result<bool, String> {
    if run["status"] == "stopping" {
        return Ok(true);
    }
    let control = run["control_generation"].as_u64().unwrap_or(0);
    Ok(state.store.list("runner_stop_intents").map_err(|e| e.to_string())?.iter().any(|intent| {
        intent["state"] == "pending" && intent["run_id"] == run["run_id"]
            && intent["generation"] == run["execution_generation"]
            && intent["control_generation"].as_u64().is_some_and(|generation| {
                generation == control || control.checked_add(1) == Some(generation)
            })
    }))
}

async fn local_stop_confirmed(state: &AppState, run: &str, execution: u64) -> Result<bool, String> {
    let socket = crate::runner::configured_socket();
    let runtime = if let Some(socket) = &socket {
        crate::runner::run_status_if_recorded(socket, run).await?
            .and_then(|mut response| response.get_mut("run").map(Value::take))
    } else {
        state.store.get("runner_runs", run).map_err(|e| e.to_string())?
    };
    let Some(runtime) = runtime else { return Ok(false); };
    let Some(generation) = runtime["generation"].as_u64().filter(|&generation| generation <= execution) else {
        return Ok(false);
    };

    if generation < execution {
        let newer_commands = state.store.list("runner_commands").map_err(|e| e.to_string())?.iter().any(|row| {
            row["run_id"] == run && row["generation"].as_u64().is_some_and(|value| value > generation)
                && !crate::runner_execution::facts(row).stopped
        });
        let other_scopes = state.store.list("runner_runs").map_err(|e| e.to_string())?.iter().any(|row| {
            row["owner_run_id"] == run && row["run_id"] != run && row["state"] != "stopped"
        });
        let remote_scopes = state.store.list("runner_controller_runs").map_err(|e| e.to_string())?.iter().any(|row| {
            row["run_id"] == run && (row["state"] != "stopped" || row["publication_pending"] == true)
        });
        if newer_commands || other_scopes || remote_scopes {
            return Ok(false);
        }
    }
    if runtime["state"] == "stopped" {
        return Ok(true);
    }
    if let Some(socket) = socket
        && matches!(runtime["state"].as_str(), Some("active" | "stopping"))
    {
        let receipt = crate::runner::exchange(&socket, &format!("/v1/containers/{run}/teardown"),
            Some(json!({"generation": generation}))).await?;
        return Ok(receipt["exit_confirmed"] == true);
    }
    Ok(false)
}

pub(super) async fn confirm_queued_stop(
    state: &AppState, handle: &RunHandle, run: &str, execution: u64,
) -> Result<bool, String> {
    let mut tasks = handle.worker_tasks.lock().unwrap().clone();
    tasks.extend(handle.coordinator_tasks.lock().unwrap().iter().cloned());
    if !wait_for_tasks(&tasks, main_runtime_cancel_timeout()).await
    {
        return Ok(false);
    }
    if let Some(socket) = crate::runner::configured_socket()
        && crate::runner::run_status_if_recorded(&socket, run).await?.is_some()
    {
        return Ok(false);
    }
    let rows = |collection| state.store.list(collection).map_err(|error| error.to_string());
    if rows("runner_runs")?.iter().any(|row| {
        (row["run_id"] == run || row["owner_run_id"] == run) && row["state"] != "stopped"
    }) || rows("runner_controller_runs")?.iter().any(|row| {
        row["run_id"] == run && (row["state"] != "stopped" || row["publication_pending"] == true)
    }) || rows("runner_commands")?.iter().any(|row| {
        row["run_id"] == run && !crate::runner_execution::facts(row).stopped
    }) || rows("run_execution_commands")?.iter().any(|row| {
        row["run_id"] == run && row["generation"] == execution && row["action"] == "external_start_requested"
    }) {
        return Ok(false);
    }
    crate::runner_resources::release_unadmitted_run(state, run, execution)
}

/// A stop confirmed by its own request needs no recovery, so it no longer fences new executions.
pub(super) fn release_applied(state: &AppState, conn: &Connection, command: &str) -> Result<(), String> {
    let Some(mut intent) = state.store.get("runner_stop_intents", command).map_err(|e| e.to_string())? else {
        return Ok(());
    };
    if intent["state"] != "pending" {
        return Ok(());
    }
    let status: String = conn.query_row("SELECT status FROM control_commands WHERE command_id=?1", [command], |row| row.get(0))
        .map_err(|e| e.to_string())?;
    if matches!(status.as_str(), "unknown" | "partial" | "projection_conflict") {
        return Ok(());
    }
    intent["state"] = json!("settled");
    state.store.put("runner_stop_intents", command, &intent).map_err(|e| e.to_string())
}

pub(crate) async fn settle(state:&AppState)->Result<(),String>{
    for intent in state.store.list("runner_stop_intents").map_err(|e|e.to_string())?.into_iter().filter(|row|row["state"]=="pending"){
        if let Err(error)=settle_one(state,intent).await{tracing::warn!(%error,"One stop confirmation remains pending");}
    }Ok(())
}
pub(crate) async fn refresh(state:&AppState,run:&str,command:&str)->Result<(),String>{
    if let Some(intent)=state.store.get("runner_stop_intents",command).map_err(|e|e.to_string())?&&intent["run_id"]==run&&intent["state"]=="pending"{settle_one(state,intent).await?;}Ok(())
}
async fn settle_one(state:&AppState,mut intent:Value)->Result<(),String>{
        let run=intent["run_id"].as_str().ok_or("Stop Run missing")?.to_owned();
        let command=intent["command_id"].as_str().ok_or("Stop command missing")?.to_owned();
        let handle=state.runs.handle(state,&run);let _guard=handle.delete_lock.lock().await;
        if !state.runs.claim_execution_owner(state,&run)?{return Ok(());}
        if state.store.get("runs",&run).map_err(|e|e.to_string())?.is_none(){return Ok(());}
        let conn=open_control_db(state,&run)?;
        control_journal::recover(state,&run,&conn)?;
        let before=state.store.get("runs",&run).map_err(|e|e.to_string())?.ok_or("Run disappeared during stop recovery")?;
        let (status,generation):(String,u64)=conn.query_row("SELECT status,generation FROM control_commands WHERE command_id=?1",[&command],|row|Ok((row.get(0)?,row.get(1)?))).map_err(|e|e.to_string())?;
        if !matches!(status.as_str(),"unknown"|"partial"|"projection_conflict"){
            intent["state"]=json!("settled");state.store.put("runner_stop_intents",&command,&intent).map_err(|e|e.to_string())?;return Ok(());
        }
        let control=before["control_generation"].as_u64().unwrap_or(0);
        if before["execution_generation"]!=intent["generation"]||generation!=intent["control_generation"].as_u64().unwrap_or(0)||!(control==generation||control.checked_add(1)==Some(generation)){
            intent["state"]=json!("superseded");state.store.put("runner_stop_intents",&command,&intent).map_err(|e|e.to_string())?;return Ok(());
        }
        let execution=intent["generation"].as_u64().ok_or("Stop execution generation missing")?;
        let remote=state.store.get("runner_controller_runs",&format!("{run}:{execution}")).map_err(|e|e.to_string())?;
        let published=if let Some(binding)=remote{
            if binding["state"]!="stopped"||binding["publication_pending"]==true{return Ok(());}
            true
        }else if local_stop_confirmed(state, &run, execution).await? {
            true
        }else {
            if !confirm_queued_stop(state, &handle, &run, execution).await? { return Ok(()); }
            false
        };
        if handle.worker_tasks.lock().unwrap().iter().any(|task|!task.is_finished())||handle.coordinator_tasks.lock().unwrap().iter().any(|task|!task.is_finished()){return Ok(());}
        worker_result::settle_claims(state,&run,"canceled","operator_stop")?;
        worker_result::recover_projections(state,&run)?;
        let mut after=before.clone();
        let solved=intent["action"]=="complete"||before["solved"]==true;
        let reason=if intent["action"]=="complete"{"goal_met"}else{"operator_stop"};
        control_freeze::settle_stopped(state,&run,&mut after)?;
        terminal::finish(state,&run,intent["generation"].as_u64().ok_or("Stop execution generation missing")?,None,solved,reason,intent["text"].as_str())?;
        let terminal=state.store.get("runs",&run).map_err(|e|e.to_string())?.ok_or("Stopped Run missing")?;
        for key in ["status","running","solved","terminal_at","terminal_generation","terminal_sequence","terminal_receipt_id","outcome_reason","outcome_detail","awaiting_operator","updated"]{after[key]=terminal[key].clone();}
        after["control_generation"]=json!(generation);after["control_mode"]=json!("terminated");
        let response=json!({"ok":true,"command_id":command,"status":"applied","generation":generation,"process_exit_confirmed":true,"effect":{"kind":"stop_reconciled","artifacts_published":published}});
        control_journal::confirm_pending_stop(state,&run,&conn,&command,&before,&after,&response)?;
        intent["state"]=json!("settled");intent["confirmed_at"]=json!(now_secs());state.store.put("runner_stop_intents",&command,&intent).map_err(|e|e.to_string())?;
        emit_event(state,&run,None,"control.command",json!({"command_id":command,"action":intent["action"],"status":"applied","generation":generation,"process_exit_confirmed":true,"effect":{"kind":"stop_reconciled","artifacts_published":published}}));

    Ok(())
}
