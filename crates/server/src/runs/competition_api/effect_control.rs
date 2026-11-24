//! Manual control of Competition submission and RunBinding effects.
use super::*;

const EFFECTS: &str = "platform_effect_receipts";
const RESULTS: &str = "competition_effect_control_results";
const SUBMISSION_OUTBOX: &str = "competition_submission_outbox_live";
const BINDING_OUTBOX: &str = "competition_binding_outbox_live";
const VERSIONS: &str = "platform_aggregate_versions";

fn requested_effect_id(envelope: &Value) -> &str {
    let requested = field(&envelope["payload"], "effect_id");
    if requested.is_empty() { field(envelope, "aggregate_id") } else { requested }
}

fn outbox_for(state: &AppState, effect: &Value) -> ApiResult<Option<(&'static str, Value)>> {
    let outbox_id = field(effect, "outbox_id");
    for collection in [SUBMISSION_OUTBOX, BINDING_OUTBOX] {
        if let Some(outbox) = state.store.get(collection, outbox_id).map_err(ApiError::internal)? {
            return Ok(Some((collection, outbox)));
        }
    }
    Ok(None)
}

pub(super) fn is_effect(state: &AppState, effect_id: &str) -> ApiResult<bool> {
    let Some(effect) = state.store.get(EFFECTS, effect_id).map_err(ApiError::internal)? else {
        return Ok(false);
    };
    Ok(effect["domain"] == "competition"
        && matches!(field(&effect["aggregate"], "type"), "platform_submission" | "run_binding"))
}

fn lock_name(collection: &str, outbox: &Value) -> String {
    if collection == SUBMISSION_OUTBOX {
        field(outbox, "submission_id").to_owned()
    } else {
        format!("binding-outbox-{}", field(outbox, "outbox_id"))
    }
}

fn check(kind: &str, payload: &Value, outbox: &Value) -> ApiResult<()> {
    let status = field(outbox, "status");
    if kind == "effect.retry" {
        if matches!(status, "processing" | "delivered" | "cancelled") {
            return Err(ApiError::new(StatusCode::CONFLICT, "effect.retry_not_allowed",
                format!("effect is {status}; remote delivery may have started")));
        }
    } else {
        if payload["confirm"] != true {
            return Err(ApiError::new(StatusCode::BAD_REQUEST,
                "effect.cancel_confirmation_required", "effect.cancel requires payload.confirm=true"));
        }
        if matches!(status, "processing" | "delivered") {
            return Err(ApiError::new(StatusCode::CONFLICT, "effect.irreversible",
                format!("effect is {status}; cancellation is no longer safe")));
        }
    }
    Ok(())
}

fn pending(state: &AppState, command_id: &str) -> ApiResult<Value> {
    let mut receipt = state.store.get(COMMANDS, command_id).map_err(ApiError::internal)?
        .ok_or_else(||ApiError::missing("effect command receipt"))?["response"]["receipt"].clone();
    receipt["error"] = json!({"code":"command.result_pending",
        "message":"Competition effect local result needs recovery",
        "category":"state","retryable":true});
    Ok(receipt)
}

