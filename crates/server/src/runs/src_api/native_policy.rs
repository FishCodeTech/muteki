//! SRC adapter coverage and conversation coordinator/worker instruction injection.
//! Native CLI tool hooks and secondary native-approval interception are disabled so internal agent execution never blocks on redundant approval walls.
use super::*;
use crate::agent_runtime::{RuntimeError, Session, StartRequest};
use agent::{AgentEvent, ApprovalDecision};

pub(crate) fn coverage()->Value{
    let declaration=|gate:&str,completion:bool|json!({"trusted_input_fields":["tool_name","arguments","cwd","identity_ref"],"observed_resource_fields":["tool_name","arguments","resource_facts"],"pre_dispatch_gate":gate,"environment_boundary":"existing-runtime-and-container","network_scope":"task-environment","content_inspection":"task-environment","cancellation":"existing-adapter","receipt_support":if completion{"dispatch-and-completion"}else{"dispatch-only"}});
    let mut rows=serde_json::Map::new();
    for(name,gate,completion)in[("claude.agent_sdk","PreToolUse",true),("acp","limited: native permission requests, not every tool",true),("codex.app_server","limited: native approval requests only",false),("droid.rpc","limited: native PermissionRequest only",false),("opencode","limited: permission.asked only",false),("kimi","limited: native approval notification only",false),("devin.acp","limited: terminal/create and native permission requests",false),("cursor.sdk","unsupported: no interactive pre-dispatch hook",false),("cli","unsupported: no in-band pre-dispatch hook",false),("pi.rpc","unsupported: no in-band approval",false),("structured_http","managed pre-dispatch",true),("managed_browser","managed pre-dispatch",true),("local_mcp","managed Gateway",true)]{rows.insert(name.into(),declaration(gate,completion));}
    rows.get_mut("codex.app_server").unwrap()["receipt_support"]=json!("dispatch-and-correlated-completion");
    Value::Object(rows)
}
fn api_error(error:Error)->RuntimeError{RuntimeError::new(error.code,error.message)}
pub(crate) fn assert_access(s:&AppState,tid:&str,_adapter:&str,_mode:&str)->Result<bool,RuntimeError>{
    if execution_runtime::task_for_thread(s,tid).map_err(api_error)?.is_none(){return Ok(false)}
    crate::business::require(s,crate::business::Module::Src).map_err(|error|RuntimeError::retryable(error.code,error.message))?;
    Ok(true)
}
pub(crate) fn observe(s:&AppState,tid:&str,doc:&mut Value,_session:&Session,event:&AgentEvent)->Api<()>{
    if matches!(event, AgentEvent::SessionClosed{..})
        && let Ok(Some(id)) = execution_runtime::task_for_thread(s, tid)
    {
        close_dispatched_hook_receipts(s, &id, tid)?;
    }
    let _ = doc;
    Ok(())
}
pub(super) fn close_dispatched_hook_receipts(s:&AppState,id:&str,tid:&str)->Api<()>{
    for row in records(s,id,"execution_request")?{
        let request=&row["payload"];
        if request["thread_id"]!=tid{continue;}
        if request["adapter_id"].as_str().is_none_or(|value|value.is_empty()){continue;}
        let Some(invocation)=request["invocation_id"].as_str().filter(|value|!value.is_empty()) else {continue;};
        if execution_runtime::receipt(s,id,invocation)?["dispatch_state"]!="dispatched"{continue;}
        execution_runtime::settle(s,id,invocation,Value::Null,Some("native_session_closed"),true)?;
    }
    Ok(())
}

