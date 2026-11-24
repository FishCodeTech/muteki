//! Provider operations use the live actor and the same catalog revision as the UI.
use super::*;

pub(super) fn names(adapter:&str)->&'static [&'static str]{match adapter{
    "pi.rpc"=>&["compact","session","stats","context","autocompact","autoretry","tree"],
    "omp.rpc_v2"=>&["compact","session","stats","context","autocompact","autoretry"],
    "claude.sdk"=>&["context","mcp"],
    "codex.app_server"=>&["goal","mcp","status","usage","models","skills","hooks","plugins","apps"],
    "droid.rpc"=>&["compact"],_=>&[]}}

pub(super) fn invocation(doc:&Value,payload:&Value)->Result<Option<Value>,Failure>{
    let adapter=doc["runtime"]["adapter_id"].as_str().unwrap_or("");
    let text=payload["text"].as_str().unwrap_or("").trim();let explicit=&payload["runtime_capability"];
    let command=explicit["invocation"]["wire_text"].as_str().or_else(||explicit["name"].as_str()).unwrap_or(text).trim_start_matches(['/','$']);
    let(name,args)=command.split_once(char::is_whitespace).unwrap_or((command,""));
    let args=payload["runtime_command_arguments"].as_str().unwrap_or(args).trim();
    let operation=names(adapter).contains(&name);
    if !explicit.is_object() && !operation{return Ok(None);}
    let session=&doc["native_session"];
    let revision=session["capability_revision"].as_u64().unwrap_or(0);
    if explicit.is_object() {
        if explicit["invocable"]==false || explicit["stale"]==true || explicit["status"]=="unavailable"{return Err(code(StatusCode::CONFLICT,"runtime.capability.stale","所选能力已过期"));}
        if adapter=="opencode.server" && name=="compact" && args.is_empty() && text=="/compact" && pool::manual_compaction(adapter){
            if payload["attachments"].as_array().is_some_and(|rows|!rows.is_empty())||payload["capability_refs"].as_array().is_some_and(|rows|!rows.is_empty()){
                return Err(code(StatusCode::BAD_REQUEST,"runtime.operation.attachments_unsupported","原生压缩不接受附件和上下文引用"));
            }
            return Ok(None);
        }
        let dynamic=session["commands"].as_array().into_iter().flatten().any(|row|row["name"]==name);
        if !operation && !dynamic{return Err(code(StatusCode::BAD_REQUEST,"runtime.capability.unavailable","当前原生目录没有所选能力"));}
    }
    if operation {
        if session["capabilities"]["operations"].as_array().is_some_and(|rows|!rows.iter().any(|row|row==name)){return Err(code(StatusCode::BAD_REQUEST,"runtime.capability.unavailable","当前原生会话未提供此操作"));}
        if payload["attachments"].as_array().is_some_and(|rows|!rows.is_empty())||payload["capability_refs"].as_array().is_some_and(|rows|!rows.is_empty()){return Err(code(StatusCode::BAD_REQUEST,"runtime.operation.attachments_unsupported","原生操作不接受附件和上下文引用"));}
        Ok(Some(json!({"name":name,"arguments":args,"revision":if explicit.is_object(){json!(revision)}else{Value::Null},"operation":true})))
    }else{
        let row=session["commands"].as_array().into_iter().flatten().find(|row|row["name"]==name);
        let wire=if row.is_some_and(|row|row["kind"]=="skill"){format!("${name}")}else{format!("/{name}")};
        Ok(Some(json!({"name":name,"wire_text":wire,"arguments":args,"revision":revision,"operation":false})))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selected_opencode_compact_uses_manual_turn_lifecycle() {
        let doc=json!({"runtime":{"adapter_id":"opencode.server"},"native_session":{"open":true,"capability_revision":3,"commands":[]}});
        let selected=json!({"text":"/compact","runtime_capability":{"name":"compact","revision":3,"invocable":true,"invocation":{"wire_text":"/compact"}}});
        assert!(invocation(&doc,&selected).unwrap().is_none());
    }
}

pub(super) async fn dispatch(s:&Arc<AppState>,tid:&str,turn:&str,session:&Arc<Session>)->Result<bool,RuntimeError>{
    let operation={let _guard=s.chat.active.lock().await;let mut doc=chat::load(s,tid).map_err(unavailable)?;
        let Some(row)=doc["turns"].as_array().into_iter().flatten().find(|row|row["turn_id"]==turn)else{return Ok(false)};
        let invocation=row["runtime_invocation"].clone();if !invocation.is_object(){return Ok(false);}
        if invocation["operation"]!=true {
            if !doc["native_session"]["commands"].as_array().into_iter().flatten().any(|row|row["name"]==invocation["name"]){return Err(RuntimeError::new("runtime.capability.stale","原生会话在执行前已撤销此命令"));}return Ok(false);
        }
        if doc["native_session"]["capabilities"]["operations"].as_array().is_some_and(|rows|!rows.contains(&invocation["name"])){return Err(RuntimeError::new("runtime.capability.stale","原生会话在执行前已撤销此操作"));}
        let agent=doc["native_session"]["agent_session_id"].clone();if let Some(row)=turn_mut(&mut doc,turn){row["operation"]=json!("runtime_operation");row["operation_name"]=invocation["name"].clone();row["agent_session_id"]=agent;}
        chat::emit(s,tid,&mut doc,"core.runtime.operation_requested",json!({"turn_id":turn,"name":invocation["name"],"arguments":invocation["arguments"]}));chat::save(s,tid,&mut doc).map_err(unavailable)?;invocation};
    let result=session.send_confirmed(SessionCommand::SetOption{id:"muteki:operation".into(),value:json!({"request_id":turn,"name":operation["name"],"arguments":operation["arguments"]})},turn).await?;
    let next={let _guard=s.chat.active.lock().await;let mut doc=chat::load(s,tid).map_err(unavailable)?;
        if doc["thread"]["state"]["running_turn_id"]!=turn{return Ok(true);}
        if !doc["messages"].is_array(){doc["messages"]=json!([]);}
        if result["context"].is_object(){if !doc["context_window"].is_object(){doc["context_window"]=json!({});}for(from,to)in [("used","total"),("limit","limit")]{if result["context"][from].is_number(){doc["context_window"][to]=result["context"][from].clone();}}}
        if result["compaction"].is_object(){let mut compaction=result["compaction"].clone();compaction["status"]=json!("completed");compaction["confirmed"]=json!(true);compaction["turn_id"]=json!(turn);chat::emit(s,tid,&mut doc,"core.context.compaction",compaction);}
        let rendered=serde_json::to_string_pretty(&result).unwrap_or_else(|_|result.to_string());
        doc["messages"].as_array_mut().unwrap().push(json!({"message_id":format!("operation:{turn}"),"turn_id":turn,"role":"assistant","text":rendered,"created_at":now()}));
        chat::emit(s,tid,&mut doc,"core.message.completed",json!({"turn_id":turn,"message_id":format!("operation:{turn}"),"role":"assistant","text":rendered}));
        chat::emit(s,tid,&mut doc,"core.runtime.operation_completed",json!({"turn_id":turn,"name":operation["name"],"result":result}));
        let next=finish_turn(s,tid,&mut doc,session,"completed",None,None);chat::save(s,tid,&mut doc).map_err(unavailable)?;next};
    if let Some((turn,dispatch))=next{chat::start(s.clone(),tid.into(),turn,dispatch).await;}Ok(true)
}
