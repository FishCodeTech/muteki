//! Connection probe and competition sync through the source CommandAPI outbox.
use super::*;
use fs2::FileExt;

const OUTBOX: &str = "competition_connection_outbox_live";
const EFFECTS: &str = "platform_effect_receipts";
const SYNC_IDENTITIES: &str = "competition_sync_identities";

fn accepted(command_id: &str, aggregate_type: &str, aggregate_id: &str) -> Value {
    json!({"schema_version":1,"command_id":command_id,"receipt_id":id("rcpt"),
        "state":"accepted","run_id":null,
        "aggregate":{"type":aggregate_type,"id":aggregate_id},"event_cursor":null,
        "deduplicated":false,"submitted_at":now(),"completed_at":null,
        "next":["receipt","snapshot","events","wait"],"output":{},
        "effect_ids":[],"error":null})
}

fn prior(state: &AppState, command_id: &str) -> ApiResult<Option<Value>> {
    prior_receipt(state,command_id).map(|value|value.map(|receipt|json!({"receipt":receipt})))
}

fn enqueue(state: &AppState, command_id: &str, aggregate_type: &str,
    aggregate_id: &str, connection: &Value, op: &str,
    extra: &[(&str, &str, Option<&Value>, &Value)]) -> ApiResult<Value> {
    let existing = state.store.get(COMMANDS,command_id).map_err(ApiError::internal)?
        .ok_or_else(||ApiError::missing("command receipt"))?;
    if existing["response"]["receipt"]["state"] != "accepted" {
        return Ok(json!({"receipt":existing["response"]["receipt"]}));
    }
    let outbox_id = id("outbox");
    let effect_id = id("effect");
    let destination = format!("platform.{}",field(connection,"platform_kind"));
    let mut output = json!({"outbox_ids":[outbox_id],"effect_ids":[effect_id],
        "effect_state":"waiting"});
    output[if op == "probe_connection" {"connection_id"} else {"competition_id"}] = json!(aggregate_id);
    let mut receipt = existing["response"]["receipt"].clone();
    receipt["state"] = json!("waiting");
    receipt["effect_ids"] = json!([effect_id]);
    receipt["output"] = output;
    let mut updated = existing.clone();
    updated["response"]["receipt"] = receipt.clone();
    let outbox = json!({"outbox_id":outbox_id,"command_id":command_id,
        "effect_id":effect_id,"aggregate_type":aggregate_type,"aggregate_id":aggregate_id,
        "connection_id":field(connection,"connection_id"),"competition_id":if op == "sync" {aggregate_id} else {""},
        "op":op,"destination":destination,"status":"pending","attempts":0,
        "next_attempt_at":null,"created_at":now(),"updated_at":now(),
        "delivered_at":null,"last_error":null});
    let effect = json!({"effect_id":effect_id,"command_id":command_id,
        "acceptance_receipt_id":receipt["receipt_id"],"domain":"competition",
        "state":"accepted","aggregate":{"type":aggregate_type,"id":aggregate_id},
        "correlation_id":command_id,"outbox_id":outbox_id,"destination":destination,
        "idempotency_key":command_id,"attempts":0,"retry_after":null,
        "object_links":{},"output":{},"error":null,
        "created_at":now(),"updated_at":now(),"completed_at":null});
    let mut expected = vec![(COMMANDS,command_id,Some(&existing)),(OUTBOX,outbox_id.as_str(),None),
        (EFFECTS,effect_id.as_str(),None)];
    let mut records = vec![(COMMANDS,command_id,&updated),(OUTBOX,outbox_id.as_str(),&outbox),
        (EFFECTS,effect_id.as_str(),&effect)];
    for (collection,key,previous,record) in extra {
        expected.push((collection,key,*previous));
        records.push((collection,key,*record));
    }
    if !state.store.put_many_if(&expected,&records).map_err(ApiError::internal)? {
        let current = state.store.get(COMMANDS,command_id).map_err(ApiError::internal)?
            .ok_or_else(||ApiError::missing("command receipt"))?;
        return Ok(json!({"receipt":current["response"]["receipt"]}));
    }
    Ok(json!({"receipt":receipt}))
}

