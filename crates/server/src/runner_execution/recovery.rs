//! Reconcile a lost Runner against immutable container identities. Never infer Worker success.
use super::Execution;
use futures_util::{stream, StreamExt};
use serde_json::{Value,json};
use std::{collections::HashMap,path::Path,time::Duration};

pub(super) async fn inspect(identity:&str,root:&Path)->Result<Value,String>{
    if identity.len()!=64||!identity.bytes().all(|b|b.is_ascii_hexdigit()){
        return Err("Container recovery requires its immutable 64-character identity".into());
    }
    let output=tokio::time::timeout(Duration::from_secs(10),crate::host_process::command("docker")
        .args(["inspect",identity]).kill_on_drop(true).output()).await.map_err(|_|"Container recovery inspect timed out".to_owned())?
        .map_err(|e|e.to_string())?;
    if !output.status.success(){
        if crate::runs::container_exec::docker_reports_absent(&output.stderr) {
            return Ok(json!({"present":false,"execution_stopped":true,"container_id":identity}));
        }
        return Err("Container recovery inspect failed".into());
    }
    let rows:Vec<Value>=serde_json::from_slice(&output.stdout).map_err(|e|e.to_string())?;
    let row=rows.first().ok_or("Container recovery inspect was empty")?;
    if row["Config"]["Labels"]["muteki.state_dir"].as_str()!=root.to_str(){return Err("Container ownership does not match Runner state root".into());}
    let restart=row["HostConfig"]["RestartPolicy"]["Name"].as_str().unwrap_or("");
    let stopped=row["State"]["Running"]==false&&row["State"]["Restarting"]!=true
        &&row["State"]["Pid"].as_u64()==Some(0)
        &&matches!(row["State"]["Status"].as_str(),Some("exited"|"dead"))&&matches!(restart,""|"no");
    Ok(json!({"present":true,"container_id":row["Id"],"execution_stopped":stopped,
        "container_state":row["State"],"restart_policy":restart}))
}