fn stage(state: &AppState, envelope: &Value, effect: &Value,
    collection: &str, outbox: &Value) -> ApiResult<bool> {
    let command_id = field(envelope, "command_id");
    let effect_id = field(effect, "effect_id");
    let competition_id = field(outbox, "competition_id");
    let kind = field(envelope, "command_type");
    let target = if kind == "effect.retry" {"waiting"} else {"cancelled"};
    let event = leases::planned_event(competition_id,"effect",effect_id,
        if kind == "effect.retry" {"competition.effect.manual_retry_requested"}
        else {"competition.effect.cancel_requested"},command_id,
        json!({"effect_id":effect_id,"original_command_id":effect["command_id"],
            "outbox_id":outbox["outbox_id"],"from":effect["state"],"to":target}));
    let admission = &envelope["_planned_admission"];
    let expected_version = admission["expected_version"].as_u64();
    let version_key = format!("effect\0{effect_id}");
    let version = if expected_version.is_some() {
        state.store.get(VERSIONS,&version_key).map_err(ApiError::internal)?
    } else {None};
    if let Some(expected) = expected_version {
        let actual = crate::operations_api::current_aggregate_version(state,"effect",effect_id)
            .map_err(ApiError::internal)?;
        if actual != expected {
            return Err(ApiError::new(StatusCode::CONFLICT,"command.version_conflict",
                format!("Expected version {expected}, current version {actual}")));
        }
    }
    let receipt = json!({"schema_version":1,"command_id":command_id,"receipt_id":id("rcpt"),
        "state":"accepted","run_id":null,"aggregate":{"type":"effect","id":effect_id},
        "event_cursor":null,"deduplicated":false,"submitted_at":now(),"completed_at":null,
        "next":["receipt","snapshot","events","wait"],"output":{},"effect_ids":[],"error":null});
    let mut request = envelope.clone();
    request.as_object_mut().unwrap().remove("_planned_admission");
    request.as_object_mut().unwrap().remove("_verified_scope");
    let mut command = json!({"request":request,"plan":{"effect_id":effect_id,
        "outbox_collection":collection,"outbox_id":outbox["outbox_id"],
        "competition_id":competition_id,"cursor_scope":field(envelope,"_verified_scope")},
        "response":{"receipt":receipt}});
    let claim = admission.get("claim").filter(|value|value.is_object());
    if let Some(claim) = claim {
        command["request_fingerprint"] = claim["request_fingerprint"].clone();
        command["status"] = json!(StatusCode::ACCEPTED.as_u16());
    }
    let mut expected = vec![(COMMANDS,command_id,None)];
    let mut records = vec![(COMMANDS,command_id,&command)];
    if expected_version.is_some() {expected.push((VERSIONS,&version_key,version.as_ref()));}
    if let Some(claim) = claim {
        expected.push(("platform_command_claims",command_id,None));
        records.push(("platform_command_claims",command_id,claim));
        let key_id = field(admission,"key_id");
        if !key_id.is_empty() {
            expected.push(("platform_idempotency_keys",key_id,None));
            records.push(("platform_idempotency_keys",key_id,&admission["keyed"]));
        }
    }
    let committed = put_records_with_events(state,competition_id,&expected,&records,&[event])?;
    if !committed && expected_version.is_some() {
        let actual = crate::operations_api::current_aggregate_version(state,"effect",effect_id)
            .map_err(ApiError::internal)?;
        if Some(actual) != expected_version {
            return Err(ApiError::new(StatusCode::CONFLICT,"command.version_conflict",
                "Effect stream changed concurrently"));
        }
    }
    Ok(committed)
}

fn finish(state: &AppState, command_id: &str) -> ApiResult<Option<Value>> {
    let Some(result) = state.store.get(RESULTS, command_id).map_err(ApiError::internal)? else {
        return Ok(None);
    };
    loop {
        let Some(before) = state.store.get(COMMANDS, command_id).map_err(ApiError::internal)? else {
            return Ok(None);
        };
        if before["response"]["receipt"]["state"] != "accepted" {
            return Ok(Some(before["response"]["receipt"].clone()));
        }
        let mut after = before.clone();
        let receipt = &mut after["response"]["receipt"];
        receipt["state"] = result["state"].clone();
        receipt["output"] = result["output"].clone();
        receipt["error"] = result["error"].clone();
        receipt["completed_at"] = json!(now());
        receipt["next"] = json!([]);
        let effect_id = field(&before["plan"], "effect_id");
        if let Some(seq) = state.store.list(EVENTS).map_err(ApiError::internal)?.iter()
            .filter(|event| event["command_id"] == command_id
                && event["aggregate_type"] == "effect" && event["aggregate_id"] == effect_id)
            .filter_map(|event|event["global_seq"].as_u64()).max() {
            receipt["event_cursor"] = json!(crate::operations_api::competition_event_cursor(
                state, "effect", effect_id, seq, field(&before["plan"], "cursor_scope"))
                .map_err(ApiError::internal)?);
        }
        after["status"] = json!(crate::operations_api::planned_receipt_status(
            &after["response"]["receipt"]).as_u16());
        let claim = state.store.get("platform_command_claims", command_id)
            .map_err(ApiError::internal)?;
        let mut next_claim = claim.clone();
        if let Some(next) = next_claim.as_mut() {
            next["state"] = json!(if result["state"] == "completed" {"completed"} else {"failed"});
            next["updated_at"] = json!(now());
        }
        let mut expected = vec![(COMMANDS, command_id, Some(&before))];
        let mut records = vec![(COMMANDS, command_id, &after)];
        if let (Some(prior), Some(next)) = (claim.as_ref(), next_claim.as_ref()) {
            expected.push(("platform_command_claims", command_id, Some(prior)));
            records.push(("platform_command_claims", command_id, next));
        }
        if state.store.put_many_if(&expected, &records).map_err(ApiError::internal)? {
            return Ok(Some(after["response"]["receipt"].clone()));
        }
    }
}