pub(super) fn test(state: &Arc<AppState>, connection_id: &str, body: &Value) -> ApiResult<Value> {
    let command_id = command_id(body);
    let expected_request = json!({
        "command_type": "connection.test",
        "aggregate_id": connection_id,
        "payload": {"connection_id": connection_id}
    });
    if !command_id.is_empty()
        && let Some(existing) = state
            .store
            .get(COMMANDS, &command_id)
            .map_err(ApiError::internal)?
    {
        if existing["request"] != expected_request {
            return Err(ApiError::new(
                StatusCode::CONFLICT,
                "command.idempotency_conflict",
                "command_id was used with different connection.test input",
            ));
        }
        if let Some(response) = prior(state, &command_id)? {
            return Ok(response);
        }
    }
    let admission = &body["_planned_admission"];
    let connection = loop {
        if let Some(receipt) = planned_key_receipt(state,admission)? {
            return Ok(json!({"receipt":receipt}));
        }
        if admission.is_object() && state.store.get("platform_command_claims",&command_id)
            .map_err(ApiError::internal)?.is_some() {
            return Err(ApiError::new(StatusCode::CONFLICT,"command.outcome_unknown",
                "Prior command outcome must be inspected before retry"));
        }
        let connection = live_connection(state,connection_id)?;
        let receipt = accepted(&command_id,"connection",connection_id);
        let mut stored = json!({"request":{"command_type":"connection.test",
            "aggregate_id":connection_id,"payload":{"connection_id":connection_id}},
            "plan":{"connection":connection},
            "response":{"receipt":receipt}});
        let claim = admission.get("claim").filter(|value|value.is_object());
        if let Some(claim) = claim {
            stored["request_fingerprint"] = claim["request_fingerprint"].clone();
            stored["status"] = json!(StatusCode::ACCEPTED.as_u16());
        }
        let event = leases::planned_event("","connection",connection_id,
            "competition.connection.test_requested",&command_id,
            json!({"connection_id":connection_id}));
        let version_key = format!("connection\0{connection_id}");
        let version = state.store.get("platform_aggregate_versions",&version_key).map_err(ApiError::internal)?;
        if body["expected_version"].as_u64().is_some_and(|expected|
            version.as_ref().and_then(|row|row["version"].as_u64()).unwrap_or(0) != expected) {
            return Err(ApiError::new(StatusCode::CONFLICT,"command.version_conflict",
                "expected_version does not match connection stream"));
        }
        let mut expected = vec![(CONNECTIONS,connection_id,Some(&connection)),
            (COMMANDS,command_id.as_str(),None),
            ("platform_aggregate_versions",version_key.as_str(),version.as_ref())];
        let mut records = vec![(COMMANDS,command_id.as_str(),&stored)];
        if let Some(claim) = claim {
            expected.push(("platform_command_claims",command_id.as_str(),None));
            records.push(("platform_command_claims",command_id.as_str(),claim));
            let key_id = field(admission,"key_id");
            if !key_id.is_empty() {
                expected.push(("platform_idempotency_keys",key_id,None));
                records.push(("platform_idempotency_keys",key_id,&admission["keyed"]));
            }
        }
        if put_records_with_events(state,"",&expected,&records,&[event])? { break connection; }
        if let Some(response) = prior(state,&command_id)? { return Ok(response); }
    };
    enqueue(state,&command_id,"connection",connection_id,&connection,"probe_connection",&[])
}

