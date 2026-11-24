//! Policy and scheduler CommandAPI plans, local effects, and result recovery.
use super::*;

const RESULTS: &str = "competition_control_effect_results";
const VERSIONS: &str = "platform_aggregate_versions";

fn plan_failed(message: impl Into<String>) -> ApiError {
    ApiError::new(StatusCode::BAD_GATEWAY,"command.plan_failed",message)
}

fn python_string(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Bool(true) => "True".to_owned(),
        Value::Bool(false) => "False".to_owned(),
        Value::Null => "None".to_owned(),
        _ => value.to_string(),
    }
}

fn python_int(value: &Value) -> ApiResult<i64> {
    match value {
        Value::Bool(value) => Ok(i64::from(*value)),
        Value::Number(number) => number.as_i64().or_else(||number.as_f64().map(|value|value as i64))
            .ok_or_else(||plan_failed("invalid integer policy value")),
        Value::String(text) => text.trim().parse::<i64>()
            .map_err(|_|plan_failed("invalid integer policy value")),
        _ => Err(plan_failed("invalid integer policy value")),
    }
}

fn python_float(value: &Value) -> ApiResult<f64> {
    match value {
        Value::Bool(value) => Ok(if *value {1.0} else {0.0}),
        Value::Number(number) => number.as_f64().ok_or_else(||plan_failed("invalid numeric policy value")),
        Value::String(text) => text.trim().parse::<f64>()
            .map_err(|_|plan_failed("invalid numeric policy value")),
        _ => Err(plan_failed("invalid numeric policy value")),
    }
}

fn python_truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(value) => *value,
        Value::Number(value) => value.as_f64().is_some_and(|value|value != 0.0),
        Value::String(value) => !value.is_empty(),
        Value::Array(value) => !value.is_empty(),
        Value::Object(value) => !value.is_empty(),
    }
}

fn sequence(value: &Value) -> ApiResult<Vec<Value>> {
    match value {
        Value::Array(values) => Ok(values.clone()),
        Value::String(value) => Ok(value.chars().map(|letter|json!(letter.to_string())).collect()),
        Value::Object(values) => Ok(values.keys().map(|key|json!(key)).collect()),
        _ => Err(plan_failed("invalid policy list value")),
    }
}