pub(super) fn settle(state: &AppState, command_id: &str) -> ApiResult<Option<Value>> {
    finish(state, command_id)
}

fn record_failure(state: &AppState, command_id: &str, error: &ApiError) -> ApiResult<Value> {
    let status = if error.status == StatusCode::CONFLICT {"conflict"} else {"failed"};
    let result = json!({"command_id":command_id,"state":status,"output":{},
        "error":{"code":error.code,"message":error.message,
            "category":if status == "conflict" {"state"}
                else if error.status == StatusCode::BAD_GATEWAY {"internal"}
                else {"validation"},
            "retryable":false},"created_at":now()});
    state.store.put_many_if(&[(RESULTS,command_id,None)],&[(RESULTS,command_id,&result)])
        .map_err(ApiError::internal)?;
    Ok(finish(state, command_id)?.unwrap_or(pending(state, command_id)?))
}

fn replay(state: &AppState, envelope: &Value) -> ApiResult<Option<Value>> {
    let admission = &envelope["_planned_admission"];
    let command_id = field(envelope,"command_id");
    let fingerprint = field(&admission["claim"],"request_fingerprint");
    let mut stored = state.store.get(COMMANDS,command_id).map_err(ApiError::internal)?;
    if let Some(prior) = stored.as_ref() {
        if !fingerprint.is_empty() && field(prior,"request_fingerprint") != fingerprint {
            return Err(ApiError::new(StatusCode::CONFLICT,"command.idempotency_conflict",
                "Command ID was used with different content"));
        }
    } else {
        let key_id = field(admission,"key_id");
        if !key_id.is_empty() {
            if let Some(keyed) = state.store.get("platform_idempotency_keys",key_id)
                .map_err(ApiError::internal)? {
                if keyed["semantic_fingerprint"] != admission["semantic_fingerprint"] {
                    return Err(ApiError::new(StatusCode::CONFLICT,"command.idempotency_conflict",
                        "Idempotency key was used with different content"));
                }
                stored = state.store.get(COMMANDS,field(&keyed,"command_id"))
                    .map_err(ApiError::internal)?;
            }
        }
    }
    let Some(stored) = stored else {return Ok(None)};
    let mut receipt = stored["response"]["receipt"].clone();
    receipt["deduplicated"] = json!(true);
    Ok(Some(receipt))
}