pub(super) fn sync(state: &Arc<AppState>, body: &Value) -> ApiResult<Value> {
    let command_id = command_id(body);
    let admission = &body["_planned_admission"];
    let connection_id = field(body,"connection_id");
    let external_id = field(body,"external_competition_id");
    if !command_id.is_empty()
        && let Some(existing) = state
            .store
            .get(COMMANDS, &command_id)
            .map_err(ApiError::internal)?
    {
        if field(&existing["request"], "command_type") != "competition.sync"
            || field(&existing["request"]["payload"], "connection_id") != connection_id
            || field(&existing["request"]["payload"], "external_competition_id") != external_id
        {
            return Err(ApiError::new(
                StatusCode::CONFLICT,
                "command.idempotency_conflict",
                "command_id was used with different competition.sync input",
            ));
        }
        if let Some(response) = prior(state, &command_id)? {
            return Ok(response);
        }
    }
    if connection_id.is_empty() || external_id.is_empty() {
        return Err(ApiError::invalid("connection_id and external_competition_id are required"));
    }
    let identity_key = serde_json::to_string(&json!([connection_id,external_id])).map_err(ApiError::internal)?;
    loop {
        if let Some(receipt) = planned_key_receipt(state,admission)? {
            return Ok(json!({"receipt":receipt}));
        }
        if admission.is_object() && state.store.get("platform_command_claims",&command_id)
            .map_err(ApiError::internal)?.is_some() {
            return Err(ApiError::new(StatusCode::CONFLICT,"command.outcome_unknown",
                "Prior command outcome must be inspected before retry"));
        }
        let connection = live_connection(state,connection_id)?;
        let identity = state.store.get(SYNC_IDENTITIES,&identity_key).map_err(ApiError::internal)?;
        let existing = if let Some(identity) = identity.as_ref() {
            state.store.get(COMPETITIONS,field(identity,"competition_id")).map_err(ApiError::internal)?
        } else {
            state.store.list(COMPETITIONS).map_err(ApiError::internal)?.into_iter().find(|row|
                row["historical"] != true && row["connection_id"] == connection_id
                && row["external_competition_id"] == external_id)
        };
        if identity.is_some() && existing.is_none() {
            return Err(ApiError::new(StatusCode::CONFLICT,"command.outcome_unknown",
                "Competition sync is accepted and awaiting its local effect"));
        }
        let competition_id = existing.as_ref().map(|row|field(row,"competition_id").to_owned())
            .unwrap_or_else(||id("comp"));
        let title = field(body,"title");
        let description = field(body,"description");
        let initial_status = body
            .get("platform_status")
            .filter(|v| v.is_object())
            .map(extension_adapter::sanitize_platform_value)
            .unwrap_or_else(|| json!({}));
        let mut record = existing.clone().unwrap_or_else(||json!({
            "competition_id":competition_id,"connection_id":connection_id,
            "external_competition_id":external_id,"title":title,"description":description,
            "starts_at":body["starts_at"],"ends_at":body["ends_at"],
            "platform_status":initial_status,"scheduler_state":"stopped","tombstoned":false,
            "archived":false,"last_sync_attempt_at":null,"last_synced_at":null,
            "last_sync_error":"","created_at":now(),"updated_at":now()}));
        if record["archived"] == true { record["archived"] = json!(false); }
        let receipt = accepted(&command_id,"competition",&competition_id);
        let mut stored = json!({"request":{"command_type":"competition.sync",
            "aggregate_id":competition_id,"payload":{"competition_id":competition_id,
            "connection_id":connection_id,"external_competition_id":external_id}},
            "plan":{"connection":connection,"competition":record},
            "response":{"receipt":receipt}});
        let claim = admission.get("claim").filter(|value|value.is_object());
        if let Some(claim) = claim {
            stored["request_fingerprint"] = claim["request_fingerprint"].clone();
            stored["status"] = json!(StatusCode::ACCEPTED.as_u16());
        }
        let identity_record = json!({"competition_id":competition_id});
        let mut events = Vec::new();
        if existing.is_none() {
            events.push(leases::planned_event(&competition_id,"competition",&competition_id,
                "competition.registered",&command_id,
                json!({"competition_id":competition_id,"connection_id":connection_id,
                    "external_competition_id":external_id,"title":record["title"]})));
        }
        events.push(leases::planned_event(&competition_id,"competition",&competition_id,
            "competition.sync.requested",&command_id,
            json!({"competition_id":competition_id,"connection_id":connection_id})));
        let version_key = format!("competition\0{competition_id}");
        let version = state.store.get("platform_aggregate_versions",&version_key).map_err(ApiError::internal)?;
        if body["expected_version"].as_u64().is_some_and(|expected|
            version.as_ref().and_then(|row|row["version"].as_u64()).unwrap_or(0) != expected) {
            return Err(ApiError::new(StatusCode::CONFLICT,"command.version_conflict",
                "expected_version does not match competition stream"));
        }
        let mut expected = vec![(SYNC_IDENTITIES,identity_key.as_str(),identity.as_ref()),
            (COMMANDS,command_id.as_str(),None),
            ("platform_aggregate_versions",version_key.as_str(),version.as_ref())];
        let mut records = vec![(SYNC_IDENTITIES,identity_key.as_str(),&identity_record),
            (COMMANDS,command_id.as_str(),&stored)];
        if let Some(claim) = claim {
            expected.push(("platform_command_claims",command_id.as_str(),None));
            records.push(("platform_command_claims",command_id.as_str(),claim));
            let key_id = field(admission,"key_id");
            if !key_id.is_empty() {
                expected.push(("platform_idempotency_keys",key_id,None));
                records.push(("platform_idempotency_keys",key_id,&admission["keyed"]));
            }
        }
        if !put_records_with_events(state,&competition_id,&expected,&records,&events)? {
            if let Some(response) = prior(state,&command_id)? { return Ok(response); }
            continue;
        }
        let prior_policy = state.store.get(POLICIES,&competition_id).map_err(ApiError::internal)?;
        let policy = prior_policy.as_ref().cloned()
            .unwrap_or_else(||policy_for_connection(&competition_id,&connection));
        return enqueue(state,&command_id,"competition",&competition_id,&connection,"sync",
            &[(COMPETITIONS,&competition_id,existing.as_ref(),&record),
              (POLICIES,&competition_id,prior_policy.as_ref(),&policy)]);
    }
}