fn policy_changes(payload: &Value) -> ApiResult<Value> {
    let mut changes = serde_json::Map::new();
    let mode = payload.get("automation_mode").filter(|value|python_truthy(value))
        .map(python_string).unwrap_or_default().trim().to_owned();
    if !mode.is_empty() {
        if !matches!(mode.as_str(),"observe" | "assisted" | "autonomous") {
            return Err(ApiError::new(StatusCode::BAD_REQUEST,"competition.policy.mode_unsupported",
                format!("automation_mode must be observe, assisted or autonomous: {mode:?}")));
        }
        changes.insert("automation_mode".to_owned(),json!(mode));
    }
    for key in ["max_concurrent_runs","max_instances","working_set","keepalive_max",
        "dry_defer_waves","deepchain_slots","stuck_waves_cap"] {
        if let Some(value) = payload.get(key).filter(|value|!value.is_null()) {
            changes.insert(key.to_owned(),json!(python_int(value)?));
        }
    }
    for key in ["fill_idle_revisits","terminal_phase_fill_idle"] {
        if let Some(value) = payload.get(key).filter(|value|!value.is_null()) {
            changes.insert(key.to_owned(),json!(python_truthy(value)));
        }
    }
    for key in ["submission_cooldown_seconds","visit_floor_s","overdue_mult",
        "keepalive_tail_s","total_budget_s","per_challenge_seconds"] {
        if let Some(value) = payload.get(key).filter(|value|!value.is_null()) {
            changes.insert(key.to_owned(),json!(python_float(value)?));
        }
    }
    if let Some(value) = payload.get("policy_profile").filter(|value|!value.is_null()) {
        let profile = if python_truthy(value) {python_string(value).trim().to_owned()}
            else {String::new()};
        let profile = if profile.is_empty() {"default"} else {profile.as_str()};
        if !matches!(profile,"default" | "tsec_eval") {
            return Err(ApiError::new(StatusCode::BAD_REQUEST,"competition.policy.profile_unsupported",
                format!("policy_profile must be default or tsec_eval: {profile:?}")));
        }
        changes.insert("policy_profile".to_owned(),json!(profile));
        let defaults = if profile == "tsec_eval" {tsec_eval_defaults()} else {json!({})};
        for (key,value) in defaults.as_object().into_iter().flatten() {
            if key != "policy_profile" && changes.get(key).is_none()
                && payload.get(key).is_none_or(Value::is_null) {
                changes.insert(key.clone(),value.clone());
            }
        }
    }
    if let Some(value) = payload.get("round_timeboxes_s").filter(|value|!value.is_null()) {
        changes.insert("round_timeboxes_s".to_owned(),json!(sequence(value)?.iter()
            .map(python_int).collect::<ApiResult<Vec<_>>>()?));
    }
    for key in ["category_allow","category_deny"] {
        if let Some(value) = payload.get(key).filter(|value|!value.is_null()) {
            changes.insert(key.to_owned(),json!(sequence(value)?.iter()
                .map(python_string).collect::<Vec<_>>()));
        }
    }
    for key in ["challenge_order","terminal_phase_challenge_ids","persistent_challenge_ids"] {
        if let Some(value) = payload.get(key).filter(|value|!value.is_null()) {
            changes.insert(key.to_owned(),json!(sequence(value)?.iter()
                .map(python_string).map(|value|value.trim().to_owned())
                .filter(|value|!value.is_empty()).collect::<Vec<_>>()));
        }
    }
    if let Some(value) = payload.get("budget_limits").filter(|value|!value.is_null()) {
        let mut budgets = serde_json::Map::new();
        if let Some(values) = value.as_object() {
            for (key,value) in values {budgets.insert(key.clone(),json!(python_float(value)?));}
        } else if let Some(values) = value.as_array() {
            for pair in values {
                let items = pair.as_array().ok_or_else(||plan_failed("invalid budget_limits policy value"))?;
                if items.len() != 2 {return Err(plan_failed("invalid budget_limits policy value"));}
                budgets.insert(python_string(&items[0]),json!(python_float(&items[1])?));
            }
        } else {return Err(plan_failed("invalid budget_limits policy value"));}
        changes.insert("budget_limits".to_owned(),Value::Object(budgets));
    }
    Ok(Value::Object(changes))
}

fn accepted(command_id: &str, competition_id: &str) -> Value {
    json!({"schema_version":1,"command_id":command_id,"receipt_id":id("rcpt"),
        "state":"accepted","run_id":null,"aggregate":{"type":"competition","id":competition_id},
        "event_cursor":null,"deduplicated":false,"submitted_at":now(),"completed_at":null,
        "next":["receipt","snapshot","events","wait"],"output":{},"effect_ids":[],"error":null})
}

