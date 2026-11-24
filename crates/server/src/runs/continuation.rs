//! Host-owned native history references for completed Run followups.
use super::*;
use std::path::Path;
pub(super) fn save(state:&AppState,run:&str,spec:&worker::WorkerSpec,cwd:&Path,resume:&Value,session:Option<&str>,established:bool)->Result<(),String>{
    let root=session_dir(state,run).canonicalize().map_err(|error|error.to_string())?;let cwd=cwd.canonicalize().map_err(|error|error.to_string())?;
    if !cwd.starts_with(&root){return Err("Worker continuation cwd escaped its Run".into())}
    let key=json!([run,spec.solver_id]).to_string();let old=state.store.get("run_worker_continuations",&key).map_err(|error|error.to_string())?;
    let established=established||old.as_ref().is_some_and(|row|row["resume"]==*resume&&row["session_id"].as_str()==session&&row["established"]==true);
    let record=json!({"run_id":run,"worker_id":spec.solver_id,"spec":spec,"cwd_relative":cwd.strip_prefix(&root).map_err(|error|error.to_string())?,"resume":resume,"session_id":session,"established":established});
    if old.as_ref()!=Some(&record){if !state.store.put_many_if(&[("run_worker_continuations",&key,old.as_ref())],&[("run_worker_continuations",&key,&record)]).map_err(|error|error.to_string())?{return Err("Worker continuation changed concurrently".into())}}Ok(())
}
pub(super) fn load(state:&AppState,run:&str,worker_id:&str)->Result<Option<Value>,String>{
    let key=json!([run,worker_id]).to_string();
    state.store.get("run_worker_continuations",&key).map_err(|error|error.to_string())
}
pub(super) fn save_local_pi(state:&AppState,run:&str,worker_id:&str,cwd:&Path,resume:&Value,session:Option<&str>,execution_generation:u64)->Result<(),String>{
    let root=session_dir(state,run).canonicalize().map_err(|error|error.to_string())?;
    let cwd=cwd.canonicalize().map_err(|error|error.to_string())?;
    if !cwd.starts_with(&root){return Err("Worker continuation cwd escaped its Run".into())}
    let key=json!([run,worker_id]).to_string();
    let old=state.store.get("run_worker_continuations",&key).map_err(|error|error.to_string())?;
    let record=json!({
        "run_id":run,
        "worker_id":worker_id,
        "driver_kind":"local_pi",
        "adapter_id":"pi.rpc",
        "cwd_relative":cwd.strip_prefix(&root).map_err(|error|error.to_string())?,
        "resume":resume,
        "session_id":session,
        "provider_session_id":session,
        "execution_generation":execution_generation,
        "established":true
    });
    if old.as_ref()!=Some(&record){
        if !state.store.put_many_if(&[("run_worker_continuations",&key,old.as_ref())],&[("run_worker_continuations",&key,&record)]).map_err(|error|error.to_string())?{
            return Err("Worker continuation changed concurrently".into())
        }
    }
    Ok(())
}
pub(super) fn winner(state:&AppState,run:&str)->Result<Option<Value>,String>{state.store.get("run_winner_continuations",run).map_err(|error|error.to_string())}