pub(super) fn recover_missing(state: &AppState, command_id: &str) -> ApiResult<Option<Value>> {
    let Some(command) = state.store.get(COMMANDS,command_id).map_err(ApiError::internal)?
        else {return Ok(None)};
    if command["response"]["receipt"]["state"] != "accepted" {
        return prior(state,command_id);
    }
    if state.store.list(OUTBOX).map_err(ApiError::internal)?.iter()
        .any(|row|row["command_id"] == command_id) {
        return prior(state,command_id);
    }
    let request = &command["request"];
    let plan = &command["plan"];
    match field(request,"command_type") {
        "connection.test" => {
            let connection_id = field(&request["payload"],"connection_id");
            let connection_id = if connection_id.is_empty() {field(request,"aggregate_id")}
                else {connection_id};
            let connection = if plan["connection"].is_object() {plan["connection"].clone()}
                else {state.store.get(CONNECTIONS,connection_id).map_err(ApiError::internal)?
                    .ok_or_else(||ApiError::missing("connection"))?};
            enqueue(state,command_id,"connection",connection_id,&connection,"probe_connection",&[])
                .map(Some)
        }
        "competition.sync" => {
            let competition_id = field(request,"aggregate_id");
            let connection_id = field(&request["payload"],"connection_id");
            let external_id = field(&request["payload"],"external_competition_id");
            let connection = if plan["connection"].is_object() {plan["connection"].clone()}
                else {state.store.get(CONNECTIONS,connection_id).map_err(ApiError::internal)?
                    .ok_or_else(||ApiError::missing("connection"))?};
            let existing = state.store.get(COMPETITIONS,competition_id).map_err(ApiError::internal)?;
            let record = if plan["competition"].is_object() {plan["competition"].clone()}
                else if let Some(record) = existing.clone() {record}
                else {
                    let title = state.store.list(EVENTS).map_err(ApiError::internal)?
                        .into_iter().find(|event|event["command_id"] == command_id
                            && event["event_type"] == "competition.registered")
                        .map(|event|event["payload"]["title"].clone()).unwrap_or(Value::Null);
                    json!({"competition_id":competition_id,"connection_id":connection_id,
                        "external_competition_id":external_id,"title":title,"description":"",
                        "starts_at":null,"ends_at":null,"platform_status":{},
                        "scheduler_state":"stopped","tombstoned":false,"archived":false,
                        "last_sync_attempt_at":null,"last_synced_at":null,"last_sync_error":"",
                        "created_at":now(),"updated_at":now()})
                };
            let prior_policy = state.store.get(POLICIES,competition_id).map_err(ApiError::internal)?;
            let policy = prior_policy.as_ref().cloned()
                .unwrap_or_else(||policy_for_connection(competition_id,&connection));
            enqueue(state,command_id,"competition",competition_id,&connection,"sync",
                &[(COMPETITIONS,competition_id,existing.as_ref(),&record),
                  (POLICIES,competition_id,prior_policy.as_ref(),&policy)]).map(Some)
        }
        _ => Ok(None),
    }
}

pub(super) fn reconcile_missing(state: &AppState) -> ApiResult<()> {
    for command in state.store.list(COMMANDS).map_err(ApiError::internal)? {
        if command["response"]["receipt"]["state"] != "accepted"
            || !matches!(field(&command["request"],"command_type"),
                "connection.test" | "competition.sync") {continue;}
        let command_id = field(&command["response"]["receipt"],"command_id");
        if let Err(error) = recover_missing(state,command_id) {
            tracing::warn!(command_id,code=%error.code,"Competition connection outbox recovery remains pending");
        }
    }
    Ok(())
}

fn retry_at(attempts: u64) -> String {
    let delay = (0.5 * 2_f64.powi(attempts.saturating_sub(1).min(12) as i32)).min(300.0);
    (chrono::Utc::now() + chrono::Duration::milliseconds((delay * 1000.0) as i64)).to_rfc3339()
}

