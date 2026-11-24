//! Blackboard MCP shares the managed registry and durable external-call admission.
use super::*;
use std::path::Path;

pub(super) fn dispatch(state:&AppState,run_id:&str,worker:&str,request:&Value,request_path:&Path,result_path:&Path)->Result<(),String>{
    let runtime=tokio::runtime::Handle::try_current().map_err(|_|"Managed MCP executor unavailable")?;
    let run=state.store.get("runs",run_id).map_err(|e|e.to_string())?.ok_or("Run not found")?;
    let mode=run["mode"].as_str().unwrap_or("ctf").to_owned();
    let engine=state.runs.handle(state,run_id).workers.lock().unwrap().get(worker).map(|w|w.engine.clone()).ok_or("Worker runtime is not active")?;
    let registry=crate::plugins::registry(state).map_err(|e|e.to_string())?;
    let id=request["request_id"].as_str().ok_or("MCP request_id missing")?;
    let key=format!("{run_id}:{worker}:{id}");let epoch=state.chat.epoch();
    if let Some(old)=state.store.get("run_worker_mcp_requests",&key).map_err(|e|e.to_string())? {
        if old["request"]!=*request{return Err("MCP request_id was reused for different input".into());}
        if let Some(response)=old.get("response") {worker::write_atomic_json(result_path,response).map_err(|e|e.to_string())?;std::fs::remove_file(request_path).map_err(|e|e.to_string())?;return Ok(());}
        if old["epoch"]==epoch{return Ok(());}
        return Err("MCP outcome is unknown after service restart; reconcile the original operation before submitting another call".into());
    }
    let admission=json!({"request":request,"epoch":epoch,"state":"pending","run_id":run_id,"worker_id":worker,"created_at":now_secs()});
    if !state.store.put_many_if(&[("run_worker_mcp_requests",&key,None)],&[("run_worker_mcp_requests",&key,&admission)]).map_err(|e|e.to_string())?{return Ok(());}
    let store=state.store.clone();let request=request.clone();let request_path=request_path.to_owned();let result_path=result_path.to_owned();let scope=run_id.to_owned();let generation=run["execution_generation"].clone();
    runtime.spawn(async move {
        let id=request["request_id"].as_str().unwrap_or("");let operation=request["operation"].as_str().unwrap_or("");let payload=request.get("payload").filter(|v|v.is_object()).unwrap_or(&request);
        let work=async {
            let tools=registry.prepare_tools(&engine,&mode,&scope).await.map_err(|e|(e.code,e.message))?;
            if operation=="mcp_tools" {
                let catalog:Vec<Value>=tools.iter().map(|tool|json!({"name":tool["name"],"description":tool["description"],"package":tool["_package"],"server":tool["_server"]})).collect();
                let errors=registry.runtime_mcp_health(&engine,&mode,&scope).map_err(|e|(e.code,e.message))?;
                return Ok(json!({"tools":catalog,"connection_errors":errors.into_iter().filter(|row|row["status"]!="connected").collect::<Vec<_>>()}));
            }
            let name=payload["name"].as_str().filter(|v|!v.is_empty()).ok_or_else(||("worker_mcp.tool_unavailable".into(),"MCP tool name is required".into()))?;
            let tool=tools.iter().find(|tool|tool["name"]==name).ok_or_else(||("worker_mcp.tool_unavailable".into(),"MCP tool is not enabled for this Worker mode and engine".into()))?;
            if operation=="mcp_schema" {return Ok(tool["input_schema"].clone());}
            if !payload["arguments"].is_object(){return Err(("worker_mcp.invalid_arguments".into(),"MCP arguments must be a JSON object".into()));}
            let validator=jsonschema::draft202012::options().build(&tool["input_schema"]).map_err(|error|("worker_mcp.schema_invalid".into(),error.to_string()))?;
            validator.validate(&payload["arguments"]).map_err(|error|("worker_mcp.invalid_arguments".into(),error.to_string()))?;
            let current=store.get("runs",&scope).map_err(|e|("storage.unavailable".into(),e.to_string()))?.ok_or_else(||("run.not_found".into(),"Run was removed".into()))?;
            if current["execution_generation"]!=generation || current["running"]!=true{return Err(("run.execution_generation_stale".into(),"Worker execution is no longer active".into()));}
            registry.invoke(&engine,name,payload["arguments"].clone(),&mode,&scope).await.map_err(|e|(e.code,e.message))
        };
        let response=match tokio::time::timeout(std::time::Duration::from_secs(120),work).await {
            Ok(Ok(result))=>json!({"request_id":id,"ok":result["isError"]!=true,"code":if result["isError"]==true{"worker_mcp.tool_error"}else{""},"message":result.to_string()}),
            Ok(Err((code,detail)))=>json!({"request_id":id,"ok":false,"code":code,"detail":detail}),
            Err(_)=>json!({"request_id":id,"ok":false,"code":"worker_mcp.outcome_unknown","detail":"MCP request timed out; reconcile its external outcome before retrying"}),
        };
        let mut completed=admission;completed["response"]=response.clone();completed["state"]=json!("completed");
        if let Err(error)=store.put("run_worker_mcp_requests",&key,&completed){tracing::error!(%error,"MCP result journal failed");return;}
        if worker::write_atomic_json(&result_path,&response).is_ok(){let _=std::fs::remove_file(request_path);}
    });
    Ok(())
}