pub(crate) async fn install(s:&Arc<AppState>,tid:&str,request:&mut StartRequest)->Result<(),RuntimeError>{
    if !assert_access(s,tid,&request.adapter_id,&request.access_mode)?{return Ok(())}
    let extra = domains::src_thread_instructions(s, tid);
    if !extra.is_empty() {
        request.instructions = Some(match request.instructions.take() {
            Some(existing) if !existing.trim().is_empty() => format!("{existing}\n\n{extra}"),
            _ => extra,
        });
    }
    request.env.retain(|(key,_)|!matches!(key.as_str(),"MUTEKI_SRC_HOOK_URL"|"MUTEKI_SRC_HOOK_TOKEN"|"MUTEKI_SRC_HOOK_SETTINGS"));
    Ok(())
}
pub(super) fn recover(s:&AppState,id:&str)->Api<()>{
    let current_epoch=s.chat.epoch();
    for row in records(s,id,"execution_request")?{
        let request=&row["payload"];
        if request["adapter_id"].is_string(){
            let invocation=require(request,"invocation_id")?;
            let r=execution_runtime::receipt(s,id,invocation)?;
            let state=r["dispatch_state"].as_str().unwrap_or("");
            let rec_epoch=r["control_epoch"].as_str().unwrap_or("");
            let prior_epoch=!rec_epoch.is_empty()&&rec_epoch!=current_epoch;
            if state=="dispatched"{
                execution_runtime::settle_from_recovery(s,id,invocation,Value::Null,Some("service_restarted"),true)?;
            }else if state=="ready"&&prior_epoch{
                execution_runtime::settle_from_recovery(s,id,invocation,Value::Null,Some("dispatch_interrupted"),false)?;
            }else if state=="waiting_approval"&&prior_epoch{
                execution_runtime::settle_from_recovery(s,id,invocation,Value::Null,Some("approval_expired"),false)?;
            }
            if (state=="dispatched"||prior_epoch)
                && let Some(row)=execution_runtime::approval(s,id,invocation)?
                && row["payload"]["status"]=="pending"
            {
                let mut approval=row["payload"].clone();
                approval["status"]=json!("superseded");
                save_rec(s,id,"execution_approval",require(&approval,"approval_id")?,approval.clone(),row["revision"].as_u64())?;
            }
        }
    }
    Ok(())
}

pub(crate) fn prepare_delivery(_s:&AppState,_tid:&str,_request_id:&str,_choice:&ApprovalDecision)->Result<(),RuntimeError>{
    Ok(())
}

pub(crate) fn delivery_failed(_s:&AppState,_tid:&str,_request_id:&str)->Api<()>{
    Ok(())
}


/// SRC supplies the conversation policy through the shared engine contract.
pub struct ConversationPolicy;
fn policy_error(error:Error)->crate::conversation_policy::Error {
    crate::conversation_policy::Error{status:error.status,code:error.code,message:error.message}
}
impl crate::conversation_policy::Policy for ConversationPolicy {
    fn binding(&self,s:&AppState,tid:&str)->Result<Option<String>,RuntimeError>{
        execution_runtime::task_for_thread(s,tid).map_err(api_error)
    }
    fn assert_access(&self,s:&AppState,tid:&str,adapter:&str,mode:&str)->Result<bool,RuntimeError>{assert_access(s,tid,adapter,mode)}
    fn install<'a>(&self,s:&'a Arc<AppState>,tid:&'a str,request:&'a mut StartRequest)->futures_util::future::BoxFuture<'a,Result<(),RuntimeError>>{Box::pin(install(s,tid,request))}
    fn observe(&self,s:&AppState,tid:&str,doc:&mut Value,session:&Session,event:&AgentEvent)->Result<(),crate::conversation_policy::Error>{observe(s,tid,doc,session,event).map_err(policy_error)}
    fn decide(&self,_s:&AppState,_doc:&Value,_row:&Value,_choice:&ApprovalDecision)->Result<(),crate::conversation_policy::Error>{
        Ok(())
    }
    fn prepare_delivery(&self,s:&AppState,tid:&str,request:&str,choice:&ApprovalDecision)->Result<(),RuntimeError>{prepare_delivery(s,tid,request,choice)}
    fn delivery_failed(&self,s:&AppState,tid:&str,request:&str)->Result<(),crate::conversation_policy::Error>{delivery_failed(s,tid,request).map_err(policy_error)}
    fn managed_approval(&self,_doc:&Value,_payload:&Value)->bool{false}
    fn decide_managed(&self,s:&AppState,tid:&str,doc:&mut Value,payload:&Value)->Result<Value,crate::conversation_policy::Error>{
        crate::business::require(s,crate::business::Module::Src).map_err(|error|crate::conversation_policy::Error{status:StatusCode::SERVICE_UNAVAILABLE,code:error.code,message:error.message})?;
        execution_runtime::decide_from_chat(s,tid,doc,payload).map_err(policy_error)
    }
}