pub(super) async fn reconcile(ex:&Execution)->Result<(),String>{
    let rows=ex.state.store.list("runner_runs").map_err(|e|e.to_string())?;
    let attached=ex.handles.lock().await;
    let pending:Vec<_>=rows.into_iter().filter(|row|row["state"]!="stopped"
        &&!row["run_id"].as_str().is_some_and(|id|attached.get(id).is_some_and(|handle|handle.link.is_alive()))).collect();
    drop(attached);
    let mut identities:Vec<String>=pending.iter().filter_map(|r|r["container_id"].as_str().map(str::to_owned)).collect();
    identities.sort();identities.dedup();
    let observations:HashMap<_,_>=stream::iter(identities).map(|id|async move {
        let observation=inspect(&id,&ex.root).await;(id,observation)
    }).buffer_unordered(4).collect().await;
    let affected:HashMap<String,Vec<Value>>=pending.iter().filter_map(|row|row["container_id"].as_str().map(|identity|(identity.to_owned(),json!({"run_id":row["run_id"],"generation":row["generation"]})))).fold(HashMap::new(),|mut groups,(identity,scope)|{groups.entry(identity).or_default().push(scope);groups});
    for mut row in pending {
        let id=row["run_id"].as_str().ok_or("Run recovery record lacks ID")?.to_owned();
        let gate=ex.gate(&id).await;let _guard=gate.lock().await;
        if ex.handles.lock().await.get(&id).is_some_and(|handle|handle.link.is_alive()){continue;}
        let Some(current)=ex.state.store.get("runner_runs",&id).map_err(|e|e.to_string())?else{continue};
        if current["generation"]!=row["generation"]||current["container_id"]!=row["container_id"]||current["state"]=="stopped"{continue;}
        row=current;
        let observation=row["container_id"].as_str().and_then(|id|observations.get(id));
        let confirmed=observation.is_some_and(|result|result.as_ref().is_ok_and(|v|v["execution_stopped"]==true));
        row["state"]=json!(if confirmed{"stopped"}else{"unknown"});
        row["recovery_checked_at"]=json!(chrono::Utc::now().to_rfc3339());
        row["recovery"]=match observation{
            Some(Ok(value))=>value.clone(),Some(Err(error))=>json!({"error":error}),
            None=>json!({"error":"Creation outcome lacks immutable container identity; explicit reconciliation required"}),
        };
        if let Some(identity)=row["container_id"].as_str().map(str::to_owned){
            row["recovery"]["affected_executions"]=json!(affected.get(&identity).cloned().unwrap_or_default());
            row["recovery"]["cause"]=json!(if row["recovery"]["container_state"]["ExitCode"]==76{"scope_supervisor_failure"}else if row["recovery"]["container_state"]["OOMKilled"]==true{"container_oom"}else{"control_link_lost"});
            if confirmed {crate::runs::container_exec::forget_stopped_runtime(&identity).await;ex.handles.lock().await.remove(&id);}
        }
        ex.state.store.document_scope(||->Result<(),String>{
            ex.state.store.put("runner_runs",&id,&row).map_err(|e|e.to_string())?;
            if confirmed{
                if let Some(mut binding)=ex.state.store.get("runner_remote_bindings",&id).map_err(|e|e.to_string())?{
                    if binding["generation"]==row["generation"]{
                        binding["state"]=json!("stopped");binding["stop_result"]=json!({"exit_confirmed":true,"recovery":row["recovery"]});
                        ex.state.store.put("runner_remote_bindings",&id,&binding).map_err(|e|e.to_string())?;
                    }
                }
                for mut command in ex.state.store.list("runner_commands").map_err(|e|e.to_string())?{
                    if command["run_id"]!=id||command["generation"]!=row["generation"]||command["exit"].is_object(){continue;}
                    let command_id=command["id"].as_str().ok_or("Recovery command ID missing")?.to_owned();
                    command["state"]=json!("outcome_unknown");
                    command["execution_stopped"]=json!(true);
                    command["stop_evidence"]=row["recovery"].clone();
                    ex.state.store.put("runner_commands",&command_id,&command).map_err(|e|e.to_string())?;
                    crate::runner_resources::release_completed(&ex.state.store,&command_id)?;
                }
            }Ok(())
        }).map_err(|e|e.to_string())??;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn mutable_names_cannot_prove_execution_stopped() {
        let root=tempfile::tempdir().unwrap();
        for identity in ["muteki-worker-missing", "abc123", "../container"] {
            assert!(inspect(identity,root.path()).await.unwrap_err().contains("immutable"));
        }
    }
    #[tokio::test]
    #[ignore="requires Docker and MUTEKI_TEST_WORKER_IMAGE_ID"]
    async fn foreign_container_is_not_accepted_as_exit_evidence(){
        let root=tempfile::tempdir().unwrap();let ex=Execution::open(root.path()).unwrap();
        let name=format!("muteki-worker-recovery-{}",uuid::Uuid::new_v4().simple());
        struct Cleanup(String);
        impl Drop for Cleanup{fn drop(&mut self){let _=crate::host_process::std_command("docker").args(["rm","-f",&self.0]).output();}}
        let _cleanup=Cleanup(name.clone());
        let image=std::env::var("MUTEKI_TEST_WORKER_IMAGE_ID").unwrap();
        let output=crate::host_process::command("docker").args(["create","--name",&name,"--network","none",
            "--label","muteki.state_dir=/foreign-node","--entrypoint","/bin/true",&image]).output().await.unwrap();
        assert!(output.status.success());let identity=String::from_utf8(output.stdout).unwrap().trim().to_owned();
        ex.state.store.put("runner_runs","foreign-test",&json!({"run_id":"foreign-test","generation":1,"state":"active","container_id":identity})).unwrap();
        ex.state.store.put("runner_commands","foreign-command",&json!({"id":"foreign-command","run_id":"foreign-test","generation":1,"state":"dispatching"})).unwrap();
        ex.reconcile().await.unwrap();
        let row=ex.state.store.get("runner_runs","foreign-test").unwrap().unwrap();
        assert_eq!(row["state"],"unknown");
        assert!(row["recovery"]["error"].as_str().unwrap().contains("ownership"));
        let command=ex.state.store.get("runner_commands","foreign-command").unwrap().unwrap();
        assert!(command["execution_stopped"].is_null());assert!(command["exit"].is_null());
    }
}