fn due(row: &Value) -> bool {
    match field(row,"status") {
        "pending" | "processing" => true,
        "failed" => row["next_attempt_at"].as_str()
            .and_then(|text|chrono::DateTime::parse_from_rfc3339(text).ok())
            .is_none_or(|at|at <= chrono::Utc::now()),
        _ => false,
    }
}

fn effect_event(row: &Value, event_type: &str, payload: Value) -> Value {
    leases::planned_event(field(row,"competition_id"),field(row,"aggregate_type"),
        field(row,"aggregate_id"),event_type,field(row,"command_id"),
        json!({"effect_id":row["effect_id"],"outbox_id":row["outbox_id"],
            "destination":row["destination"],"payload":payload}))
}

fn claim(state: &AppState, outbox_id: &str) -> ApiResult<Option<Value>> {
    let Some(before) = state.store.get(OUTBOX,outbox_id).map_err(ApiError::internal)? else {return Ok(None)};
    if !due(&before) { return Ok(None); }
    let command_id = field(&before,"command_id");
    let effect_id = field(&before,"effect_id");
    let Some(effect) = state.store.get(EFFECTS,effect_id).map_err(ApiError::internal)? else {
        return Err(ApiError::missing("connection effect receipt"));
    };
    let Some(command) = state.store.get(COMMANDS,command_id).map_err(ApiError::internal)? else {
        return Err(ApiError::missing("connection command receipt"));
    };
    let attempts = before["attempts"].as_u64().unwrap_or(0)+1;
    let mut running = before.clone();
    running["status"] = json!("processing");
    running["attempts"] = json!(attempts);
    running["next_attempt_at"] = Value::Null;
    running["updated_at"] = json!(now());
    let mut running_effect = effect.clone();
    running_effect["state"] = json!("running");
    running_effect["attempts"] = json!(attempts);
    running_effect["updated_at"] = json!(now());
    let mut running_command = command.clone();
    running_command["response"]["receipt"]["state"] = json!("running");
    running_command["response"]["receipt"]["output"] = json!({
        "outbox_ids":[outbox_id],"effect_ids":[effect_id],"effect_state":"running"});
    let event = effect_event(&running,"competition.effect.running",json!({"attempt":attempts}));
    if !put_records_with_events(state,field(&running,"competition_id"),
        &[(OUTBOX,outbox_id,Some(&before)),(EFFECTS,effect_id,Some(&effect)),
          (COMMANDS,command_id,Some(&command))],
        &[(OUTBOX,outbox_id,&running),(EFFECTS,effect_id,&running_effect),
          (COMMANDS,command_id,&running_command)],&[event])? {return Ok(None)}
    Ok(Some(running))
}

async fn deliver(state: &Arc<AppState>, row: &Value) -> ApiResult<Value> {
    let connection_id = field(row,"connection_id");
    if row["op"] == "probe_connection" {
        probe_connection_effect(state,connection_id).await?;
        return Ok(json!({"connection_id":connection_id,"healthy":true}));
    }
    let competition_id = field(row,"competition_id");
    let competition = state.store.get(COMPETITIONS,competition_id).map_err(ApiError::internal)?
        .ok_or_else(||ApiError::missing("competition"))?;
    let output = sync_competition_effect(state,&competition,field(row,"command_id"),true).await?;
    Ok(output)
}

struct ConnectionFailure {
    pause_for_auth: bool,
    terminal: bool,
    category: &'static str,
    retryable: bool,
}

/// A 403 permission denial is not an authentication pause. Auth still pauses
/// and can be retried after reauthorization; permission is terminal.
fn classify_connection_failure(status: StatusCode, code: &str, attempts: u64) -> ConnectionFailure {
    let permission = code == "competition.platform_permission";
    let pause_for_auth = !permission
        && (status == StatusCode::UNAUTHORIZED
            || status == StatusCode::FORBIDDEN
            || code == "competition.auth_required");
    let transport_retryable =
        status.is_server_error() || status == StatusCode::TOO_MANY_REQUESTS;
    let terminal = !pause_for_auth && (!transport_retryable || attempts >= 8);
    ConnectionFailure {
        pause_for_auth,
        terminal,
        category: if permission {
            "permission"
        } else if pause_for_auth {
            "auth"
        } else {
            "state"
        },
        retryable: !terminal,
    }
}

