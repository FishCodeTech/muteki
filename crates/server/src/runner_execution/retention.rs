//! Node-wide bound on persisted Worker event logs, plus retention of logs whose Worker has exited.
use super::*;
use std::sync::atomic::{AtomicU64,Ordering};

const DEFAULT_NODE_BYTES: u64 = 4*1024*1024*1024;
const DEFAULT_RETENTION_SECS: u64 = 7*24*3600;
pub(super) fn gc_ticks()->u64{(env_u64("MUTEKI_RUNNER_LOG_GC_SECS",300)/2).max(1)}

fn env_u64(name:&str,default:u64)->u64{std::env::var(name).ok().and_then(|v|v.trim().parse::<u64>().ok()).filter(|v|*v>0).unwrap_or(default)}
pub(super) fn node_budget()->u64{env_u64("MUTEKI_RUNNER_NODE_LOG_BYTES",DEFAULT_NODE_BYTES)}
fn retention()->u64{env_u64("MUTEKI_RUNNER_LOG_RETENTION_SECS",DEFAULT_RETENTION_SECS)}

pub(super) fn measure(root:&std::path::Path)->u64{
    std::fs::read_dir(root.join("runner/events")).into_iter().flatten().flatten()
        .filter_map(|entry|entry.metadata().ok()).filter(|meta|meta.is_file()).map(|meta|meta.len()).sum()
}

/// Reserve room for one frame. A refusal stops the affected Worker through the existing evidence-failure path.
pub(super) fn reserve(total:&AtomicU64,bytes:u64)->Result<(),std::io::Error>{
    let before=total.fetch_add(bytes,Ordering::SeqCst);
    if before.saturating_add(bytes)>node_budget(){
        total.fetch_sub(bytes,Ordering::SeqCst);
        return Err(std::io::Error::other("Runner node event-log budget is exhausted; completed logs must be purged before more output is stored"));
    }
    Ok(())
}

/// Remove event logs of exited Workers: past retention always; oldest first when the node is above 90% of its budget,
/// or above 75% when a Worker was just refused room (`force`). Running Workers' logs are never touched.
pub(super) fn purge(ex:&Execution,force:bool)->Result<usize,String>{
    let budget=node_budget();let now=std::time::SystemTime::now();let keep=std::time::Duration::from_secs(retention());
    let mut settled=Vec::new();
    for row in ex.state.store.list("runner_commands").map_err(|e|e.to_string())?{
        if row["events_purged_at"].is_number()||!(row["exit"].is_object()||row["execution_stopped"]==true){continue;}
        let Some(worker)=row["result"]["worker_id"].as_str()else{continue};
        let Ok(path)=ex.events_path(worker)else{continue};
        let Ok(meta)=std::fs::metadata(&path)else{continue};
        settled.push((meta.modified().unwrap_or(now),meta.len(),path,row));
    }
    settled.sort_by_key(|entry|entry.0);
    let mut removed=0;
    for (modified,size,path,mut row) in settled{
        let expired=now.duration_since(modified).unwrap_or_default()>=keep;
        let total=ex.log_bytes.load(Ordering::SeqCst);let pressured=total>budget/4*3&&(force||total>budget/10*9);
        if !expired&&!pressured{continue;}
        match std::fs::remove_file(&path){
            Ok(())=>{ex.log_bytes.fetch_sub(size.min(ex.log_bytes.load(Ordering::SeqCst)),Ordering::SeqCst);}
            Err(error) if error.kind()==std::io::ErrorKind::NotFound=>{}
            Err(error)=>{tracing::warn!(%error,"Runner event log purge failed");continue;}
        }
        row["events_purged_at"]=json!(chrono::Utc::now().timestamp());
        row["events_purged_reason"]=json!(if expired{"retention"}else{"node_log_budget"});
        if let Some(id)=row["id"].as_str().map(str::to_owned){ex.state.store.put("runner_commands",&id,&row).map_err(|e|e.to_string())?;}
        removed+=1;
    }
    Ok(removed)
}
