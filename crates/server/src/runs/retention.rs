//! Opt-in per-mode retention follows the last persisted event clock.
use super::*;

pub async fn sweep(state:&Arc<AppState>)->Result<Value,String>{
    if state.admission.draining(){return Ok(json!({"archived":[],"deleted":[],"paused":true}));}
    let saved=state.store.get("settings","run_retention").map_err(|e|e.to_string())?.unwrap_or_else(||json!({}));
    if !saved.is_object(){return Err("Retention policy store is invalid".into());}
    let mut archived=Vec::new();let mut deleted=Vec::new();let at=now_secs();
    for run in state.store.list("runs").map_err(|e|e.to_string())? {
        let Some(id)=run["run_id"].as_str()else{continue};if run["pinned"]==true || (run["execution_generation"].as_u64().unwrap_or(0)==0 && run["started"]!=true){continue;}
        let mode=run["mode"].as_str().unwrap_or("ctf");if !matches!(mode,"ctf"|"pentest"){continue;}
        let policy=saved.get(mode).cloned().unwrap_or_else(||default_retention_policy(mode));
        for key in ["archive_enabled","delete_enabled"]{if !policy[key].is_boolean(){return Err(format!("Retention {mode}.{key} is invalid"));}}
        let archive_days=policy["archive_after_days"].as_u64().filter(|n|(1..=3650).contains(n)).ok_or("Invalid archive retention interval")?;
        let delete_days=policy["delete_after_days"].as_u64().filter(|n|(1..=3650).contains(n)).ok_or("Invalid delete retention interval")?;
        if policy["archive_enabled"]==true&&policy["delete_enabled"]==true&&delete_days<=archive_days{return Err("Delete retention must follow archive retention".into());}
        let activity=last_persisted_activity(state,id).await?;
        if activity<=0.||activity>at{continue;}
        let idle=at-activity;
        if run["archived"]==true {
            if policy["delete_enabled"]!=true||idle<=delete_days as f64*86400.{continue;}
            let response=delete_run_inner(state.clone(),id.to_owned(),Some((run.clone(),activity))).await;
            let status=response.status();let body=axum::body::to_bytes(response.into_body(),1024*1024).await.map_err(|e|e.to_string())?;
            let body:Value=serde_json::from_slice(&body).map_err(|e|e.to_string())?;
            if status.is_success()&&body["ok"]==true{deleted.push(id.to_owned());}else{tracing::warn!(run_id=id,response=%body,"Retention deletion did not complete");}
        }else if policy["archive_enabled"]==true&&idle>archive_days as f64*86400. {
            let handle=state.runs.handle(state,id);let _guard=handle.delete_lock.lock().await;
            if state.store.get("runs",id).map_err(|e|e.to_string())?.as_ref()!=Some(&run){continue;}
            let last=last_persisted_activity(state,id).await?;if last!=activity{continue;}
            let mut next=run.clone();next["archived"]=json!(true);next["archived_at"]=json!(at);next["pinned"]=json!(false);next["pinned_at"]=Value::Null;
            if state.store.put_many_if(&[("runs",id,Some(&run))],&[("runs",id,&next)]).map_err(|e|e.to_string())?{archived.push(id.to_owned());}
        }
    }Ok(json!({"archived":archived,"deleted":deleted}))
}
pub fn spawn_loop(state:Arc<AppState>){
    if std::env::var("MUTEKI_RETENTION_ENABLED").is_ok_and(|v|matches!(v.to_ascii_lowercase().as_str(),"0"|"false"|"no"|"off"|"")){return;}
    let seconds=std::env::var("MUTEKI_RETENTION_INTERVAL").ok().and_then(|v|v.parse::<f64>().ok()).filter(|v|v.is_finite()&&*v>0.).unwrap_or(3600.);
    tokio::spawn(async move{loop{tokio::time::sleep(Duration::from_secs_f64(seconds)).await;
        match sweep(&state).await{Ok(result)=>{if result["archived"].as_array().is_some_and(|v|!v.is_empty())||result["deleted"].as_array().is_some_and(|v|!v.is_empty()){tracing::info!(%result,"Run retention applied");}},Err(error)=>tracing::error!(%error,"Run retention sweep failed")}
    }});
}