fn settle(state: &AppState, claimed: &Value, result: ApiResult<Value>) -> ApiResult<()> {
    let outbox_id = field(claimed,"outbox_id");
    let Some(before) = state.store.get(OUTBOX,outbox_id).map_err(ApiError::internal)? else {return Ok(())};
    if before["status"] != "processing" {return Ok(())}
    let command_id = field(&before,"command_id");
    let effect_id = field(&before,"effect_id");
    let Some(effect) = state.store.get(EFFECTS,effect_id).map_err(ApiError::internal)? else {
        return Err(ApiError::missing("connection effect receipt"));
    };
    let Some(command) = state.store.get(COMMANDS,command_id).map_err(ApiError::internal)? else {
        return Err(ApiError::missing("connection command receipt"));
    };
    let mut outbox = before.clone();
    let mut next_effect = effect.clone();
    let mut next_command = command.clone();
    let attempts = before["attempts"].as_u64().unwrap_or(1);
    let event = match result {
        Ok(output) => {
            outbox["status"] = json!("delivered");
            outbox["delivered_at"] = json!(now());
            outbox["next_attempt_at"] = Value::Null;
            outbox["last_error"] = Value::Null;
            next_effect["state"] = json!("completed");
            next_effect["output"] = output.clone();
            next_effect["error"] = Value::Null;
            next_effect["completed_at"] = json!(now());
            next_command["response"]["receipt"]["state"] = json!("completed");
            next_command["response"]["receipt"]["completed_at"] = json!(now());
            next_command["response"]["receipt"]["error"] = Value::Null;
            next_command["response"]["receipt"]["output"] = json!({
                "outbox_ids":[outbox_id],"effect_ids":[effect_id],"effect_state":"completed",
                "result":output});
            effect_event(&outbox,"competition.effect.completed",
                json!({"attempt":attempts,"output":next_effect["output"]}))
        },
        Err(error) => {
            let class = classify_connection_failure(error.status, error.code, attempts);
            outbox["status"] = json!(if class.pause_for_auth {"paused"} else if class.terminal {"dead_letter"} else {"failed"});
            outbox["last_error"] = json!(error.code);
            outbox["next_attempt_at"] = if class.pause_for_auth || class.terminal {Value::Null} else {json!(retry_at(attempts))};
            next_effect["state"] = json!(if class.terminal {"dead_letter"} else {"waiting"});
            next_effect["retry_after"] = outbox["next_attempt_at"].clone();
            let failure = json!({"code":error.code,"message":error.message,
                "category":class.category,"retryable":class.retryable});
            next_effect["error"] = failure.clone();
            next_effect["completed_at"] = if class.terminal {json!(now())} else {Value::Null};
            next_command["response"]["receipt"]["state"] = json!(if class.terminal {"failed"} else {"waiting"});
            next_command["response"]["receipt"]["completed_at"] = if class.terminal {json!(now())} else {Value::Null};
            next_command["response"]["receipt"]["error"] = failure;
            next_command["response"]["receipt"]["output"] = json!({
                "outbox_ids":[outbox_id],"effect_ids":[effect_id],
                "effect_state":if class.terminal {"failed"} else {"waiting"}});
            effect_event(&outbox,if class.terminal {"competition.effect.dead_letter"}
                else {"competition.effect.retry_scheduled"},
                json!({"attempt":attempts,"error_code":error.code}))
        },
    };
    outbox["updated_at"] = json!(now());
    next_effect["updated_at"] = json!(now());
    if !put_records_with_events(state,field(&outbox,"competition_id"),
        &[(OUTBOX,outbox_id,Some(&before)),(EFFECTS,effect_id,Some(&effect)),
          (COMMANDS,command_id,Some(&command))],
        &[(OUTBOX,outbox_id,&outbox),(EFFECTS,effect_id,&next_effect),
          (COMMANDS,command_id,&next_command)],&[event])? {
        return Err(ApiError::new(StatusCode::CONFLICT,"competition.effect.concurrent_update",
            "Connection effect changed during settlement"));
    }
    Ok(())
}