fn apply(state: &AppState, command_id: &str) -> ApiResult<Value> {
    let command = state.store.get(COMMANDS,command_id).map_err(ApiError::internal)?
        .ok_or_else(||ApiError::missing("effect command receipt"))?;
    if command["response"]["receipt"]["state"] != "accepted" {
        return Ok(command["response"]["receipt"].clone());
    }
    if state.store.get(RESULTS,command_id).map_err(ApiError::internal)?.is_some() {
        return Ok(finish(state,command_id)?.unwrap_or(pending(state,command_id)?));
    }
    let kind = field(&command["request"],"command_type");
    let effect_id = field(&command["plan"],"effect_id");
    let collection = field(&command["plan"],"outbox_collection");
    let outbox_id = field(&command["plan"],"outbox_id");
    let effect = state.store.get(EFFECTS,effect_id).map_err(ApiError::internal)?
        .ok_or_else(||ApiError::missing("effect receipt"))?;
    let outbox = state.store.get(collection,outbox_id).map_err(ApiError::internal)?
        .ok_or_else(||ApiError::missing("effect outbox"))?;
    let submission_id = if collection == SUBMISSION_OUTBOX && field(&outbox,"op") == "submit" {
        field(&outbox,"submission_id")
    } else {""};
    let submission = if submission_id.is_empty() {None} else {
        state.store.get(SUBMISSIONS,submission_id).map_err(ApiError::internal)?
    };
    if let Err(error) = check(kind,&command["request"]["payload"],&outbox) {
        return record_failure(state,command_id,&error);
    }
    if kind == "effect.cancel" && let Some(submission) = submission.as_ref() {
        let from = field(submission,"state");
        if from != "cancelled" && !matches!(from,
            "queued" | "rate_limited" | "transient_failure" | "auth_required" | "unknown") {
            return record_failure(state,command_id,&ApiError::new(StatusCode::BAD_GATEWAY,
                "command.side_effect_failed",
                format!("submission cannot move from {from} to cancelled")));
        }
    }
    let retry = kind == "effect.retry";
    let mut next_outbox = outbox.clone();
    if retry {
        if outbox["status"] != "pending" {
            next_outbox["status"] = json!("pending");
            next_outbox["next_attempt_at"] = json!(now());
            next_outbox["last_error"] = Value::Null;
            next_outbox["delivered_at"] = Value::Null;
        }
    } else if outbox["status"] != "cancelled" {
        next_outbox["status"] = json!("cancelled");
        next_outbox["last_error"] = json!("cancelled by operator before delivery");
    }
    next_outbox["updated_at"] = json!(now());
    let mut next_effect = effect.clone();
    let target = if retry {"waiting"} else {"cancelled"};
    next_effect["state"] = json!(target);
    next_effect["attempts"] = next_outbox["attempts"].clone();
    next_effect["retry_after"] = Value::Null;
    next_effect["error"] = Value::Null;
    next_effect["completed_at"] = if retry {Value::Null} else {json!(now())};
    next_effect["updated_at"] = json!(now());
    let original_id = field(&effect,"command_id");
    let original = state.store.get(COMMANDS,original_id).map_err(ApiError::internal)?;
    let next_original = original.as_ref().map(|prior| {
        let mut next = prior.clone();
        let receipt = &mut next["response"]["receipt"];
        receipt["state"] = json!(target);
        receipt["completed_at"] = if retry {Value::Null} else {json!(now())};
        receipt["error"] = Value::Null;
        receipt["effect_ids"] = json!([effect_id]);
        let mut output = receipt["output"].as_object().cloned().unwrap_or_default();
        output.insert("effect_state".to_owned(),json!(target));
        output.insert("effect_ids".to_owned(),json!([effect_id]));
        output.insert("outbox_ids".to_owned(),json!([outbox_id]));
        receipt["output"] = Value::Object(output);
        next["status"] = json!(crate::operations_api::planned_receipt_status(receipt).as_u16());
        next
    });
    let original_claim = state.store.get("platform_command_claims",original_id)
        .map_err(ApiError::internal)?;
    let next_original_claim = original_claim.as_ref().map(|prior| {
        let mut next = prior.clone();
        next["state"] = json!(if retry {"accepted"} else {"failed"});
        next["updated_at"] = json!(now());
        next
    });
    let next_submission = submission.as_ref().and_then(|prior| {
        if !retry && prior["state"] != "cancelled" {
            let mut next = prior.clone();
            next["state"] = json!("cancelled");
            next["cancel_reason"] = json!("effect_cancelled");
            next["updated_at"] = json!(now());
            Some(next)
        } else {None}
    });
    let result = json!({"command_id":command_id,"state":"completed",
        "output":{"effect":next_effect,"outbox_status":next_outbox["status"],
            "original_command_id":original_id},"error":null,"created_at":now()});
    let mut expected = vec![(RESULTS,command_id,None),(collection,outbox_id,Some(&outbox)),
        (EFFECTS,effect_id,Some(&effect))];
    let mut records = vec![(RESULTS,command_id,&result),(collection,outbox_id,&next_outbox),
        (EFFECTS,effect_id,&next_effect)];
    if let (Some(prior),Some(next)) = (original.as_ref(),next_original.as_ref()) {
        expected.push((COMMANDS,original_id,Some(prior)));
        records.push((COMMANDS,original_id,next));
    }
    if let (Some(prior),Some(next)) = (original_claim.as_ref(),next_original_claim.as_ref()) {
        expected.push(("platform_command_claims",original_id,Some(prior)));
        records.push(("platform_command_claims",original_id,next));
    }
    let mut events = Vec::new();
    if let (Some(prior),Some(next)) = (submission.as_ref(),next_submission.as_ref()) {
        expected.push((SUBMISSIONS,submission_id,Some(prior)));
        records.push((SUBMISSIONS,submission_id,next));
        events.push(leases::planned_event(field(&outbox,"competition_id"),
            "platform_submission",submission_id,"competition.platform_submission.state_changed","",
            json!({"submission_id":submission_id,"candidate_id":prior["candidate_id"],
                "challenge_id":prior["competition_challenge_id"],"from":prior["state"],
                "to":"cancelled","attempt":prior["attempt"],"reason":"effect_cancelled"})));
    }
    if !put_records_with_events(state,field(&outbox,"competition_id"),
        &expected,&records,&events)? {
        return pending(state,command_id);
    }
    Ok(finish(state,command_id)?.unwrap_or(pending(state,command_id)?))
}

