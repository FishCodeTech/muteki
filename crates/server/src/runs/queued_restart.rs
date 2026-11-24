//! Resume only a saved plan that never crossed external admission: a coordinator plan starts
//! its coordinator again, which reads the Run graph for what is already proven.
//! Running/unknown effects require explicit reconciliation, never automatic replay.
use super::*;

/// Interrupted dispatches not yet taken over by a later reservation (see `restored_from`).
fn unrestored(dispatches:&[Value])->impl Iterator<Item=&Value>{
    dispatches.iter().filter(|row|row["reason"]=="web_restarted_before_confirmed_dispatch"&&row["status"]=="interrupted"
        &&!dispatches.iter().any(|later|later["restored_from"]==row["id"]))
}

pub(super) fn pending(state:&AppState,run:&str)->Result<bool,String>{
    Ok(unrestored(&state.store.list("runner_dispatches").map_err(|e|e.to_string())?).any(|row|row["run_id"]==run))
}

pub(super) fn spawn(state:Arc<AppState>){
    tokio::spawn(async move{
        loop{
            if !state.admission.draining(){if let Err(error)=recover(&state).await{tracing::warn!(%error,"Queued execution recovery is pending");}}
            tokio::time::sleep(Duration::from_secs(5)).await;
        }
    });
}
async fn recover(state:&Arc<AppState>)->Result<(),String>{
    let dispatches=state.store.list("runner_dispatches").map_err(|e|e.to_string())?;
    let runs:HashSet<String>=unrestored(&dispatches).filter_map(|row|row["run_id"].as_str().map(str::to_owned)).collect();
    for run_id in runs{
        let handle=state.runs.handle(state,&run_id);
        let _guard=handle.delete_lock.lock().await;
        if handle.deleting.load(Ordering::SeqCst)||state.runs.active_worker_count(&run_id)>0{continue;}
        let Some(run)=state.store.get("runs",&run_id).map_err(|e|e.to_string())?else{continue};
        if !crate::business::run_available(state,&run){continue;}
        if run["running"]!=true||matches!(run["control_mode"].as_str(),Some("frozen"|"quiesced"))
            || runner_stop_recovery::pending(state, &run)? { continue; }
        let generation=run["execution_generation"].as_u64().unwrap_or(0);
        let markers=state.store.list("run_execution_commands").map_err(|e|e.to_string())?;
        let Some(marker)=markers.iter().find(|row|row["run_id"]==run_id&&row["generation"]==generation&&matches!(row["action"].as_str(),Some("start"|"resolve")))else{continue};
        let command=marker["command_id"].as_str().ok_or("Saved queue command ID missing")?;
        let key=format!("{run_id}:{generation}");
        if state.store.get("runner_queue_recoveries",&key).map_err(|e|e.to_string())?.is_some_and(|row|row["host_epoch"]==state.chat.epoch()){continue;}
        let external=markers.iter().any(|row|row["run_id"]==run_id&&row["generation"]==generation&&row["action"]=="external_start_requested");
        let pending=state.store.list("runner_commands").map_err(|e|e.to_string())?.iter().any(|row|row["run_id"]==run_id&&row["generation"]==generation);
        let reason=if external||pending{Some("prior_external_execution_requires_reconciliation")}
            else if marker["control_generation"]!=run["control_generation"]{Some("authorization_or_control_generation_changed")}
            else{None};
        let mut recovery=json!({"run_id":run_id,"generation":generation,"command_id":command,"host_epoch":state.chat.epoch(),"state":"recovering","started_at":now_secs()});
        if let Some(reason)=reason{
            if external&&!pending&&super::launch_config::settings(state,&run_id).is_ok_and(|settings|crate::runner_remote::controller::selected(&settings).is_none()) {
                if let Some(socket)=crate::runner::configured_socket(){
                    match crate::runner::exchange(&socket,&format!("/v1/containers/{run_id}/teardown"),Some(json!({"generation":generation}))).await {
                        Ok(value) if value["exit_confirmed"]==true => {
                            recovery["state"]=json!("interrupted_exit_confirmed");
                            recovery["reason"]=json!("web_restarted_before_confirmed_start");
                            state.store.put("runner_queue_recoveries",&key,&recovery).map_err(|e|e.to_string())?;
                            terminal::finish(state,&run_id,generation,None,false,"interrupted_by_web_restart",Some("Start was interrupted before confirmation; the prior execution was stopped. Resolve to continue."))?;
                            emit_event(state,&run_id,None,"run.queue_recovered",recovery);
                            continue;
                        }
                        Err(error)=>recovery["stop_error"]=json!(error),
                        _=>{}
                    }
                }
            }
            recovery["state"]=json!("resume_required");recovery["reason"]=json!(reason);
            state.store.put("runner_queue_recoveries",&key,&recovery).map_err(|e|e.to_string())?;
            emit_event(state,&run_id,None,"run.queue_recovered",recovery);
            continue;
        }
        if run["src_task_id"].is_string(){if let Err(error)=src_api::bound_contract(state,&run_id,&marker["challenge"]){recovery["state"]=json!("authorization_required");recovery["reason"]=json!(error);state.store.put("runner_queue_recoveries",&key,&recovery).map_err(|e|e.to_string())?;continue;}}
        state.store.put("runner_queue_recoveries",&key,&recovery).map_err(|e|e.to_string())?;
        let response=resume_execution_command(state,&run_id,command,marker.clone());
        recovery["state"]=json!(if response.status().is_success(){"resumed"}else{"resume_required"});
        recovery["http_status"]=json!(response.status().as_u16());
        state.store.put("runner_queue_recoveries",&key,&recovery).map_err(|e|e.to_string())?;
        emit_event(state,&run_id,None,"run.queue_recovered",recovery);
    }
    Ok(())
}