pub(super) async fn maintenance(state: &Arc<AppState>) -> ApiResult<()> {
    let root = native_chat::state_dir(state).join("control/competition/connection-outbox");
    fs::create_dir_all(&root).map_err(ApiError::internal)?;
    let mut pending = state.store.list(OUTBOX).map_err(ApiError::internal)?;
    pending.sort_by(|a,b|field(a,"created_at").cmp(field(b,"created_at")));
    for row in pending.into_iter().filter(due).take(100) {
        let outbox_id = field(&row,"outbox_id").to_owned();
        let path = root.join(format!("{outbox_id}.lock"));
        let lock = OpenOptions::new().create(true).read(true).write(true).open(path)
            .map_err(ApiError::internal)?;
        if lock.try_lock_exclusive().is_err() {continue;}
        if let Some(current) = state.store.get(OUTBOX,&outbox_id).map_err(ApiError::internal)? {
            if current["status"] == "processing" && current["op"] == "sync" {
                if let Some(output) = applied_sync_output(state,field(&current,"competition_id"),
                    field(&current,"command_id"))? {
                    settle(state,&current,Ok(output))?;
                    continue;
                }
            }
        }
        if let Some(claimed) = claim(state,&outbox_id)? {
            let result = deliver(state,&claimed).await;
            settle(state,&claimed,result)?;
        }
    }
    Ok(())
}

pub(super) fn is_effect(state: &AppState, effect_id: &str) -> ApiResult<bool> {
    let Some(effect) = state.store.get(EFFECTS,effect_id).map_err(ApiError::internal)? else {
        return Ok(false);
    };
    if effect["domain"] != "competition" {return Ok(false)}
    Ok(state.store.get(OUTBOX,field(&effect,"outbox_id"))
        .map_err(ApiError::internal)?.is_some())
}

fn exclusive_lock(state: &AppState, outbox_id: &str, retry: bool) -> ApiResult<std::fs::File> {
    let path = native_chat::state_dir(state).join("control/competition/connection-outbox")
        .join(format!("{outbox_id}.lock"));
    if let Some(parent) = path.parent() {fs::create_dir_all(parent).map_err(ApiError::internal)?;}
    let file = OpenOptions::new().create(true).read(true).write(true).open(path)
        .map_err(ApiError::internal)?;
    file.try_lock_exclusive().map_err(|error| {
        if error.kind() == std::io::ErrorKind::WouldBlock {
            ApiError::new(StatusCode::CONFLICT,
                if retry {"effect.retry_not_allowed"} else {"effect.irreversible"},
                "Remote delivery may have started")
        } else {ApiError::internal(error)}
    })?;
    Ok(file)
}