pub(super) async fn control(state: &Arc<AppState>, envelope: &Value) -> ApiResult<Value> {
    let effect_id = requested_effect_id(envelope);
    let effect = state.store.get(EFFECTS,effect_id).map_err(ApiError::internal)?
        .filter(|effect|effect["domain"] == "competition")
        .ok_or_else(||ApiError::new(StatusCode::NOT_FOUND,"effect.not_found",
            format!("competition effect not found: {effect_id}")))?;
    let (collection,outbox) = outbox_for(state,&effect)?
        .ok_or_else(||ApiError::new(StatusCode::CONFLICT,"effect.outbox_missing",
            format!("outbox record missing: {}",field(&effect,"outbox_id"))))?;
    let _lock = submission_outbox::submission_lock(state,&lock_name(collection,&outbox)).await?;
    let effect = state.store.get(EFFECTS,effect_id).map_err(ApiError::internal)?
        .ok_or_else(||ApiError::missing("effect receipt"))?;
    let outbox = state.store.get(collection,field(&effect,"outbox_id"))
        .map_err(ApiError::internal)?.ok_or_else(||ApiError::missing("effect outbox"))?;
    let command_id = field(envelope,"command_id");
    if let Some(receipt) = replay(state,envelope)? {
        if receipt["command_id"] == command_id && receipt["state"] == "accepted" {
            let mut applied = apply(state,command_id)?;
            applied["deduplicated"] = json!(true);
            return Ok(applied);
        }
        return Ok(receipt);
    }
    check(field(envelope,"command_type"),&envelope["payload"],&outbox)?;
    if !stage(state,envelope,&effect,collection,&outbox)? {
        return replay(state,envelope)?.ok_or_else(||ApiError::new(StatusCode::CONFLICT,
            "competition.effect.concurrent_update","Effect command changed concurrently"));
    }
    match apply(state,command_id) {
        Ok(receipt) => Ok(receipt),
        Err(error) => {
            tracing::warn!(command_id,code=%error.code,"Competition effect local result needs recovery");
            pending(state,command_id)
        }
    }
}

pub(super) async fn reconcile(state: &Arc<AppState>) -> ApiResult<()> {
    for command in state.store.list(COMMANDS).map_err(ApiError::internal)? {
        let kind = field(&command["request"],"command_type");
        if !matches!(kind,"effect.retry" | "effect.cancel")
            || command["response"]["receipt"]["state"] != "accepted"
            || !command["plan"]["outbox_collection"].is_string() {continue;}
        let command_id = field(&command["response"]["receipt"],"command_id").to_owned();
        let collection = field(&command["plan"],"outbox_collection");
        let outbox_id = field(&command["plan"],"outbox_id");
        let Some(outbox) = state.store.get(collection,outbox_id).map_err(ApiError::internal)? else {continue;};
        let _lock = submission_outbox::submission_lock(state,&lock_name(collection,&outbox)).await?;
        if let Err(error) = apply(state,&command_id) {
            tracing::warn!(command_id,code=%error.code,"Competition effect control recovery pending");
        }
    }
    Ok(())
}
