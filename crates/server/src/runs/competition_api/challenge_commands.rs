//! Source challenge select, queue, and skip plans with recoverable local effects.
use super::*;

const RESULTS: &str = "competition_challenge_effect_results";
const VERSIONS: &str = "platform_aggregate_versions";

fn canonical(kind: &str) -> &str {
    if kind == "competition.challenge.queue" { "challenge.queue" } else { kind }
}

fn load_challenge(state: &AppState, kind: &str, payload: &Value) -> ApiResult<Value> {
    let challenge_id = field(payload,"challenge_id");
    let challenge = if !challenge_id.is_empty() {
        state.store.get(CHALLENGES,challenge_id).map_err(ApiError::internal)?
    } else {
        let competition_id = field(payload,"competition_id");
        if competition_id.is_empty() {
            return Err(ApiError::new(StatusCode::BAD_REQUEST,
                "competition.challenge.competition_id_required",
                format!("{kind} requires payload.competition_id")));
        }
        let external_id = field(payload,"external_challenge_id");
        if external_id.is_empty() {
            return Err(ApiError::new(StatusCode::BAD_REQUEST,
                "competition.challenge.external_challenge_id_required",
                format!("{kind} requires payload.external_challenge_id")));
        }
        ordered_scoped(state,CHALLENGES,competition_id)?.into_iter()
            .find(|row|field(row,"external_challenge_id") == external_id)
    };
    let identifier = if challenge_id.is_empty() {field(payload,"external_challenge_id")} else {challenge_id};
    challenge.ok_or_else(||ApiError::new(StatusCode::NOT_FOUND,
        "competition.challenge.not_found",format!("unknown challenge {identifier:?}")))
}

fn transition_allowed(from: &str, to: &str, paused_from: &str) -> bool {
    matches!((from,to),
        ("discovered","selected" | "skipped")
        | ("selected","queued" | "skipped")
        | ("queued","queued" | "skipped")
        | ("paused","skipped"))
        || from == "paused" && to == paused_from && matches!(to,"selected" | "queued")
}

fn receipt(command_id: &str, challenge_id: &str) -> Value {
    json!({"schema_version":1,"command_id":command_id,"receipt_id":id("rcpt"),
        "state":"accepted","run_id":null,
        "aggregate":{"type":"challenge","id":challenge_id},
        "event_cursor":null,"deduplicated":false,"submitted_at":now(),
        "completed_at":null,"next":["receipt","snapshot","events","wait"],
        "output":{},"effect_ids":[],"error":null})
}

pub(super) fn finish(state: &AppState, command_id: &str) -> ApiResult<Value> {
    let result = state.store.get(RESULTS,command_id).map_err(ApiError::internal)?
        .ok_or_else(||ApiError::missing("challenge effect result"))?;
    loop {
        let previous = state.store.get(COMMANDS,command_id).map_err(ApiError::internal)?
            .ok_or_else(||ApiError::missing("challenge command receipt"))?;
        if previous["response"]["receipt"]["state"] != "accepted" {
            return Ok(previous["response"]["receipt"].clone());
        }
        let mut updated = previous.clone();
        let receipt = &mut updated["response"]["receipt"];
        receipt["state"] = result["state"].clone();
        receipt["output"] = result["output"].clone();
        receipt["error"] = result["error"].clone();
        receipt["completed_at"] = json!(now());
        let challenge_id = field(&previous["plan"],"challenge_id");
        let events = state.store.list(EVENTS).map_err(ApiError::internal)?;
        if let Some(last) = events.iter().filter(|event|event["command_id"] == command_id
            && event["aggregate_type"] == "challenge" && event["aggregate_id"] == challenge_id)
            .filter_map(|event|event["global_seq"].as_u64()).max() {
            let scope = field(&previous["plan"],"cursor_scope");
            receipt["event_cursor"] = json!(crate::operations_api::competition_event_cursor(
                state,"challenge",challenge_id,last,scope).map_err(ApiError::internal)?);
        }
        updated["status"] = json!(crate::operations_api::planned_receipt_status(
            &updated["response"]["receipt"]).as_u16());
        if state.store.put_many_if(&[(COMMANDS,command_id,Some(&previous))],
            &[(COMMANDS,command_id,&updated)]).map_err(ApiError::internal)? {
            return Ok(updated["response"]["receipt"].clone());
        }
    }
}