pub(super) fn finish(state: &AppState, command_id: &str) -> ApiResult<Value> {
    let result = state.store.get(RESULTS,command_id).map_err(ApiError::internal)?
        .ok_or_else(||ApiError::missing("control effect result"))?;
    loop {
        let previous = state.store.get(COMMANDS,command_id).map_err(ApiError::internal)?
            .ok_or_else(||ApiError::missing("control command receipt"))?;
        if previous["response"]["receipt"]["state"] != "accepted" {
            return Ok(previous["response"]["receipt"].clone());
        }
        let mut updated = previous.clone();
        let receipt = &mut updated["response"]["receipt"];
        receipt["state"] = result["state"].clone();
        receipt["output"] = result["output"].clone();
        receipt["error"] = result["error"].clone();
        receipt["completed_at"] = json!(now());
        let competition_id = field(&previous["plan"],"competition_id");
        if let Some(seq) = state.store.list(EVENTS).map_err(ApiError::internal)?.iter()
            .filter(|event|event["command_id"] == command_id
                && event["aggregate_type"] == "competition" && event["aggregate_id"] == competition_id)
            .filter_map(|event|event["global_seq"].as_u64()).max() {
            receipt["event_cursor"] = json!(crate::operations_api::competition_event_cursor(
                state,"competition",competition_id,seq,field(&previous["plan"],"cursor_scope"))
                .map_err(ApiError::internal)?);
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
    let mut output=json!({});
    if status=="completed" {
        let command=state.store.get(COMMANDS,command_id).map_err(ApiError::internal)?.ok_or_else(||ApiError::missing("control command receipt"))?;
        if matches!(field(&command["request"],"command_type"),"policy.update"|"competition.policy.update") {
            let payload=&command["request"]["payload"];
            let warnings=["keepalive_tail_s","stuck_waves_cap"].into_iter()
                .filter(|key|payload.get(*key).is_some_and(|value|!value.is_null())||payload["policy_profile"]=="tsec_eval")
                .map(|key|json!({"code":"competition.policy.field_inactive","field":key,"message":"该兼容字段已保存，当前运行时未使用此参数"})).collect::<Vec<_>>();
            if !warnings.is_empty(){output["warnings"]=json!(warnings);}
        }
    }
    state.store.put(RESULTS,command_id,&json!({"command_id":command_id,
        "state":status,"output":output,"error":error,"created_at":now()})).map_err(ApiError::internal)?;
    finish(state,command_id)
}

fn apply(state: &AppState, command_id: &str) -> ApiResult<Value> {
    let command = state.store.get(COMMANDS,command_id).map_err(ApiError::internal)?
        .ok_or_else(||ApiError::missing("control command receipt"))?;
    if command["response"]["receipt"]["state"] != "accepted" {
        return Ok(command["response"]["receipt"].clone());
    }
    if state.store.get(RESULTS,command_id).map_err(ApiError::internal)?.is_some() {
        return finish(state,command_id);
    }
    let plan = &command["plan"];
    let collection = field(plan,"collection");
    let competition_id = field(plan,"competition_id");
    let before = &plan["before"];
    let after = &plan["after"];
    let current = state.store.get(collection,competition_id).map_err(ApiError::internal)?;
    if current.as_ref() == Some(after) {return journal(state,command_id,"completed",Value::Null);}
    if current.as_ref() != before.as_object().map(|_|before) {
        return journal(state,command_id,"failed",json!({"code":"command.side_effect_failed",
            "message":"control state changed before local effect was applied",
            "category":"state","retryable":false}));
    }
    match state.store.put_many_if(&[(collection,competition_id,before.as_object().map(|_|before))],
        &[(collection,competition_id,after)]) {
        Ok(true) => journal(state,command_id,"completed",Value::Null),
        Ok(false) => apply(state,command_id),
        Err(error) => journal(state,command_id,"failed",json!({"code":"command.side_effect_failed",
            "message":error.to_string(),"category":"internal","retryable":true})),
    }
}

pub(super) fn execute(state: &Arc<AppState>, competition_id: &str,
    kind: &str, requested_command_id: &str, payload: &Value,
    expected_version: Option<u64>, cursor_scope: &str, admission: &Value) -> ApiResult<Value> {
    if payload.get("competition_id").filter(|value|python_truthy(value))
        .map(python_string).is_none_or(|value|value.trim().is_empty()) {
        let prefix = if matches!(kind,"policy.update" | "competition.policy.update") {
            "competition.policy.competition_id_required"
        } else {"competition.scheduler.competition_id_required"};
        return Err(ApiError::new(StatusCode::BAD_REQUEST,prefix,
            format!("{kind} requires payload.competition_id")));
    }
    let command_id = if requested_command_id.is_empty() {id("cmd")} else {requested_command_id.to_owned()};
    let request = json!({"command_type":kind,"competition_id":competition_id,
        "payload":payload,"expected_version":expected_version});
    loop {
        if let Some(previous) = state.store.get(COMMANDS,&command_id).map_err(ApiError::internal)? {
            if previous["request"] != request {
                return Err(ApiError::new(StatusCode::CONFLICT,"command.idempotency_conflict",
                    "command_id was used with different control content"));
            }
            if previous["response"]["receipt"]["state"] == "accepted" {let _ = apply(state,&command_id);}
            return prior_receipt(state,&command_id)?.ok_or_else(||ApiError::missing("control command receipt"));
        }
        if let Some(receipt) = planned_key_receipt(state,admission)? {return Ok(receipt);}
        if admission.is_object() && state.store.get("platform_command_claims",&command_id)
            .map_err(ApiError::internal)?.is_some() {
            return Err(ApiError::new(StatusCode::CONFLICT,"command.outcome_unknown",
                "Prior command outcome must be inspected before retry"));
        }
        let competition = state.store.get(COMPETITIONS,competition_id).map_err(ApiError::internal)?
            .ok_or_else(||ApiError::new(StatusCode::NOT_FOUND,"competition.competition.not_found",
                format!("unknown competition {competition_id:?}")))?;
        let (collection,before,after,event_type,event_payload) = if matches!(kind,"policy.update" | "competition.policy.update") {
            let previous = state.store.get(POLICIES,competition_id).map_err(ApiError::internal)?;
            let changes = policy_changes(payload)?;
            let mut policy = previous.clone().unwrap_or_else(||default_policy(competition_id));
            for (key,value) in changes.as_object().into_iter().flatten() {policy[key] = value.clone();}
            policy["updated_at"] = json!(now());
            (POLICIES,previous,policy,"competition.policy.updated",
                json!({"competition_id":competition_id,"changes":changes}))
        } else {
            let from = field(&competition,"scheduler_state");
            let (expected,to,event_type) = match kind {
                "scheduler.start" => ("stopped","running","competition.scheduler.started"),
                "scheduler.pause" => ("running","paused","competition.scheduler.paused"),
                _ => ("paused","running","competition.scheduler.resumed"),
            };
            if from != expected {
                return Err(ApiError::new(StatusCode::CONFLICT,"competition.scheduler.illegal_transition",
                    format!("{kind} requires scheduler_state {expected:?}, current is {from:?}")));
            }
            let mut updated = competition.clone();
            updated["scheduler_state"] = json!(to);
            updated["updated_at"] = json!(now());
            (COMPETITIONS,Some(competition.clone()),updated,event_type,
                json!({"competition_id":competition_id,"from":from,"to":to}))
        };
        let event = leases::planned_event(competition_id,"competition",competition_id,
            event_type,&command_id,event_payload);
        let version_key = format!("competition\0{competition_id}");
        let version = state.store.get(VERSIONS,&version_key).map_err(ApiError::internal)?;
        if expected_version.is_some_and(|expected|version.as_ref()
            .and_then(|row|row["version"].as_u64()).unwrap_or(0) != expected) {
            return Err(ApiError::new(StatusCode::CONFLICT,"command.version_conflict",
                "expected_version does not match competition stream"));
        }
        let plan = json!({"competition_id":competition_id,"collection":collection,
            "before":before,"after":after,"cursor_scope":cursor_scope});
        let mut stored = json!({"request":request,"plan":plan,
            "response":{"receipt":accepted(&command_id,competition_id)}});
        let claim = admission.get("claim").filter(|value|value.is_object());
        if let Some(claim) = claim {
            stored["request_fingerprint"] = claim["request_fingerprint"].clone();
            stored["status"] = json!(StatusCode::ACCEPTED.as_u16());
        }
        let mut expected = vec![(COMPETITIONS,competition_id,Some(&competition)),
            (VERSIONS,version_key.as_str(),version.as_ref()),
            (COMMANDS,command_id.as_str(),None)];
        if collection == POLICIES {expected.push((POLICIES,competition_id,before.as_ref()));}
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
        if !put_records_with_events(state,competition_id,&expected,
            &records,&[event])? {continue;}
        return apply(state,&command_id);
    }
}

pub(super) fn reconcile(state: &Arc<AppState>) -> ApiResult<()> {
    for command in state.store.list(COMMANDS).map_err(ApiError::internal)? {
        if !matches!(field(&command["request"],"command_type"),"policy.update" | "competition.policy.update"
            | "scheduler.start" | "scheduler.pause" | "scheduler.resume")
            || command["response"]["receipt"]["state"] != "accepted" {continue;}
        let command_id = field(&command["response"]["receipt"],"command_id");
        if let Err(error) = apply(state,command_id) {
            tracing::warn!(command_id,code=%error.code,"Control local effect recovery failed");
        }
    }
    Ok(())
}