pub(super) async fn control_effect(state: &Arc<AppState>, envelope: &Value) -> ApiResult<Value> {
    let kind = field(envelope,"command_type");
    let requested = field(&envelope["payload"],"effect_id");
    let effect_id = if requested.is_empty() {field(envelope,"aggregate_id")} else {requested};
    let effect = state.store.get(EFFECTS,effect_id).map_err(ApiError::internal)?
        .ok_or_else(||ApiError::new(StatusCode::NOT_FOUND,"effect.not_found",
            "Competition effect not found"))?;
    let outbox_id = field(&effect,"outbox_id").to_owned();
    let retry = kind == "effect.retry";
    let _lock = exclusive_lock(state,&outbox_id,retry)?;
    let effect = state.store.get(EFFECTS,effect_id).map_err(ApiError::internal)?
        .ok_or_else(||ApiError::missing("effect receipt"))?;
    let outbox = state.store.get(OUTBOX,&outbox_id).map_err(ApiError::internal)?
        .ok_or_else(||ApiError::new(StatusCode::CONFLICT,"effect.outbox_missing",
            "Competition effect outbox is missing"))?;
    let status = field(&outbox,"status");
    if retry && matches!(status,"processing" | "delivered" | "cancelled") {
        return Err(ApiError::new(StatusCode::CONFLICT,"effect.retry_not_allowed",
            "Remote delivery may have started"));
    }
    if !retry {
        if envelope["payload"]["confirm"] != true {
            return Err(ApiError::new(StatusCode::BAD_REQUEST,"effect.cancel_confirmation_required",
                "effect.cancel requires payload.confirm=true"));
        }
        if matches!(status,"processing" | "delivered") {
            return Err(ApiError::new(StatusCode::CONFLICT,"effect.irreversible",
                "Remote delivery may have started"));
        }
    }
    let command_id = field(envelope,"command_id");
    let original_command_id = field(&effect,"command_id");
    let target = if retry {"waiting"} else {"cancelled"};
    let event = leases::planned_event(field(&outbox,"competition_id"),"effect",effect_id,
        if retry {"competition.effect.manual_retry_requested"}
            else {"competition.effect.cancel_requested"},command_id,
        json!({"effect_id":effect_id,"original_command_id":original_command_id,
            "outbox_id":outbox_id,"from":effect["state"],"to":target}));
    let accepted = accepted(command_id,"effect",effect_id);
    let planned = json!({"request":envelope,"response":{"receipt":accepted}});
    if !put_records_with_events(state,field(&outbox,"competition_id"),
        &[(COMMANDS,command_id,None)],&[(COMMANDS,command_id,&planned)],&[event])? {
        let prior = state.store.get(COMMANDS,command_id).map_err(ApiError::internal)?
            .ok_or_else(||ApiError::missing("effect command receipt"))?;
        if prior["request"] != *envelope {
            return Err(ApiError::new(StatusCode::CONFLICT,"command.idempotency_conflict",
                "Effect command ID was used with different input"));
        }
        let mut receipt = prior["response"]["receipt"].clone();
        receipt["deduplicated"] = json!(true);
        return Ok(receipt);
    }
    let mut next_outbox = outbox.clone();
    next_outbox["status"] = json!(if retry {"pending"} else {"cancelled"});
    next_outbox["next_attempt_at"] = if retry {json!(now())} else {Value::Null};
    next_outbox["last_error"] = if retry {Value::Null}
        else {json!("cancelled by operator before delivery")};
    next_outbox["delivered_at"] = Value::Null;
    next_outbox["updated_at"] = json!(now());
    let mut next_effect = effect.clone();
    next_effect["state"] = json!(target);
    next_effect["retry_after"] = Value::Null;
    next_effect["error"] = Value::Null;
    next_effect["completed_at"] = if retry {Value::Null} else {json!(now())};
    next_effect["updated_at"] = json!(now());
    let original = state.store.get(COMMANDS,original_command_id).map_err(ApiError::internal)?;
    let mut updated_original = original.clone().unwrap_or(Value::Null);
    if original.is_some() {
        let receipt = &mut updated_original["response"]["receipt"];
        receipt["state"] = json!(target);
        receipt["completed_at"] = if retry {Value::Null} else {json!(now())};
        receipt["error"] = Value::Null;
        receipt["effect_ids"] = json!([effect_id]);
        let mut output = receipt["output"].as_object().cloned().unwrap_or_default();
        output.insert("effect_state".to_owned(),json!(target));
        output.insert("effect_ids".to_owned(),json!([effect_id]));
        output.insert("outbox_ids".to_owned(),json!([outbox_id]));
        receipt["output"] = Value::Object(output);
    }
    let mut control = state.store.get(COMMANDS,command_id).map_err(ApiError::internal)?
        .ok_or_else(||ApiError::missing("effect command receipt"))?;
    let control_before = control.clone();
    control["response"]["receipt"]["state"] = json!("completed");
    control["response"]["receipt"]["completed_at"] = json!(now());
    control["response"]["receipt"]["output"] = json!({
        "effect":next_effect,"outbox_status":next_outbox["status"],
        "original_command_id":original_command_id});
    let mut expected = vec![(OUTBOX,outbox_id.as_str(),Some(&outbox)),
        (EFFECTS,effect_id,Some(&effect)),(COMMANDS,command_id,Some(&control_before))];
    let mut records = vec![(OUTBOX,outbox_id.as_str(),&next_outbox),
        (EFFECTS,effect_id,&next_effect),(COMMANDS,command_id,&control)];
    if let Some(original) = original.as_ref() {
        expected.push((COMMANDS,original_command_id,Some(original)));
        records.push((COMMANDS,original_command_id,&updated_original));
    }
    if !state.store.put_many_if(&expected,&records).map_err(ApiError::internal)? {
        return Err(ApiError::new(StatusCode::CONFLICT,"competition.effect.concurrent_update",
            "Competition effect changed during manual control"));
    }
    Ok(control["response"]["receipt"].clone())
}

#[cfg(test)]
mod tests {
    use super::classify_connection_failure;
    use axum::http::StatusCode;

    #[test]
    fn permission_denial_stays_separate_from_auth() {
        let permission = classify_connection_failure(
            StatusCode::FORBIDDEN,
            "competition.platform_permission",
            1,
        );
        assert!(!permission.pause_for_auth);
        assert!(permission.terminal);
        assert_eq!(permission.category, "permission");
        assert!(!permission.retryable);

        let auth = classify_connection_failure(
            StatusCode::UNAUTHORIZED,
            "competition.auth_required",
            1,
        );
        assert!(auth.pause_for_auth);
        assert!(!auth.terminal);
        assert_eq!(auth.category, "auth");
    }
}
