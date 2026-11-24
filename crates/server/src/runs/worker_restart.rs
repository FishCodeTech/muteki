//! A runner may bind a private restarter; ordinary Runs never imply one exists.
use super::*;
use std::{future::Future,pin::Pin,path::Path};
pub type TargetRestarter=Arc<dyn Fn(String,String,String,String)->Pin<Box<dyn Future<Output=Result<Value,String>>+Send>>+Send+Sync>;
pub(super) fn dispatch(state:&AppState,run_id:&str,worker:&str,request:&Value,request_path:&Path,result_path:&Path)->Result<(),String>{
    let payload=request.get("payload").unwrap_or(request);let evidence=payload["evidence"].as_str().filter(|v|!v.trim().is_empty() && !v.contains('\0')).ok_or("A nonempty fault evidence string is required")?;
    if payload["confirmed_fault"]!=true{return Err("confirmed_fault=true is required".into());}
    let restarter=state.runs.target_restarters.lock().unwrap().get(run_id).cloned().ok_or("No target restarter is bound to this Run")?;
    let runtime=tokio::runtime::Handle::try_current().map_err(|_|"Target restart executor unavailable")?;
    let id=request["request_id"].as_str().filter(|v|!v.is_empty()).ok_or("Restart request_id missing")?;let key=format!("{run_id}:{worker}:{id}");
    if let Some(record)=state.store.get("run_target_restarts",&key).map_err(|e|e.to_string())?{
        if record["request"]!=*request{return Err("Restart request_id was reused with different input".into());}
        if let Some(response)=record.get("response"){worker::write_atomic_json(result_path,response).map_err(|e|e.to_string())?;std::fs::remove_file(request_path).map_err(|e|e.to_string())?;return Ok(());}
        if record["epoch"]==state.chat.epoch(){return Ok(());}
        return Err("Target restart outcome is unknown after service restart; reconcile its original request".into());
    }
    let record=json!({"request":request,"epoch":state.chat.epoch(),"state":"pending"});
    if !state.store.put_many_if(&[("run_target_restarts",&key,None)],&[("run_target_restarts",&key,&record)]).map_err(|e|e.to_string())?{return Ok(());}
    let(store,run_id,worker,id,evidence,request_path,result_path)=(state.store.clone(),run_id.to_owned(),worker.to_owned(),id.to_owned(),evidence.to_owned(),request_path.to_owned(),result_path.to_owned());
    runtime.spawn(async move {
        let response=match tokio::time::timeout(Duration::from_secs(90),restarter(run_id,worker,evidence,id.clone())).await {
            Ok(Ok(result))=>json!({"request_id":id,"ok":true,"result":result,"message":"RESTARTED"}),
            Ok(Err(error))=>json!({"request_id":id,"ok":false,"code":"restart_failed","detail":error}),
            Err(_)=>json!({"request_id":id,"ok":false,"code":"restart_outcome_unknown","detail":"Target restarter timed out; reconcile its original request"}),
        };
        let mut completed=record;completed["response"]=response.clone();completed["state"]=json!("completed");
        if let Err(error)=store.put("run_target_restarts",&key,&completed){tracing::error!(%error,"Target restart result journal failed");return;}
        if worker::write_atomic_json(&result_path,&response).is_ok(){let _=std::fs::remove_file(request_path);}
    });Ok(())
}
