use super::*;

const DELIVERY_RESULTS: &str = "native_control_delivery_results";

fn result_key(tid: &str, request: &str) -> String { json!([tid, request]).to_string() }

fn staged_result(result: &Result<Value, RuntimeError>) -> Value {
    match result {
        Ok(delivery) => json!({"state":"completed","delivery":delivery}),
        Err(error) => json!({"state":if error.code == "runtime.delivery_unknown" {"unknown"} else {"failed"},"error":error.as_json()}),
    }
}

pub(super) fn stage(s: &Arc<AppState>, tid: &str, request: &str, result: &Result<Value, RuntimeError>) -> Result<(), String> {
    let Some(document) = s.store.get(chat::COLLECTION,tid).map_err(|error|error.to_string())? else { return Ok(()) };
    if !document["native_controls"][request].is_object() { return Ok(()); }
    let key = result_key(tid,request);
    let previous = s.store.get(DELIVERY_RESULTS,&key).map_err(|error|error.to_string())?;
    if previous.as_ref().is_some_and(|row|row["result"]["state"]=="completed") { return Ok(()); }
    let row = json!({"thread_id":tid,"request_id":request,"result":staged_result(result),"observed_at":now()});
    s.store.put(DELIVERY_RESULTS,&key,&row).map_err(|error|error.to_string())
}

pub(super) fn spawn_recovery(s: Arc<AppState>) {
    let state = Arc::downgrade(&s);
    crate::business::spawn(s.clone(),crate::business::Module::Chat,"chat.control_recovery",async move {
        let mut tick = tokio::time::interval(Duration::from_secs(2));
        loop {
            tick.tick().await;
            let Some(s) = state.upgrade() else { return };
            if s.admission.draining() || crate::business::require(&s,crate::business::Module::Chat).is_err() { continue; }
            let _guard = s.chat.active.lock().await;
            match s.store.list(DELIVERY_RESULTS) {
                Ok(rows) => for row in rows {
                    let (Some(tid),Some(request)) = (row["thread_id"].as_str(),row["request_id"].as_str()) else {
                        tracing::error!("Native control result journal has no identity"); continue;
                    };
                    if let Err(error) = settle_recorded(&s,&result_key(tid,request)) {
                        tracing::warn!(thread_id=tid,request_id=request,%error,"Native delivery recovery remains pending");
                    }
                },
                Err(error) => tracing::warn!(%error,"Native delivery recovery could not read its journal"),
            }
        }
    });
}

/// The durable result journal survives a crash between delivery and transcript settlement.
/// Retrying settlement never sends the provider command again.
pub(super) async fn settle(s: &Arc<AppState>, tid: &str, request_id: &str, result: Result<Value, RuntimeError>) {
    let key = result_key(tid, request_id);
    let mut attempts = 0u64;
    loop {
        let outcome = {
            let _guard = s.chat.active.lock().await;
            s.store.document_scope(|| -> Result<(), String> {
                stage(s,tid,request_id,&result)
            }).map_err(|error| error.to_string()).and_then(|result| result)
        };
        if outcome.is_ok() { break }
        attempts += 1;
        tracing::error!(thread_id=tid, request_id, attempts, error=?outcome.err(), "Native delivery result awaits durable recording");
        tokio::time::sleep(Duration::from_millis(100 * attempts.min(50))).await;
    }
    loop {
        let outcome = {
            let _guard = s.chat.active.lock().await;
            settle_recorded(s, &key)
        };
        match outcome {
            Ok(()) => return,
            Err(error) => {
                attempts += 1;
                tracing::error!(thread_id=tid, request_id, attempts, %error, "Native delivery receipt remains pending");
                tokio::time::sleep(Duration::from_millis(100 * attempts.min(50))).await;
            }
        }
    }
}