fn journal(state: &AppState, command_id: &str, status: &str, error: Value) -> ApiResult<Value> {
    let result = json!({"command_id":command_id,"state":status,
        "output":{},"error":error,"created_at":now()});
    state.store.put(RESULTS,command_id,&result).map_err(ApiError::internal)?;
    finish(state,command_id)
}

fn apply(state: &AppState, command_id: &str) -> ApiResult<Value> {
    let command = state.store.get(COMMANDS,command_id).map_err(ApiError::internal)?
        .ok_or_else(||ApiError::missing("challenge command receipt"))?;
    if command["response"]["receipt"]["state"] != "accepted" {
        return Ok(command["response"]["receipt"].clone());
    }
    if state.store.get(RESULTS,command_id).map_err(ApiError::internal)?.is_some() {
        return finish(state,command_id);
    }
    let plan = &command["plan"];
    let challenge_id = field(plan,"challenge_id");
    let before = &plan["challenge_before"];
    let after = &plan["challenge_after"];
    let queue_before = &plan["queue_before"];
    let queue_after = &plan["queue_after"];
    let current = state.store.get(CHALLENGES,challenge_id).map_err(ApiError::internal)?;
    let queue = state.store.get(QUEUE,challenge_id).map_err(ApiError::internal)?;
    if current.as_ref() == Some(after) && queue.as_ref() == queue_after.as_object().map(|_|queue_after) {
        return journal(state,command_id,"completed",Value::Null);
    }
    if current.as_ref() != Some(before)
        || queue.as_ref() != queue_before.as_object().map(|_|queue_before) {
        return journal(state,command_id,"failed",json!({"code":"command.side_effect_failed",
            "message":"challenge changed before local effect was applied",
            "category":"state","retryable":false}));
    }
    let mut expected = vec![(CHALLENGES,challenge_id,Some(before))];
    let mut records = vec![(CHALLENGES,challenge_id,after)];
    if queue_after != queue_before {
        expected.push((QUEUE,challenge_id,queue_before.as_object().map(|_|queue_before)));
        if queue_after.is_object() { records.push((QUEUE,challenge_id,queue_after)); }
    }
    match state.store.put_many_if(&expected,&records) {
        Ok(true) => journal(state,command_id,"completed",Value::Null),
        Ok(false) => apply(state,command_id),
        Err(error) => journal(state,command_id,"failed",json!({
            "code":"command.side_effect_failed","message":error.to_string(),
            "category":"internal","retryable":true})),
    }
}

