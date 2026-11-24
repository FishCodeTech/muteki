use super::*;
pub(super) async fn apply(state:&Arc<AppState>,run_id:&str,command_id:&str,body:&Value,actor:&str,run_doc:&mut Value)->Result<(bool,Value),String>{
    let handle=state.runs.handle(state,run_id);let graph=graph::SharedGraph::open(state,run_id)?;
    let requests=graph.open_decisions()?;
    let action=body["action"].as_str().unwrap_or("");let dismiss=matches!(action,"dismiss"|"dismiss_help");
    let request_id=body["request_id"].as_str().unwrap_or("");let scope=body["scope"].as_str().unwrap_or("global");
    let selected=requests.into_iter().filter(|row|if !request_id.is_empty(){row["request_id"]==request_id}else if dismiss{scope=="global"||scope.split_once(':').is_some_and(|(kind,value)|matches!(kind,"worker"|"solver")&&row["worker"]==value)}else{false}).collect::<Vec<_>>();
    if selected.is_empty(){return Ok((false,json!({"kind":"decision_not_found","request_id":request_id})));}
    let answer=body["text"].as_str().or_else(||body["hint"].as_str()).unwrap_or("").trim();
    if !dismiss&&answer.is_empty(){return Err("Decision answer requires text".into());}
    let mut continuation=None;let mut delivery="dismissed";
    if !dismiss {
        let selected=&selected[0];let worker=selected["worker"].as_str().unwrap_or("");let target=format!("worker:{worker}");
        let observed=if !answer.starts_with("secret://"){steer_live_worker(state,run_id,&target,answer).await}else{None};
        match observed {
            Some(true)=>delivery="delivered",
            Some(false)=>return Ok((false,json!({"kind":"decision_delivery_unknown","request_id":request_id}))),
            None=>{
                let intent=format!("I-control-{command_id}");let scope=format!("intent:{intent}");
                let conn=open_control_db(state,run_id)?;
                control_context::create_guidance(&conn,run_id,command_id,&target,&scope,"hint",body)?;
                continuation=Some(intent);delivery="queued";
            }
        }
    }
    let result=graph.settle_decisions(command_id,actor,&selected,if dismiss{body["reason"].as_str().unwrap_or("operator dismissed")}else{answer},dismiss,continuation.as_deref())?;
    let ids=selected.iter().filter_map(|row|row["request_id"].as_str()).collect::<Vec<_>>();
    handle.hitl_requests.lock().unwrap().retain(|row|!ids.contains(&row["request_id"].as_str().unwrap_or("")));
    let remaining=graph.open_decisions()?;
    run_doc["awaiting_operator"]=remaining.first().map(|row|row["need"].clone()).unwrap_or(Value::Null);
    if remaining.is_empty() && run_doc["control_mode"]!="frozen"{handle.paused.store(false,Ordering::SeqCst);}
    emit_event(state,run_id,None,"hitl.resolved",json!({"command_id":command_id,"request_ids":ids,"action":action,"delivery":delivery,"continuation_intent_id":continuation}));
    Ok((true,json!({"kind":if dismiss{"decision_dismissed"}else{"decision_answered"},"delivery":delivery,"result":result})))
}