fn settle_recorded(s: &Arc<AppState>, journal_key: &str) -> Result<(), String> {
    s.store.document_scope(|| -> Result<(), String> {
        let Some(row) = s.store.get(DELIVERY_RESULTS, journal_key).map_err(|error| error.to_string())? else { return Ok(()) };
        let tid = row["thread_id"].as_str().ok_or("Native delivery thread identity missing")?;
        let request_id = row["request_id"].as_str().ok_or("Native delivery request identity missing")?;
        if s.store.get(chat::COLLECTION, tid).map_err(|error| error.to_string())?.is_some() {
            apply_result(s, tid, request_id, &row["result"])?;
        }
        s.store.delete(DELIVERY_RESULTS, journal_key).map_err(|error| error.to_string())?;
        Ok(())
    }).map_err(|error| error.to_string())?
}

fn apply_result(s: &Arc<AppState>, tid: &str, request_id: &str, result: &Value) -> Result<(), String> {
    let mut doc = chat::load(s, tid).map_err(|(_, Json(error))| error.to_string())?;
    let Some(control)=doc["native_controls"].get(request_id).cloned()else{return Ok(())};
    let key=control["key"].as_str().unwrap_or("");
    let Some(old)=s.store.get("conversation_commands",key).map_err(|error| error.to_string())? else { return Ok(()) };
    if old["response"]["receipt"]["state"]=="completed" {return Ok(());}
    let mut record=old.clone();
    let (mut status,mut error,mut delivery)=match result["state"].as_str() {
        Some("completed")=>("completed",Value::Null,result["delivery"].clone()),
        Some("unknown")=>("unknown",result["error"].clone(),Value::Null),
        Some("failed")=>("failed",result["error"].clone(),Value::Null),
        _=>return Err("Native delivery result state is invalid".into()),
    };
    if control["operation"] == "steer" {
        if status == "failed" && let Some(steer) = doc["pending_steers"].as_object_mut().and_then(|rows|rows.remove(request_id)) {
            requeue_steer(s,tid,&mut doc,request_id,&steer);
        }
        if let Some(queue) = doc["native_steer_fallbacks"][request_id].as_str() {
            delivery = json!({"status":"queued","queue_id":queue,"fallback":true,"native_error":error});
            status = "completed"; error = Value::Null;
        }
    }
    record["response"]["receipt"]["state"]=json!(status);
    record["response"]["receipt"]["output"]["delivery"]=delivery;
    record["response"]["receipt"]["error"]=error.clone();
    doc["native_controls"][request_id]["state"]=json!(status);
    doc["native_controls"][request_id]["settled_at"]=json!(now());
    if status!="completed" {
        let pending=if status=="unknown"{"delivery_unknown"}else{"pending"};
        match control["operation"].as_str().unwrap_or("") {
            "approval"=>{if doc["thread"]["state"]["pending_approvals"][request_id].is_object(){doc["thread"]["state"]["pending_approvals"][request_id]["status"]=json!(pending);doc["thread"]["state"]["pending_approvals"][request_id]["delivery_error"]=error.clone();}},
            "user_input"=>{if doc["thread"]["state"]["pending_user_input"]["request_id"]==request_id{doc["thread"]["state"]["pending_user_input"]["status"]=json!(pending);doc["thread"]["state"]["pending_user_input"]["delivery_error"]=error.clone();}},
            _=>{}
        }
        translate::refresh_pending(&mut doc);
    }
    chat::emit(s,tid,&mut doc,"core.runtime.control.settled",json!({"request_id":request_id,"command_id":control["command_id"],"operation":control["operation"],"status":status,"error":error}));
    let mut records = vec![("conversation_commands", key, &record)];
    if let Some(key) = control["idem_key"].as_str() { records.push(("conversation_commands", key, &record)); }
    let Some((events, watermark)) = s.store.chat_commit_with_documents(tid, &doc, None, None,
        &[("conversation_commands", key, Some(&old))], &records).map_err(|error| error.to_string())?
        else { return Err("Control receipt changed before settlement".into()) };
    chat::finish_save(s, tid, &mut doc, events, watermark);
    Ok(())
}