pub(super) fn execute(state: &Arc<AppState>, route_competition_id: &str,
    kind: &str, requested_command_id: &str, payload: &Value,
    expected_version: Option<u64>, cursor_scope: &str, admission: &Value) -> ApiResult<Value> {
    let command_id = if requested_command_id.is_empty() { id("cmd") }
        else { requested_command_id.to_owned() };
    let request = json!({"command_type":kind,"competition_id":route_competition_id,
        "payload":payload,"expected_version":expected_version});
    loop {
        if let Some(previous) = state.store.get(COMMANDS,&command_id).map_err(ApiError::internal)? {
            if previous["request"] != request {
                return Err(ApiError::new(StatusCode::CONFLICT,"command.idempotency_conflict",
                    "command_id was used with different challenge content"));
            }
            if previous["response"]["receipt"]["state"] == "accepted" {
                let _ = apply(state,&command_id);
            }
            return prior_receipt(state,&command_id)?.ok_or_else(||ApiError::missing("challenge command receipt"));
        }
        if let Some(receipt) = planned_key_receipt(state,admission)? {return Ok(receipt);}
        if admission.is_object() && state.store.get("platform_command_claims",&command_id)
            .map_err(ApiError::internal)?.is_some() {
            return Err(ApiError::new(StatusCode::CONFLICT,"command.outcome_unknown",
                "Prior command outcome must be inspected before retry"));
        }
        let challenge = load_challenge(state,kind,payload)?;
        let challenge_id = field(&challenge,"challenge_id").to_owned();
        let competition_id = field(&challenge,"competition_id").to_owned();
        if challenge["tombstoned"] == true {
            return Err(ApiError::new(StatusCode::CONFLICT,"competition.challenge.tombstoned",
                format!("challenge {challenge_id} is tombstoned")));
        }
        let from = field(&challenge,"state").to_owned();
        let to = match canonical(kind) {
            "challenge.select" => "selected",
            "challenge.queue" => "queued",
            _ => "skipped",
        };
        if !transition_allowed(&from,to,field(&challenge,"paused_from")) {
            return Err(ApiError::new(StatusCode::CONFLICT,
                "competition.challenge.illegal_transition",
                format!("illegal challenge transition: {from} -> {to}")));
        }
        let mut updated = challenge.clone();
        updated["state"] = json!(to);
        updated["paused_from"] = Value::Null;
        updated["updated_at"] = json!(now());
        let queue_before = state.store.get(QUEUE,&challenge_id).map_err(ApiError::internal)?;
        let queue_after = if to == "queued" {
            let mut entry = queue_before.clone().unwrap_or_else(||json!({
                "competition_id":competition_id,"competition_challenge_id":challenge_id,
                "state":"queued","priority":0.0,"score":0.0,"not_before":null,
                "admission_decision":{},"enqueued_at":now(),"updated_at":now()}));
            entry["state"] = json!("queued");
            entry["priority"] = json!(payload["priority"].as_f64().unwrap_or(0.0));
            entry["updated_at"] = json!(now());
            Some(entry)
        } else if to == "skipped" {
            queue_before.as_ref().map(|entry| {
                if matches!(field(entry,"state"),"done" | "dropped") { return entry.clone(); }
                let mut updated = entry.clone();
                updated["state"] = json!("dropped");
                updated["updated_at"] = json!(now());
                updated
            })
        } else { queue_before.clone() };
        let plan = json!({"challenge_id":challenge_id,"challenge_before":challenge,
            "challenge_after":updated,"queue_before":queue_before,"queue_after":queue_after,
            "cursor_scope":cursor_scope});
        let mut events = vec![leases::planned_event(&competition_id,"challenge",&challenge_id,
            "competition.challenge.state_changed",&command_id,
            json!({"challenge_id":challenge_id,"from":from,"to":to}))];
        if to == "queued" {
            events.push(leases::planned_event(&competition_id,"challenge",&challenge_id,
                "competition.queue.enqueued",&command_id,
                json!({"challenge_id":challenge_id,"priority":plan["queue_after"]["priority"]})));
        }
        let version_key = format!("challenge\0{challenge_id}");
        let version = state.store.get(VERSIONS,&version_key).map_err(ApiError::internal)?;
        if expected_version.is_some_and(|expected|version.as_ref()
            .and_then(|row|row["version"].as_u64()).unwrap_or(0) != expected) {
            return Err(ApiError::new(StatusCode::CONFLICT,"command.version_conflict",
                "expected_version does not match challenge stream"));
        }
        let mut stored = json!({"request":request,"plan":plan,
            "response":{"receipt":receipt(&command_id,&challenge_id)}});
        let mut expected = vec![(CHALLENGES,challenge_id.as_str(),Some(&challenge)),
            (QUEUE,challenge_id.as_str(),queue_before.as_ref()),
            (VERSIONS,version_key.as_str(),version.as_ref()),
            (COMMANDS,command_id.as_str(),None)];
        let claim = admission.get("claim").filter(|value|value.is_object());
        if let Some(claim) = claim {
            stored["request_fingerprint"] = claim["request_fingerprint"].clone();
            stored["status"] = json!(StatusCode::ACCEPTED.as_u16());
        }
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
        if !put_records_with_events(state,&competition_id,&expected,
            &records,&events)? { continue; }
        return apply(state,&command_id);
    }
}

pub(super) fn reconcile(state: &Arc<AppState>) -> ApiResult<()> {
    for command in state.store.list(COMMANDS).map_err(ApiError::internal)? {
        if !matches!(field(&command["request"],"command_type"),
            "challenge.select" | "challenge.queue" | "competition.challenge.queue" | "challenge.skip")
            || command["response"]["receipt"]["state"] != "accepted" { continue; }
        let command_id = field(&command["response"]["receipt"],"command_id");
        if let Err(error) = apply(state,command_id) {
            tracing::warn!(command_id,code=%error.code,"Challenge local effect recovery failed");
        }
    }
    Ok(())
}
