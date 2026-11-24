//! A terminal observation is committed under its execution fence before publication.
use super::*;

tokio::task_local! {pub(super) static GENERATION:(String,u64);}
pub(super) fn task_generation(run:&str)->Option<u64>{GENERATION.try_with(|(id,generation)|if id==run{Some(*generation)}else{None}).ok().flatten()}

pub fn settle_terminal_outcome(doc: &Value, requested_solved: bool, requested_reason: &str) -> (bool, String) {
    let mode = doc["mode"].as_str().unwrap_or("ctf");
    let doc_solved = doc["solved"] == true;
    let multi_flag = doc["multi_flag"] == true;
    let expected_flags = doc["expected_flags"].as_u64().unwrap_or(1);
    let platform_confirmation_required = doc["platform_confirmation_required"] == true;
    let has_valid_flags = doc["flags"]
        .as_array()
        .is_some_and(|rows| rows.iter().any(|v| v.as_str().is_some_and(|s| !s.trim().is_empty())));
    let hard_failure = matches!(
        requested_reason,
        "preflight_failed"
            | "graph_origin_failed"
            | "pentest_contract_unavailable"
            | "unsupported_mode"
            | "runtime_failure"
    );
    let collected_unknown_multi_flag = mode != "pentest"
        && !platform_confirmation_required
        && multi_flag
        && expected_flags <= 1
        && has_valid_flags
        && !hard_failure;
    let solved = requested_solved || doc_solved || collected_unknown_multi_flag;
    let effective_reason = if solved
        && mode != "pentest"
        && matches!(requested_reason, "operator_stop" | "no_progress" | "budget_exhausted")
    {
        "solved".to_owned()
    } else {
        requested_reason.to_owned()
    };
    (solved, effective_reason)
}

pub(super) fn finish(state:&AppState,run:&str,generation:u64,solver:Option<&str>,solved:bool,reason:&str,detail:Option<&str>)->Result<(),String>{
    finish_projected(state,run,generation,solver,solved,reason,detail,None)
}
#[allow(clippy::too_many_arguments)]
pub(super) fn finish_projected(state:&AppState,run:&str,generation:u64,solver:Option<&str>,solved:bool,reason:&str,detail:Option<&str>,projection:Option<(&Value,&Value)>)->Result<(),String>{
    let legacy_key=crate::runner::canonical_digest(json!([run,generation]))?;
    let receipt=state.store.document_scope(||->Result<Value,String>{
        let mut doc=state.store.get("runs",run).map_err(|e|e.to_string())?.ok_or("Run missing at terminal transition")?;
        if let Some((before,after))=projection{if &doc!=before{return Err("Run changed before terminal projection".into());}doc=after.clone();state.store.put("runs",run,&doc).map_err(|e|e.to_string())?;}
        if doc["execution_generation"].as_u64().unwrap_or(1)!=generation{return Err("run.execution_generation_stale: terminal observation belongs to an earlier execution".into());}
        if doc["running"]==false&&doc["terminal_generation"].as_u64()==Some(generation){
            let prior_key=doc["terminal_receipt_id"].as_str().unwrap_or(&legacy_key);
            if let Some(prior)=state.store.get("run_terminal_receipts",prior_key).map_err(|e|e.to_string())?{return Ok(prior);}
        }
        let sequence=doc["terminal_sequence"].as_u64().unwrap_or(0).checked_add(1).ok_or("Terminal sequence exhausted")?;
        let key=crate::runner::canonical_digest(json!([run,generation,sequence]))?;
        if doc["running"]==true{execution_budget::checkpoint(state,run)?;doc=state.store.get("runs",run).map_err(|e|e.to_string())?.ok_or("Run missing after budget checkpoint")?;}
        doc["terminal_sequence"]=json!(sequence);doc["terminal_receipt_id"]=json!(key);
        let (solved, effective_reason) = settle_terminal_outcome(&doc, solved, reason);
        doc["status"]=json!(if solved{"solved"}else{"finished"});doc["running"]=json!(false);doc["finished"]=json!(true);doc["solved"]=json!(solved);
        doc["terminal_at"]=json!(now_secs());doc["terminal_generation"]=json!(generation);doc["outcome_reason"]=json!(effective_reason);doc["outcome_detail"]=detail.map(|v|json!(v)).unwrap_or(Value::Null);
        doc["awaiting_operator"]=Value::Null;doc["updated"]=json!(now_secs());
        let receipt=json!({"id":key,"run_id":run,"generation":generation,"solver_id":solver,"state":"publication_pending","payload":{"terminal_receipt_id":key,"execution_generation":generation,"solved":solved,"flag":doc["flags"].as_array().and_then(|rows|rows.first()),"flags":doc["flags"].as_array().cloned().unwrap_or_default(),"expected_flags":doc["expected_flags"].as_u64().unwrap_or(1),"multi_flag":doc["multi_flag"]==true,"reason":effective_reason,"detail":detail.unwrap_or("")}});
        state.store.put("runs",run,&doc).map_err(|e|e.to_string())?;
        state.store.put("run_terminal_receipts",&key,&receipt).map_err(|e|e.to_string())?;Ok(receipt)
    }).map_err(|e|e.to_string())??;
    if let Err(error)=publish(state,receipt){tracing::warn!(run_id=run,%error,"Committed terminal receipt still awaits publication");}
    Ok(())
}

fn publish(state:&AppState,mut receipt:Value)->Result<(),String>{
    if receipt["state"]!="publication_pending"{return Ok(())}
    let run=receipt["run_id"].as_str().ok_or("Terminal Run ID missing")?.to_owned();
    let key=receipt["id"].as_str().ok_or("Terminal receipt identity missing")?.to_owned();
    let handle=state.runs.handle(state,&run);let _publication=handle.terminal_publication.lock().unwrap();
    let current=state.store.get("runs",&run).map_err(|e|e.to_string())?;
    if current.as_ref().is_none_or(|doc|doc["execution_generation"].as_u64().unwrap_or(1)!=receipt["generation"].as_u64().unwrap_or(1)||doc["running"]==true||doc["terminal_receipt_id"].as_str().is_some_and(|active|active!=key)){
        receipt["state"]=json!("superseded");
    }else{
        let exists=persisted_event_iter(state,&run)?.any(|event|event["event_type"]=="run.finished"&&event["payload"]["terminal_receipt_id"]==key);
        if !exists{let event=emit_event(state,&run,receipt["solver_id"].as_str(),"run.finished",receipt["payload"].clone());if event["persisted"]==false{return Err("Terminal event persistence failed; publication remains pending".into());}}
        receipt["state"]=json!("published");receipt["published_at"]=json!(now_secs());
    }
    state.store.put("run_terminal_receipts",&key,&receipt).map_err(|e|e.to_string())
}

pub(super) fn reconcile(state:&AppState)->Result<(),String>{
    let mut errors=Vec::new();
    for row in state.store.list("run_terminal_receipts").map_err(|e|e.to_string())?.into_iter().filter(|row|row["state"]=="publication_pending"){
        if let Err(error)=publish(state,row){errors.push(error);}
    }
    if errors.is_empty(){Ok(())}else{Err(errors.join("; "))}
}