pub(super) fn recover(s:&Arc<AppState>)->rusqlite::Result<()> {
    for row in s.store.list(DELIVERY_RESULTS)? {
        let tid = row["thread_id"].as_str().ok_or(rusqlite::Error::InvalidQuery)?;
        let request = row["request_id"].as_str().ok_or(rusqlite::Error::InvalidQuery)?;
        settle_recorded(s, &result_key(tid, request)).map_err(|error| rusqlite::Error::ToSqlConversionFailure(Box::new(std::io::Error::other(error))))?;
    }
    for before in s.store.list(chat::COLLECTION)? {
        let Some(tid)=before["thread"]["thread_id"].as_str()else{continue};let mut doc=before.clone();let mut records=Vec::<(String,Value)>::new();
        let ids=doc["native_controls"].as_object().into_iter().flat_map(|rows|rows.iter()).filter(|(id,row)|
            (row["state"]=="processing"&&row["epoch"]!=s.chat.epoch()) ||
            (row["state"]!="completed" && doc["native_steer_fallbacks"][id.as_str()].is_string())
        ).map(|(id,_)|id.clone()).collect::<Vec<_>>();
        for id in ids {
            let control=doc["native_controls"][&id].clone();
            let queue=doc["native_steer_fallbacks"][&id].as_str().map(str::to_owned);
            let status=if queue.is_some(){"completed"}else{"unknown"};
            let error=if queue.is_some(){Value::Null}else{json!({"code":"runtime.delivery_unknown","category":"unknown","message":"Server restarted before provider delivery was confirmed","retryable":false})};
            for key in [control["key"].as_str(),control["idem_key"].as_str()].into_iter().flatten(){if let Some(mut record)=s.store.get("conversation_commands",key)?{
                record["response"]["receipt"]["state"]=json!(status);record["response"]["receipt"]["error"]=error.clone();
                if let Some(queue)=&queue{record["response"]["receipt"]["output"]["delivery"]=json!({"status":"queued","queue_id":queue,"fallback":true});}
                records.push((key.into(),record));
            }}
            doc["native_controls"][&id]["state"]=json!(status);
            if queue.is_none() {
                if control["operation"]=="approval"&&doc["thread"]["state"]["pending_approvals"][&id].is_object(){doc["thread"]["state"]["pending_approvals"][&id]["status"]=json!("delivery_unknown");}
                if control["operation"]=="user_input"&&doc["thread"]["state"]["pending_user_input"]["request_id"]==id{doc["thread"]["state"]["pending_user_input"]["status"]=json!("delivery_unknown");}
            }
            chat::emit(s,tid,&mut doc,"core.runtime.control.settled",json!({"request_id":id,"command_id":control["command_id"],"status":status,"error":error}));
        }
        if !records.is_empty(){let borrowed=records.iter().map(|(key,row)|("conversation_commands",key.as_str(),row)).collect::<Vec<_>>();if let Some((events,watermark))=s.store.chat_commit_with_documents(tid,&doc,Some(&before),None,&[],&borrowed)?{chat::finish_save(s,tid,&mut doc,events,watermark);}}
    }Ok(())
}

/// Reconcile late native acknowledgements even after a waiting caller timed out.
pub(super) fn observed(event:&AgentEvent)->Option<(String,Result<Value,RuntimeError>)>{
    match event {
        AgentEvent::Origin{event,..}=>observed(event),
        AgentEvent::ApprovalResolved{request_id,decision}=>Some((request_id.clone(),Ok(json!({"status":"confirmed","source":"native_event","decision":decision})))),
        AgentEvent::UserInputResolved{request_id,answers}=>Some((request_id.clone(),Ok(json!({"status":"confirmed","source":"native_event","answers":answers})))),
        AgentEvent::SteerAccepted{request_id}=>Some((request_id.clone(),Ok(json!({"status":"confirmed","source":"native_event"})))),
        AgentEvent::ControlDelivery{request_id,status,error,..} if status=="accepted"=>Some((request_id.clone(),Ok(json!({"status":"accepted"})))),
        AgentEvent::ControlDelivery{request_id,status,error,..} if status=="unknown"=>Some((request_id.clone(),Err(RuntimeError::new("runtime.delivery_unknown",error.as_ref().map(Value::to_string).unwrap_or_else(||"Provider response delivery is unknown".into()))))),
        AgentEvent::ControlDelivery{request_id,status,error,..} if status=="failed"=>Some((request_id.clone(),Err(RuntimeError::new("runtime.delivery_failed",error.as_ref().map(Value::to_string).unwrap_or_else(||"Native delivery failed".into()))))),
        _=>None,
    }
}
