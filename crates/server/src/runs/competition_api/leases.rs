//! Persisted dynamic-instance effects and lease recovery.
use super::*;

const OUTBOX: &str = "competition_lease_outbox_live";
const EFFECTS: &str = "platform_effect_receipts";
const VERSIONS: &str = "platform_aggregate_versions";
const HEADS: &str = "competition_lease_heads_live";
const CLOCKS: &str = "competition_lease_clocks_live";
const FENCING: &str = "competition_lease_fencing_live";

async fn lease_lock(state: &AppState, lease_id: &str) -> ApiResult<std::fs::File> {
    let path = native_chat::state_dir(state)
        .join("_platform")
        .join("lease-locks")
        .join(format!("{lease_id}.lock"));
    tokio::task::spawn_blocking(move || -> std::io::Result<std::fs::File> {
        std::fs::create_dir_all(path.parent().unwrap())?;
        let mut options = std::fs::OpenOptions::new();
        options.create(true).read(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let file = options.open(path)?;
        fs2::FileExt::lock_exclusive(&file)?;
        Ok(file)
    })
    .await
    .map_err(ApiError::internal)?
    .map_err(ApiError::internal)
}

fn occupies(lease: &Value) -> bool {
    matches!(
        field(lease, "state"),
        "requested" | "provisioning" | "active" | "renewing" | "releasing"
    )
}

fn active_for(
    state: &AppState,
    competition_id: &str,
    challenge_id: &str,
) -> ApiResult<Option<Value>> {
    Ok(scoped(state, LEASES, competition_id)?
        .into_iter()
        .find(|lease| lease["competition_challenge_id"] == challenge_id && occupies(lease)))
}

fn conflict(code: &'static str, message: &'static str) -> ApiError {
    ApiError::new(StatusCode::CONFLICT, code, message)
}

fn receipt(state: &AppState, command_id: &str, kind: &str, lease: &Value) -> ApiResult<Value> {
    let output = if kind == "instance.stop" {
        json!({"lease_id":lease["lease_id"],"state":lease["state"]})
    } else {
        json!({"lease_id":lease["lease_id"],"state":lease["state"],"address":lease["address"],
            "fencing_token":lease["fencing_token"],"generation":lease["generation"]})
    };
    save_receipt(
        state,
        command_id,
        kind,
        "instance_lease",
        field(lease, "lease_id"),
        output,
    )
}

fn effect_id(outbox: &Value) -> String {
    format!("effect-{}", field(outbox, "outbox_id"))
}

fn lifecycle_event(
    state: &AppState,
    lease: &Value,
    outbox: &Value,
    effect: Option<&Value>,
    event_type: &str,
    details: Value,
) -> ApiResult<Value> {
    let destination = if let Some(effect) = effect {
        field(effect, "destination").to_owned()
    } else {
        let platform = state
            .store
            .get(CONNECTIONS, field(lease, "connection_id"))
            .map_err(ApiError::internal)?
            .and_then(|connection| connection["platform_kind"].as_str().map(str::to_owned));
        platform
            .map(|kind| format!("platform.{kind}"))
            .unwrap_or_else(|| "competition.lease".to_owned())
    };
    let mut payload = json!({
        "outbox_id":outbox["outbox_id"],"destination":destination,
        "aggregate_type":"instance_lease","aggregate_id":lease["lease_id"]
    });
    if let (Some(payload), Some(details)) = (payload.as_object_mut(), details.as_object()) {
        payload.extend(details.clone());
    }
    let aggregate_id = effect
        .map(|effect| field(effect, "effect_id"))
        .filter(|id| !id.is_empty())
        .unwrap_or_else(|| field(outbox, "outbox_id"));
    Ok(planned_event(
        field(lease, "competition_id"), "effect", aggregate_id,
        event_type, field(outbox, "command_id"), payload,
    ))
}

fn retry_after_seconds(outbox: &Value) -> Value {
    outbox["next_attempt_at"]
        .as_str()
        .and_then(|text| chrono::DateTime::parse_from_rfc3339(text).ok())
        .map(|at| (at.with_timezone(&chrono::Utc) - chrono::Utc::now())
            .num_milliseconds().max(0) as f64 / 1000.0)
        .map_or(Value::Null, |seconds| json!(seconds))
}

fn failure_event(
    state: &AppState,
    lease: &Value,
    outbox: &Value,
    effect: Option<&Value>,
    error_code: &str,
) -> ApiResult<Value> {
    let terminal = outbox["status"] == "dead_letter";
    let paused = outbox["status"] == "paused";
    lifecycle_event(
        state, lease, outbox, effect,
        if terminal { "competition.effect.dead_letter" }
        else if paused { "competition.effect.authentication_paused" }
        else { "competition.effect.retry_scheduled" },
        json!({"attempt":outbox["attempts"],"error_code":effect_failure_code(error_code),
            "retry_after_seconds":if terminal || paused {Value::Null} else {retry_after_seconds(outbox)}}),
    )
}

fn is_auth_error(code: &str) -> bool {
    matches!(code, "competition.auth_required" | "competition.credential_expired")
}

fn acquire_rejected(error: &ApiError) -> bool {
    matches!(error.status, StatusCode::BAD_REQUEST | StatusCode::METHOD_NOT_ALLOWED
        | StatusCode::UNPROCESSABLE_ENTITY | StatusCode::NOT_FOUND)
}

fn effect_failure_code(code: &str) -> &str {
    if is_auth_error(code) { "competition.effect.auth_required" } else { code }
}

fn effect_error(outbox: &Value, code: &str, message: &str, retryable: bool) -> Value {
    let category = if code == "competition.platform_not_found" {
        "not_found"
    } else if is_auth_error(code) {
        "permission"
    } else if code == "competition.lease.never_dispatched" {
        "state"
    } else {
        "platform"
    };
    let correlation_id = field(outbox, "command_id");
    json!({"schema_version":1,"code":effect_failure_code(code),"message":message,"category":category,
        "recovery_hint":if is_auth_error(code) {"reauthorize the platform connection"}
            else if retryable {"retry the effect after the recorded retry window"}
            else {"inspect the effect and retry manually"},
        "correlation_id":if correlation_id.is_empty() {field(outbox,"outbox_id")} else {correlation_id},
        "retryable":retryable,"detail":{}})
}

fn ensure_effect(state: &AppState, lease: &Value, outbox: &Value) -> ApiResult<Option<Value>> {
    let command_id = field(outbox, "command_id");
    if command_id.is_empty() {
        return Ok(None);
    }
    let Some(command) = state
        .store
        .get(COMMANDS, command_id)
        .map_err(ApiError::internal)?
    else {
        return Ok(None);
    };
    let effect_id = effect_id(outbox);
    if let Some(existing) = state
        .store
        .get(EFFECTS, &effect_id)
        .map_err(ApiError::internal)?
    {
        return Ok(Some(existing));
    }
    let connection_id = field(lease, "connection_id");
    let platform = state
        .store
        .get(CONNECTIONS, connection_id)
        .map_err(ApiError::internal)?
        .and_then(|connection| connection["platform_kind"].as_str().map(str::to_owned));
    let destination = platform
        .map(|kind| format!("platform.{kind}"))
        .unwrap_or_else(|| "competition.lease".to_owned());
    let lease_id = field(lease, "lease_id");
    let correlation_id = command["request"]["payload"]["correlation_id"]
        .as_str()
        .filter(|value| !value.is_empty())
        .unwrap_or(command_id);
    let effect = json!({"schema_version":1,"effect_id":effect_id,"command_id":command_id,
        "acceptance_receipt_id":command["response"]["receipt"]["receipt_id"],
        "domain":"competition","state":"waiting","aggregate":{"type":"instance_lease","id":lease_id},
        "correlation_id":correlation_id,"outbox_id":outbox["outbox_id"],"destination":destination,
        "idempotency_key":outbox["idempotency_key"].as_str().filter(|value|!value.is_empty()).unwrap_or(command_id),
        "attempts":outbox["attempts"],"max_attempts":outbox["max_attempts"].as_u64().unwrap_or(8),
        "retry_after":null,"object_links":{"aggregate":format!("instance_lease:{lease_id}")},
        "output":{},"error":null,"created_at":now(),"updated_at":now(),"completed_at":null});
    let _ = state
        .store
        .put_if_absent(EFFECTS, &effect_id, &effect)
        .map_err(ApiError::internal)?;
    state
        .store
        .get(EFFECTS, &effect_id)
        .map_err(ApiError::internal)
}

fn effect_transition(
    before: &Value,
    outbox: &Value,
    status: &str,
    output: Value,
    error: Option<(&str, &str)>,
) -> Value {
    let mut next = before.clone();
    next["state"] = json!(status);
    next["attempts"] = outbox["attempts"].clone();
    next["retry_after"] = outbox["next_attempt_at"].clone();
    next["output"] = output;
    next["error"] = error
        .map(|(code, message)| effect_error(outbox, code, message, status == "waiting"))
        .unwrap_or(Value::Null);
    next["updated_at"] = json!(now());
    next["completed_at"] = if matches!(status, "completed" | "failed" | "cancelled" | "dead_letter")
    {
        json!(now())
    } else {
        Value::Null
    };
    next
}

pub(super) fn is_effect(state: &AppState, id: &str) -> ApiResult<bool> {
    Ok(state
        .store
        .get(EFFECTS, id)
        .map_err(ApiError::internal)?
        .is_some_and(|effect| {
            effect["domain"] == "competition" && effect["aggregate"]["type"] == "instance_lease"
        }))
}

pub(super) async fn control_effect(state: &Arc<AppState>, envelope: &Value) -> ApiResult<Value> {
    let kind = field(envelope, "command_type");
    let effect_id = field(&envelope["payload"], "effect_id");
    let effect_id = if effect_id.is_empty() {
        field(envelope, "aggregate_id")
    } else {
        effect_id
    };
    let command_id = field(envelope, "command_id");
    let effect = state
        .store
        .get(EFFECTS, effect_id)
        .map_err(ApiError::internal)?
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::NOT_FOUND,
                "effect.not_found",
                "Competition effect not found",
            )
        })?;
    let lease_id = field(&effect["aggregate"], "id");
    let _lock = lease_lock(state, lease_id).await?;
    let effect = state
        .store
        .get(EFFECTS, effect_id)
        .map_err(ApiError::internal)?
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::NOT_FOUND,
                "effect.not_found",
                "Competition effect not found",
            )
        })?;
    let outbox_id = field(&effect, "outbox_id");
    let outbox = state
        .store
        .get(OUTBOX, outbox_id)
        .map_err(ApiError::internal)?
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::CONFLICT,
                "effect.outbox_missing",
                "Instance effect outbox is missing",
            )
        })?;
    let status = field(&outbox, "status");
    let retry = kind == "effect.retry";
    if retry && matches!(status, "processing" | "delivered" | "cancelled") {
        return Err(conflict(
            "effect.retry_not_allowed",
            "Remote delivery may have started",
        ));
    }
    if !retry {
        if envelope["payload"]["confirm"] != true {
            return Err(ApiError::new(
                StatusCode::BAD_REQUEST,
                "effect.cancel_confirmation_required",
                "effect.cancel requires payload.confirm=true",
            ));
        }
        if matches!(status, "processing" | "delivered") {
            return Err(conflict(
                "effect.irreversible",
                "Remote delivery may have started",
            ));
        }
    }
    let lease = state
        .store
        .get(LEASES, lease_id)
        .map_err(ApiError::internal)?
        .ok_or_else(|| ApiError::missing("instance lease"))?;
    let competition_id = field(&lease, "competition_id");
    let original_command_id = field(&effect, "command_id");
    let target = if retry { "waiting" } else { "cancelled" };
    let event_type = if retry {
        "competition.effect.manual_retry_requested"
    } else {
        "competition.effect.cancel_requested"
    };
    let event = planned_event(
        competition_id,
        "effect",
        effect_id,
        event_type,
        command_id,
        json!({"effect_id":effect_id,"original_command_id":original_command_id,
            "outbox_id":outbox_id,"from":effect["state"],"to":target}),
    );
    let accepted = json!({"schema_version":1,"command_id":command_id,"receipt_id":id("rcpt"),
        "state":"accepted","run_id":null,"aggregate":{"type":"effect","id":effect_id},
        "event_cursor":null,"deduplicated":false,"submitted_at":now(),"completed_at":null,
        "next":["receipt"],"output":{},"effect_ids":[],"error":null});
    let planned = json!({"request":envelope,"response":{"receipt":accepted}});
    let committed = crate::operations_api::persist_competition_plan(
        state,
        command_id,
        &planned,
        competition_id,
        &[event],
    )
    .map_err(ApiError::internal)?;
    let Some(events) = committed else {
        let prior = state
            .store
            .get(COMMANDS, command_id)
            .map_err(ApiError::internal)?
            .ok_or_else(|| {
                conflict(
                    "competition.command.concurrent_update",
                    "Effect command changed concurrently",
                )
            })?;
        if prior["request"] != *envelope {
            return Err(conflict(
                "competition.command.idempotency_conflict",
                "Command ID was used with different input",
            ));
        }
        let mut receipt = prior["response"]["receipt"].clone();
        receipt["deduplicated"] = json!(true);
        return Ok(receipt);
    };
    for event in events {
        let _ = event_bus(competition_id).send(event);
    }
    let mut next_outbox = outbox.clone();
    let mut next_effect = effect.clone();
    next_outbox["status"] = json!(if retry { "pending" } else { "cancelled" });
    next_outbox["updated_at"] = json!(now());
    if retry {
        next_outbox["next_attempt_at"] = json!(now());
        next_outbox["last_error"] = Value::Null;
        next_outbox["delivered_at"] = Value::Null;
    } else {
        next_outbox["last_error"] = json!("cancelled by operator before delivery");
    }
    next_effect["state"] = json!(target);
    next_effect["attempts"] = next_outbox["attempts"].clone();
    next_effect["retry_after"] = Value::Null;
    next_effect["error"] = Value::Null;
    next_effect["completed_at"] = if retry { Value::Null } else { json!(now()) };
    next_effect["updated_at"] = json!(now());
    let previous_command = state
        .store
        .get(COMMANDS, original_command_id)
        .map_err(ApiError::internal)?;
    let next_command = previous_command.as_ref().map(|prior| {
        let mut next = prior.clone();
        let receipt = &mut next["response"]["receipt"];
        receipt["state"] = json!(target);
        receipt["completed_at"] = if retry { Value::Null } else { json!(now()) };
        receipt["error"] = Value::Null;
        receipt["effect_ids"] = json!([effect_id]);
        let mut output = receipt["output"].as_object().cloned().unwrap_or_default();
        output.insert("effect_state".to_owned(), json!(target));
        output.insert("effect_ids".to_owned(), json!([effect_id]));
        output.insert("outbox_ids".to_owned(), json!([outbox_id]));
        receipt["output"] = Value::Object(output);
        next["status"] = json!(crate::operations_api::planned_receipt_status(
            &next["response"]["receipt"]
        ).as_u16());
        next
    });
    let previous_claim = state.store.get("platform_command_claims", original_command_id)
        .map_err(ApiError::internal)?;
    let next_claim = previous_claim.as_ref().map(|prior| {
        let mut next = prior.clone();
        next["state"] = json!(if retry { "accepted" } else { "failed" });
        next["updated_at"] = json!(now());
        next
    });
    let mut expected = vec![
        (OUTBOX, outbox_id, Some(&outbox)),
        (EFFECTS, effect_id, Some(&effect)),
    ];
    let mut records = vec![
        (OUTBOX, outbox_id, &next_outbox),
        (EFFECTS, effect_id, &next_effect),
    ];
    if let (Some(prior), Some(next)) = (&previous_command, &next_command) {
        expected.push((COMMANDS, original_command_id, Some(prior)));
        records.push((COMMANDS, original_command_id, next));
    }
    if let (Some(prior), Some(next)) = (&previous_claim, &next_claim) {
        expected.push(("platform_command_claims", original_command_id, Some(prior)));
        records.push(("platform_command_claims", original_command_id, next));
    }
    if !state
        .store
        .put_many_if(&expected, &records)
        .map_err(ApiError::internal)?
    {
        return Err(conflict(
            "competition.lease.concurrent_update",
            "Instance effect changed concurrently",
        ));
    }
    let mut receipt = accepted;
    receipt["state"] = json!("completed");
    receipt["completed_at"] = json!(now());
    receipt["output"] = json!({"effect":next_effect,"outbox_status":next_outbox["status"],
        "original_command_id":original_command_id});
    Ok(receipt)
}

pub(super) fn planned_event(
    competition_id: &str,
    aggregate_type: &str,
    aggregate_id: &str,
    event_type: &str,
    command_id: &str,
    payload: Value,
) -> Value {
    json!({"event_type":event_type,"occurred_at":now(),"competition_id":competition_id,
        "aggregate_type":aggregate_type,"aggregate_id":aggregate_id,"payload":payload,
        "command_id":command_id})
}

fn record_plan(
    state: &AppState,
    competition_id: &str,
    kind: &str,
    command_id: &str,
    lease_id: &str,
    payload: &Value,
    events: &[Value],
    admission: &Value,
) -> ApiResult<(String, Value)> {
    let command_id = if command_id.is_empty() {
        id("cmd")
    } else {
        command_id.to_owned()
    };
    let expected_version = admission["expected_version"].as_u64();
    let streams = events.iter().map(|event|
        (field(event,"aggregate_type"),field(event,"aggregate_id")))
        .collect::<std::collections::HashSet<_>>();
    if expected_version.is_some() && streams.len() != 1 {
        return Err(ApiError::new(StatusCode::BAD_GATEWAY,"command.local_commit_failed",
            "expected_version requires all events to target one stream"));
    }
    let version_target = expected_version.and_then(|_|streams.iter().next().copied());
    let version_key = version_target.map_or_else(String::new,|(kind,id)|format!("{kind}\0{id}"));
    let version_row = if version_target.is_some() {
        state.store.get(VERSIONS, &version_key).map_err(ApiError::internal)?
    } else { None };
    if let (Some(expected),Some((version_type,version_id))) = (expected_version,version_target) {
        let actual = crate::operations_api::current_aggregate_version(
            state, version_type, version_id).map_err(ApiError::internal)?;
        if actual != expected {
            return Err(ApiError::new(StatusCode::CONFLICT,"command.version_conflict",
                format!("Expected version {expected}, current version {actual}")));
        }
    }
    let request = json!({"command_type":kind,"competition_id":competition_id,"payload":payload});
    let receipt = json!({"schema_version":1,"command_id":command_id,"receipt_id":id("rcpt"),
        "state":"accepted","run_id":null,"aggregate":{"type":"instance_lease","id":lease_id},
        "event_cursor":null,"deduplicated":false,"submitted_at":now(),"completed_at":null,
        "next":["receipt"],"output":{"lease_id":lease_id},"effect_ids":[],"error":null});
    let mut command = json!({"request":request,"response":{"receipt":receipt}});
    let claim = admission.get("claim").filter(|value| value.is_object());
    let key_id = field(admission, "key_id");
    let keyed = admission.get("keyed").filter(|value| value.is_object());
    if let Some(claim) = claim {
        command["request_fingerprint"] = claim["request_fingerprint"].clone();
        command["status"] = json!(StatusCode::ACCEPTED.as_u16());
    }
    let mut expected = vec![(COMMANDS, command_id.as_str(), None)];
    let mut records = vec![(COMMANDS, command_id.as_str(), &command)];
    if version_target.is_some() {
        expected.push((VERSIONS, version_key.as_str(), version_row.as_ref()));
    }
    if let Some(claim) = claim {
        expected.push(("platform_command_claims", command_id.as_str(), None));
        records.push(("platform_command_claims", command_id.as_str(), claim));
        if !key_id.is_empty() && let Some(keyed) = keyed {
            expected.push(("platform_idempotency_keys", key_id, None));
            records.push(("platform_idempotency_keys", key_id, keyed));
        }
    }
    let committed = put_records_with_events(state, competition_id, &expected, &records, events)?;
    if committed {
        Ok((command_id, receipt))
    } else {
        if let (Some(expected),Some((version_type,version_id))) = (expected_version,version_target) {
            let actual = crate::operations_api::current_aggregate_version(
                state, version_type, version_id).map_err(ApiError::internal)?;
            if actual != expected {
                return Err(ApiError::new(StatusCode::CONFLICT,"command.version_conflict",
                    format!("Expected version {expected}, current version {actual}")));
            }
        }
        let previous = state
            .store
            .get(COMMANDS, &command_id)
            .map_err(ApiError::internal)?
            .ok_or_else(|| {
                conflict(
                    "competition.command.concurrent_update",
                    "Command changed concurrently",
                )
            })?;
        if previous["request"] != request {
            return Err(conflict(
                "competition.command.idempotency_conflict",
                "Command ID was used with different input",
            ));
        }
        let mut prior = previous["response"]["receipt"].clone();
        prior["deduplicated"] = json!(true);
        Ok((command_id, prior))
    }
}

fn update_receipt(
    state: &AppState,
    command_id: &str,
    status: &str,
    output: Value,
    error: Option<(&str, &str)>,
) -> ApiResult<Value> {
    let Some(before) = state
        .store
        .get(COMMANDS, command_id)
        .map_err(ApiError::internal)?
    else {
        return Err(ApiError::missing("command receipt"));
    };
    let mut after = receipt_transition(&before, status, output, error);
    after["status"] = json!(crate::operations_api::planned_receipt_status(
        &after["response"]["receipt"]
    ).as_u16());
    let result = after["response"]["receipt"].clone();
    let claim = state
        .store
        .get("platform_command_claims", command_id)
        .map_err(ApiError::internal)?;
    let mut next_claim = claim.clone();
    if let Some(next) = next_claim.as_mut() {
        next["state"] = json!(if status == "completed" { "completed" }
            else if matches!(status, "failed" | "cancelled" | "conflict" | "dead_letter") {
                "failed"
            } else { "accepted" });
        next["updated_at"] = json!(now());
    }
    let mut expected = vec![(COMMANDS, command_id, Some(&before))];
    let mut records = vec![(COMMANDS, command_id, &after)];
    if let (Some(before_claim), Some(after_claim)) = (claim.as_ref(), next_claim.as_ref()) {
        expected.push(("platform_command_claims", command_id, Some(before_claim)));
        records.push(("platform_command_claims", command_id, after_claim));
    }
    if !state
        .store
        .put_many_if(&expected, &records)
        .map_err(ApiError::internal)?
    {
        return Err(conflict(
            "competition.command.concurrent_update",
            "Command receipt changed concurrently",
        ));
    }
    Ok(result)
}

fn receipt_transition(
    before: &Value,
    status: &str,
    output: Value,
    error: Option<(&str, &str)>,
) -> Value {
    let mut after = before.clone();
    let receipt = &mut after["response"]["receipt"];
    receipt["state"] = json!(status);
    if let Some(effect_ids) = output.get("effect_ids") {
        receipt["effect_ids"] = effect_ids.clone();
    }
    receipt["output"] = output;
    receipt["completed_at"] = if matches!(status, "completed" | "failed" | "cancelled") {
        json!(now())
    } else {
        Value::Null
    };
    receipt["error"] = error.map(|(code, message)| {
        if is_auth_error(code) {
            json!({"code":effect_failure_code(code),"message":message,
                "category":"permission","recovery_hint":"reauthorize the platform connection",
                "retryable":status != "failed"})
        } else {
            json!({"code":code,"message":message,
                "category":if code.starts_with("competition.platform") {"platform"} else {"state"}})
        }
    }).unwrap_or(Value::Null);
    after["status"] = json!(crate::operations_api::planned_receipt_status(
        &after["response"]["receipt"]
    ).as_u16());
    after
}

fn new_outbox(lease: &Value, kind: &str, command_id: &str) -> Value {
    let outbox_id = id("lob");
    let idempotency_key = if command_id.is_empty() {
        outbox_id.as_str()
    } else {
        command_id
    };
    json!({"outbox_id":outbox_id,"idempotency_key":idempotency_key,
        "lease_id":lease["lease_id"],"competition_id":lease["competition_id"],
        "challenge_id":lease["competition_challenge_id"],"command_id":command_id,"op":kind,
        "status":"pending","attempts":0,"max_attempts":8,"next_attempt_at":null,
        "created_at":now(),"updated_at":now(),"last_error":null,
        "release_receipt":null})
}

fn lease_outbox(state: &AppState, lease_id: &str) -> ApiResult<Option<Value>> {
    Ok(state
        .store
        .list(OUTBOX)
        .map_err(ApiError::internal)?
        .into_iter()
        .filter(|row| row["lease_id"] == lease_id)
        .max_by(|a, b| field(a, "created_at").cmp(field(b, "created_at")))
        .filter(|row| matches!(field(row, "status"), "pending" | "processing" | "failed" | "paused")))
}

fn retry_due(outbox: &Value) -> bool {
    outbox["next_attempt_at"]
        .as_str()
        .and_then(|text| chrono::DateTime::parse_from_rfc3339(text).ok())
        .is_none_or(|at| at <= chrono::Utc::now())
}

fn retry_after(attempts: u64) -> Value {
    let base = (0.5 * 2_f64.powi(attempts.saturating_sub(1).min(12) as i32)).min(300.0);
    let jitter = uuid::Uuid::new_v4().as_u128() as u64 as f64 / u64::MAX as f64;
    json!(
        (chrono::Utc::now()
            + chrono::Duration::milliseconds(((base + jitter * base * 0.2) * 1000.0) as i64))
        .to_rfc3339()
    )
}

fn claim_replay(state: &AppState, lease: &Value, outbox: &Value) -> ApiResult<Value> {
    let outbox_id = field(outbox, "outbox_id");
    let previous = state.store.get(OUTBOX, outbox_id).map_err(ApiError::internal)?;
    let current = previous.as_ref().unwrap_or(outbox);
    if current["status"] == "paused" {
        return Err(conflict("competition.effect.authentication_paused",
            "Instance effect awaits authentication and manual retry"));
    }
    if current["status"] == "processing" {
        return Ok(current.clone());
    }
    let mut claimed = current.clone();
    claimed["status"] = json!("processing");
    claimed["attempts"] = json!(current["attempts"].as_u64().unwrap_or(0) + 1);
    claimed["next_attempt_at"] = Value::Null;
    claimed["updated_at"] = json!(now());
    let effect = ensure_effect(state, lease, &claimed)?;
    let next_effect = effect.as_ref().map(|prior| {
        let mut next = prior.clone();
        next["state"] = json!("running");
        next["attempts"] = claimed["attempts"].clone();
        next["retry_after"] = Value::Null;
        next["error"] = Value::Null;
        next["updated_at"] = json!(now());
        next
    });
    let effect_key = effect_id(&claimed);
    let mut expected = vec![(OUTBOX, outbox_id, previous.as_ref())];
    let mut records = vec![(OUTBOX, outbox_id, &claimed)];
    if let (Some(prior), Some(next)) = (&effect, &next_effect) {
        expected.push((EFFECTS, effect_key.as_str(), Some(prior)));
        records.push((EFFECTS, effect_key.as_str(), next));
    }
    let event = lifecycle_event(state, lease, &claimed, next_effect.as_ref(),
        "competition.effect.running", json!({"attempt":claimed["attempts"]}))?;
    if !put_records_with_events(state, field(lease, "competition_id"),
        &expected, &records, &[event])? {
        return Err(conflict("competition.lease.effect_claimed", "Instance effect was claimed by another process"));
    }
    Ok(claimed)
}

fn schedule_retry(state: &AppState, outbox: &Value, error: &ApiError) -> ApiResult<()> {
    let code = error.code;
    let auth = is_auth_error(code);
    let mut failed = outbox.clone();
    let attempts = outbox["attempts"].as_u64().unwrap_or(0)
        + u64::from(outbox["status"] != "processing");
    failed["attempts"] = json!(attempts);
    failed["status"] = json!(if attempts >= 8 {
        "dead_letter"
    } else if auth {
        "paused"
    } else {
        "failed"
    });
    failed["next_attempt_at"] = if attempts >= 8 || auth {
        Value::Null
    } else {
        retry_after(attempts)
    };
    failed["last_error"] = json!(code);
    failed["updated_at"] = json!(now());
    let lease = state
        .store
        .get(LEASES, field(outbox, "lease_id"))
        .map_err(ApiError::internal)?;
    let effect = if let Some(lease) = lease.as_ref() {
        ensure_effect(state, lease, outbox)?
    } else {
        None
    };
    let next_effect = effect.as_ref().map(|prior| {
        let mut next = prior.clone();
        next["state"] = json!(if failed["status"] == "dead_letter" {
            "dead_letter"
        } else {
            "waiting"
        });
        next["attempts"] = failed["attempts"].clone();
        next["retry_after"] = failed["next_attempt_at"].clone();
        next["error"] = effect_error(
            outbox,
            code,
            &error.message,
            failed["status"] != "dead_letter",
        );
        next["updated_at"] = json!(now());
        if failed["status"] == "dead_letter" {
            next["completed_at"] = json!(now());
        }
        next
    });
    let effect_key = effect_id(outbox);
    let outbox_id = field(outbox, "outbox_id");
    let mut expected = vec![(OUTBOX, outbox_id, Some(outbox))];
    let mut records = vec![(OUTBOX, outbox_id, &failed)];
    if let (Some(prior), Some(next)) = (&effect, &next_effect) {
        expected.push((EFFECTS, &effect_key, Some(prior)));
        records.push((EFFECTS, &effect_key, next));
    }
    let connection_id = lease.as_ref().map(|lease| field(lease, "connection_id").to_owned())
        .unwrap_or_default();
    let prior_connection = if auth && !connection_id.is_empty() {
        state.store.get(CONNECTIONS, &connection_id).map_err(ApiError::internal)?
    } else { None };
    let next_connection = prior_connection.as_ref().map(|prior| {
        let mut next = prior.clone();
        next["status"] = json!("auth_required");
        next["last_error"] = json!(error.message);
        next["updated_at"] = json!(now());
        next
    });
    if let (Some(prior), Some(next)) = (&prior_connection, &next_connection) {
        expected.push((CONNECTIONS, &connection_id, Some(prior)));
        records.push((CONNECTIONS, &connection_id, next));
    }
    let command_id = field(outbox, "command_id");
    let command_state = if failed["status"] == "dead_letter" { "failed" } else { "waiting" };
    let previous_command = if command_id.is_empty() { None } else {
        state.store.get(COMMANDS, command_id).map_err(ApiError::internal)?
    };
    let next_command = previous_command.as_ref().map(|prior| receipt_transition(
        prior, command_state,
        json!({"lease_id":outbox["lease_id"],"outbox_ids":[outbox_id],
            "effect_ids":[effect_key],
            "effect_state":next_effect.as_ref().map(|effect|field(effect,"state"))}),
        Some((code, &error.message)),
    ));
    if let (Some(prior), Some(next)) = (&previous_command, &next_command) {
        expected.push((COMMANDS, command_id, Some(prior)));
        records.push((COMMANDS, command_id, next));
    }
    let event_lease = lease.unwrap_or_else(|| json!({"lease_id":outbox["lease_id"],
        "competition_id":outbox["competition_id"]}));
    let event = failure_event(state, &event_lease, &failed, next_effect.as_ref(), code)?;
    if !put_records_with_events(state, field(&failed, "competition_id"),
        &expected, &records, &[event])? {
        return Err(conflict(
            "competition.lease.concurrent_update",
            "Instance effect changed concurrently",
        ));
    }
    Ok(())
}

fn quota(
    state: &AppState,
    competition_id: &str,
    connection: &Value,
    challenge_id: &str,
) -> ApiResult<()> {
    let policy = state
        .store
        .get(POLICIES, competition_id)
        .map_err(ApiError::internal)?
        .unwrap_or_else(|| default_policy(competition_id));
    let leases = state.store.list(LEASES).map_err(ApiError::internal)?;
    let competition_count = leases
        .iter()
        .filter(|lease| lease["competition_id"] == competition_id && occupies(lease))
        .count() as u64;
    let connection_id = field(connection, "connection_id");
    let connection_count = leases
        .iter()
        .filter(|lease| lease["connection_id"] == connection_id && occupies(lease))
        .count() as u64;
    let competition_limit = policy["max_instances"].as_u64().unwrap_or(0);
    let connection_limit = connection["capabilities"]["max_instances"]
        .as_u64()
        .or_else(|| connection["capabilities"]["detail"]["max_instances"].as_u64())
        .unwrap_or(0);
    for (scope, active, limit) in [
        ("competition", competition_count, competition_limit),
        ("connection", connection_count, connection_limit),
    ] {
        if limit > 0 && active >= limit {
            append_event(
                state,
                competition_id,
                "competition",
                competition_id,
                "competition.instance.quota_rejected",
                "",
                json!({"challenge_id":challenge_id,"scope":scope,"limit":limit,"active":active}),
            )?;
            return Err(conflict(
                "competition.instance.quota_exhausted",
                "Dynamic instance quota is exhausted",
            ));
        }
    }
    Ok(())
}

fn stage_ensure(
    state: &AppState,
    competition_id: &str,
    challenge: &Value,
    connection: &Value,
    command_id: &str,
    lease_id: &str,
    owner: &str,
    ttl_seconds: u64,
) -> ApiResult<(Value, Value)> {
    let challenge_id = field(challenge, "challenge_id");
    for _ in 0..3 {
        if active_for(state, competition_id, challenge_id)?.is_some() {
            return Err(conflict(
                "competition.lease.active_exists",
                "Challenge already has an active lease",
            ));
        }
        quota(state, competition_id, connection, challenge_id)?;
        let head = state
            .store
            .get(HEADS, challenge_id)
            .map_err(ApiError::internal)?;
        let comp_clock_id = format!("competition:{competition_id}");
        let conn_clock_id = format!("connection:{}", field(connection, "connection_id"));
        let comp_clock = state
            .store
            .get(CLOCKS, &comp_clock_id)
            .map_err(ApiError::internal)?;
        let conn_clock = state
            .store
            .get(CLOCKS, &conn_clock_id)
            .map_err(ApiError::internal)?;
        let bump = |value: &Option<Value>| json!({"version":value.as_ref().and_then(|v|v["version"].as_u64()).unwrap_or(0)+1});
        let next_comp_clock = bump(&comp_clock);
        let next_conn_clock = bump(&conn_clock);
        let lease = json!({"lease_id":lease_id,"connection_id":field(connection,"connection_id"),
            "competition_id":competition_id,"competition_challenge_id":challenge_id,"platform_instance_id":"",
            "generation":0,"fencing_token":0,"owner":owner,"address":"","has_credential":false,
            "ttl_seconds":ttl_seconds,"renew_deadline_at":null,"expires_at":null,"state":"requested","last_error":"",
            "created_at":now(),"updated_at":now()});
        let outbox = new_outbox(&lease, "ensure", command_id);
        let next_head = json!({"lease_id":lease_id,"updated_at":now()});
        let mut expected = vec![
            (HEADS, challenge_id, head.as_ref()),
            (CLOCKS, comp_clock_id.as_str(), comp_clock.as_ref()),
            (CLOCKS, conn_clock_id.as_str(), conn_clock.as_ref()),
        ];
        let mut records = vec![
            (HEADS, challenge_id, &next_head),
            (CLOCKS, comp_clock_id.as_str(), &next_comp_clock),
            (CLOCKS, conn_clock_id.as_str(), &next_conn_clock),
            (LEASES, lease_id, &lease),
            (OUTBOX, field(&outbox, "outbox_id"), &outbox),
        ];
        let mut provisioning = challenge.clone();
        if challenge["state"] == "queued" {
            expected.push((CHALLENGES, challenge_id, Some(challenge)));
            provisioning["state"] = json!("provisioning");
            provisioning["paused_from"] = Value::Null;
            provisioning["updated_at"] = json!(now());
            records.push((CHALLENGES, challenge_id, &provisioning));
        }
        if state
            .store
            .put_many_if(&expected, &records)
            .map_err(ApiError::internal)?
        {
            return Ok((lease, outbox));
        }
    }
    Err(conflict(
        "competition.lease.concurrent_update",
        "Instance lease changed concurrently",
    ))
}

fn stage_stop(
    state: &AppState,
    lease: &Value,
    command_id: &str,
) -> ApiResult<(Value, Option<Value>)> {
    let lease_id = field(lease, "lease_id");
    let status = field(lease, "state");
    if status == "requested" {
        let mut released = lease.clone();
        released["state"] = json!("released");
        released["release_receipt"] = json!("no_remote_instance");
        released["release_command_id"] = json!(command_id);
        released["updated_at"] = json!(now());
        let pending = lease_outbox(state, lease_id)?;
        let committed = if let Some(outbox) = &pending {
            let mut cancelled = outbox.clone();
            cancelled["status"] = json!("cancelled");
            cancelled["updated_at"] = json!(now());
            state
                .store
                .put_many_if(
                    &[
                        (LEASES, lease_id, Some(lease)),
                        (OUTBOX, field(outbox, "outbox_id"), Some(outbox)),
                    ],
                    &[
                        (LEASES, lease_id, &released),
                        (OUTBOX, field(outbox, "outbox_id"), &cancelled),
                    ],
                )
                .map_err(ApiError::internal)?
        } else {
            state
                .store
                .put_many_if(
                    &[(LEASES, lease_id, Some(lease))],
                    &[(LEASES, lease_id, &released)],
                )
                .map_err(ApiError::internal)?
        };
        if !committed {
            return Err(conflict(
                "competition.lease.concurrent_update",
                "Instance lease changed concurrently",
            ));
        }
        if let Some(outbox) = pending.as_ref()
            && !field(outbox, "command_id").is_empty()
        {
            let _ = update_receipt(
                state,
                field(outbox, "command_id"),
                "cancelled",
                json!({"lease_id":lease_id,"state":"released"}),
                None,
            );
        }
        append_event(
            state,
            field(lease, "competition_id"),
            "instance_lease",
            lease_id,
            "competition.instance.state_changed",
            command_id,
            json!({"lease_id":lease_id,"from":"requested","to":"released","release_receipt":"no_remote_instance"}),
        )?;
        return Ok((released, None));
    }
    if status == "releasing" {
        return Ok((lease.clone(), lease_outbox(state, lease_id)?));
    }
    if !matches!(status, "active" | "renewing") {
        return Err(conflict(
            "competition.instance.illegal_transition",
            "Instance cannot be stopped from its current state",
        ));
    }
    let mut releasing = lease.clone();
    releasing["state"] = json!("releasing");
    releasing["release_command_id"] = json!(command_id);
    releasing["updated_at"] = json!(now());
    let outbox = new_outbox(&releasing, "stop", command_id);
    if !state
        .store
        .put_many_if(
            &[(LEASES, lease_id, Some(lease))],
            &[
                (LEASES, lease_id, &releasing),
                (OUTBOX, field(&outbox, "outbox_id"), &outbox),
            ],
        )
        .map_err(ApiError::internal)?
    {
        return Err(conflict(
            "competition.lease.concurrent_update",
            "Instance lease changed concurrently",
        ));
    }
    Ok((releasing, Some(outbox)))
}

fn stage_renew(state: &AppState, lease: &Value) -> ApiResult<Option<Value>> {
    if field(lease, "state") != "active" {
        return Ok(None);
    }
    let lease_id = field(lease, "lease_id");
    let mut renewing = lease.clone();
    renewing["state"] = json!("renewing");
    renewing["updated_at"] = json!(now());
    let outbox = new_outbox(&renewing, "renew", "");
    if !state
        .store
        .put_many_if(
            &[(LEASES, lease_id, Some(lease))],
            &[
                (LEASES, lease_id, &renewing),
                (OUTBOX, field(&outbox, "outbox_id"), &outbox),
            ],
        )
        .map_err(ApiError::internal)?
    {
        return Ok(None);
    }
    append_event(
        state,
        field(lease, "competition_id"),
        "instance_lease",
        lease_id,
        "competition.instance.state_changed",
        "",
        json!({"lease_id":lease_id,"from":"active","to":"renewing"}),
    )?;
    Ok(Some(outbox))
}

fn superseded(state:&AppState,lease:&Value)->ApiResult<bool>{
    let remote=field(lease,"platform_instance_id");if remote.is_empty(){return Ok(false);}
    let key=format!("{}\0{remote}",field(lease,"connection_id"));let token=lease["fencing_token"].as_u64().unwrap_or(0);
    let head=state.store.get(FENCING,&key).map_err(ApiError::internal)?.and_then(|row|row["fencing_token"].as_u64()).unwrap_or(0);
    Ok(head>token || state.store.list(LEASES).map_err(ApiError::internal)?.iter().any(|row|row["connection_id"]==lease["connection_id"] && row["platform_instance_id"]==remote && row["fencing_token"].as_u64().unwrap_or(0)>token))
}
async fn retire_superseded(state:&Arc<AppState>,lease:&Value)->ApiResult<Value>{
    let lease_id=field(lease,"lease_id").to_owned();
    for _ in 0..32{let current=state.store.get(LEASES,&lease_id).map_err(ApiError::internal)?.ok_or_else(||ApiError::missing("instance lease"))?;if current["state"]=="lost"||!superseded(state,&current)?{return Ok(current);}let lease=&current;
    let mut next=lease.clone();next["state"]=json!("lost");next["last_error"]=json!("Remote instance ownership was superseded by a higher fencing token");next["updated_at"]=json!(now());
    let outbox=lease_outbox(state,field(lease,"lease_id"))?;
    let failed=outbox.as_ref().map(|row|{let mut next=row.clone();next["status"]=json!("dead_letter");next["last_error"]=json!("stale_fencing");next});
    let effect_key=outbox.as_ref().map(effect_id).unwrap_or_default();let command_id=outbox.as_ref().map(|row|field(row,"command_id")).unwrap_or("");
    let effect=state.store.get(EFFECTS,&effect_key).map_err(ApiError::internal)?;let command=state.store.get(COMMANDS,command_id).map_err(ApiError::internal)?;
    let error=json!({"code":"competition.lease.stale_fencing","message":"Instance ownership changed","retryable":false});
    let next_effect=effect.as_ref().map(|row|{let mut next=row.clone();next["state"]=json!("failed");next["error"]=error.clone();next["completed_at"]=json!(now());next});
    let next_command=command.as_ref().map(|row|{let mut next=row.clone();next["response"]["receipt"]["state"]=json!("failed");next["response"]["receipt"]["error"]=error.clone();next["response"]["receipt"]["completed_at"]=json!(now());next});
    let mut expected=vec![(LEASES,field(lease,"lease_id"),Some(lease))];let mut records=vec![(LEASES,field(lease,"lease_id"),&next)];
    if let(Some(before),Some(after))=(&effect,&next_effect){expected.push((EFFECTS,&effect_key,Some(before)));records.push((EFFECTS,&effect_key,after));}
    if let(Some(before),Some(after))=(&command,&next_command){expected.push((COMMANDS,command_id,Some(before)));records.push((COMMANDS,command_id,after));}
    if let(Some(before),Some(after))=(&outbox,&failed){expected.push((OUTBOX,field(before,"outbox_id"),Some(before)));records.push((OUTBOX,field(before,"outbox_id"),after));}
    let event=planned_event(field(lease,"competition_id"),"instance_lease",field(lease,"lease_id"),"competition.instance.state_changed","",json!({"lease_id":lease["lease_id"],"from":lease["state"],"to":"lost","reason":"stale_fencing"}));
    if put_records_with_events(state,field(lease,"competition_id"),&expected,&records,&[event])?{degrade(state,&next,"stale_fencing").await?;return Ok(next);}
    }Err(conflict("competition.lease.concurrent_update","Lease ownership changed repeatedly during retirement"))
}

async fn remote(state: &Arc<AppState>, lease: &Value, op: &str) -> ApiResult<(StatusCode, Value)> {
    if superseded(state,lease)?{return Err(conflict("competition.lease.stale_fencing","Remote instance ownership has changed"));}
    let competition_id = field(lease, "competition_id");
    let competition = live_competition(state, competition_id)?;
    let connection = live_connection(state, field(&competition, "connection_id"))?;
    let challenge = state
        .store
        .get(CHALLENGES, field(lease, "competition_challenge_id"))
        .map_err(ApiError::internal)?
        .ok_or_else(|| ApiError::missing("challenge"))?;
    let platform = field(&connection, "platform_kind");
    let external = field(&challenge, "external_challenge_id");
    if !matches!(platform, "gzctf" | "rctf") {
        let result = extension_adapter::instance(
            state,
            &connection,
            &challenge,
            if op == "ensure" { None } else { Some(lease) },
            op == "stop",
        )
        .await?;
        return Ok((StatusCode::OK, result));
    }
    let (method, path) = if platform == "gzctf" {
        let (game, cid) = external
            .split_once('/')
            .ok_or_else(|| ApiError::invalid("Invalid GZCTF challenge identity"))?;
        external_segment(game)?;
        external_segment(cid)?;
        (
            if op == "stop" {
                reqwest::Method::DELETE
            } else {
                reqwest::Method::POST
            },
            format!(
                "/api/game/{game}/container/{cid}{}",
                if op == "renew" || op == "probe" {
                    "/extend"
                } else {
                    ""
                }
            ),
        )
    } else {
        external_segment(external)?;
        if connection["capabilities"]["evidence"]["api_version"] == "v1" {
            return Err(ApiError::unsupported(
                "rCTF dynamic instances require API v2",
            ));
        }
        (
            match op {
                "stop" => reqwest::Method::DELETE,
                "ensure" => reqwest::Method::PUT,
                _ => reqwest::Method::PATCH,
            },
            format!("/api/v2/integrations/challs/{external}/instance"),
        )
    };
    let body = if platform == "rctf" && matches!(op, "ensure" | "renew" | "probe") {
        Some(json!({}))
    } else {
        None
    };
    let (status, body) = platform_request(state, &connection, method, &path, body).await?;
    if status == StatusCode::NOT_FOUND {
        return Ok((status, Value::Null));
    }
    Ok((status, checked_platform_response(status, body, platform)?))
}

fn result_address(result:&Value)->String{
    if let Some(value)=result.as_str(){return value.to_owned();}
    if let Some(value)=result["url"].as_str().filter(|v|!v.is_empty()){return value.to_owned();}
    let endpoints=&result["endpoints"];
    for kind in ["http","https","tcp"]{
        if let Some(address)=endpoints[kind].as_str().filter(|v|!v.is_empty()){return address.to_owned();}
        if let Some(rows)=endpoints.as_array(){for endpoint in rows.iter().filter(|row|row["kind"]==kind){if let(Some(host),Some(port))=(endpoint["host"].as_str(),endpoint["port"].as_u64()){return if kind=="tcp"{format!("{host}:{port}")}else{format!("{kind}://{host}:{port}")};}}}
    }
    if let Some(address)=endpoints.as_object().and_then(|rows|rows.values().find_map(Value::as_str)){return address.to_owned();}
    if let Some(host)=result["host"].as_str().or_else(||result["entry"].as_str()){
        return result["port"].as_u64().map(|port|format!("{host}:{port}")).unwrap_or_else(||host.to_owned());
    }String::new()
}

fn apply_result(
    state: &AppState,
    lease: &mut Value,
    result: &Value,
    adopt: bool,
) -> ApiResult<bool> {
    let old_id = field(lease, "platform_instance_id").to_owned();
    let old_address = field(lease, "address").to_owned();
    let competition_id = field(lease, "competition_id");
    let competition = live_competition(state, competition_id)?;
    let connection = live_connection(state, field(&competition, "connection_id"))?;
    let platform = field(&connection, "platform_kind");
    let challenge = state
        .store
        .get(CHALLENGES, field(lease, "competition_challenge_id"))
        .map_err(ApiError::internal)?
        .ok_or_else(|| ApiError::missing("challenge"))?;
    let external = field(&challenge, "external_challenge_id");
    let remote_id = result["lease"]["lease_id"]
        .as_str()
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| {
            if old_id.is_empty() {
                format!("{platform}:{external}")
            } else {
                old_id.clone()
            }
        });
    let address = result_address(result);
    let hard = old_id != remote_id || (!address.is_empty() && old_address != address);
    let persistent = state
        .store
        .get(POLICIES, competition_id)
        .map_err(ApiError::internal)?
        .is_some_and(|policy| {
            policy["persistent_challenge_ids"]
                .as_array()
                .is_some_and(|ids| ids.iter().any(|v| v == external))
        });
    let expiry = result["expectStopAt"]
        .as_str()
        .or_else(|| result["expiresAt"].as_str())
        .or_else(|| result["lease"]["expires_at"].as_str())
        .and_then(|text| chrono::DateTime::parse_from_rfc3339(text).ok())
        .map(|value| value.with_timezone(&chrono::Utc))
        .unwrap_or_else(|| chrono::Utc::now() + chrono::Duration::hours(1));
    let ttl = (expiry - chrono::Utc::now()).num_seconds().max(0);
    let skew = 60_i64.min(ttl / 2);
    lease["platform_instance_id"] = json!(remote_id);
    if !address.is_empty() {
        lease["address"] = json!(address);
    }
    lease["expires_at"] = if persistent {
        Value::Null
    } else {
        json!(expiry.to_rfc3339())
    };
    lease["renew_deadline_at"] = if persistent {
        Value::Null
    } else {
        json!((expiry - chrono::Duration::seconds(skew)).to_rfc3339())
    };
    lease["ttl_seconds"] = json!(if persistent { 0 } else { ttl });
    lease["last_error"] = json!("");
    lease["reconcile_degraded"] = json!(false);
    if adopt || old_id != remote_id {
        let peers = state.store.list(LEASES).map_err(ApiError::internal)?;
        let token = peers
            .iter()
            .filter(|row| {
                row["connection_id"] == lease["connection_id"]
                    && row["platform_instance_id"] == remote_id
            })
            .filter_map(|row| row["fencing_token"].as_u64())
            .max()
            .unwrap_or(0)
            + 1;
        let generation = peers
            .iter()
            .filter(|row| {
                row["connection_id"] == lease["connection_id"]
                    && row["platform_instance_id"] == remote_id
                    && row["lease_id"] != lease["lease_id"]
            })
            .filter_map(|row| row["generation"].as_u64())
            .max()
            .unwrap_or(0)
            + 1;
        lease["fencing_token"] = json!(token);
        lease["generation"] =
            json!(generation.max(lease["generation"].as_u64().unwrap_or(0) + u64::from(!adopt)));
    }
    Ok(hard)
}

fn terminal_not_found(
    state: &AppState,
    before: &Value,
    outbox: &Value,
    next_state: &str,
    reconciled: Option<&str>,
) -> ApiResult<Option<Value>> {
    terminal_failure(state, before, outbox, next_state, reconciled,
        "competition.platform_not_found", "Remote instance was not found")
}

fn terminal_failure(
    state: &AppState, before: &Value, outbox: &Value, next_state: &str,
    reconciled: Option<&str>, code: &str, message: &str,
) -> ApiResult<Option<Value>> {
    let lease_id = field(before, "lease_id");
    let outbox_id = field(outbox, "outbox_id");
    let command_id = field(outbox, "command_id");
    let effect_key = effect_id(outbox);
    let mut terminal = before.clone();
    terminal["state"] = json!(next_state);
    terminal["last_error"] = json!(code);
    terminal["reconcile_degraded"] = json!(false);
    terminal["updated_at"] = json!(now());
    let mut dead_letter = outbox.clone();
    dead_letter["status"] = json!("dead_letter");
    dead_letter["last_error"] = json!(code);
    dead_letter["next_attempt_at"] = Value::Null;
    dead_letter["updated_at"] = json!(now());
    let previous_effect = ensure_effect(state, before, outbox)?;
    let next_effect = previous_effect.as_ref().map(|prior| effect_transition(
        prior, &dead_letter, "dead_letter",
        json!({"lease_id":lease_id,"state":next_state}),
        Some((code, message)),
    ));
    let previous_command = if command_id.is_empty() { None } else {
        state.store.get(COMMANDS, command_id).map_err(ApiError::internal)?
    };
    let next_command = previous_command.as_ref().map(|prior| receipt_transition(
        prior, "failed",
        json!({"lease_id":lease_id,"state":next_state,"outbox_ids":[outbox_id],
            "effect_ids":[effect_key],"effect_state":"dead_letter"}),
        Some((code, message)),
    ));
    let previous_claim = if command_id.is_empty() { None } else {
        state.store.get("platform_command_claims", command_id).map_err(ApiError::internal)?
    };
    let next_claim = previous_claim.as_ref().map(|prior| {
        let mut next = prior.clone();
        next["state"] = json!("failed");
        next["updated_at"] = json!(now());
        next
    });
    let mut expected = vec![(LEASES, lease_id, Some(before)), (OUTBOX, outbox_id, Some(outbox))];
    let mut records = vec![(LEASES, lease_id, &terminal), (OUTBOX, outbox_id, &dead_letter)];
    if let (Some(prior), Some(next)) = (&previous_effect, &next_effect) {
        expected.push((EFFECTS, effect_key.as_str(), Some(prior)));
        records.push((EFFECTS, effect_key.as_str(), next));
    }
    if let (Some(prior), Some(next)) = (&previous_command, &next_command) {
        expected.push((COMMANDS, command_id, Some(prior)));
        records.push((COMMANDS, command_id, next));
    }
    if let (Some(prior), Some(next)) = (&previous_claim, &next_claim) {
        expected.push(("platform_command_claims", command_id, Some(prior)));
        records.push(("platform_command_claims", command_id, next));
    }
    let mut state_payload = json!({"lease_id":lease_id,"from":before["state"],"to":next_state});
    if let Some(reason) = reconciled {
        state_payload["reconciled"] = json!(reason);
    }
    let events = [
        failure_event(state, &terminal, &dead_letter, next_effect.as_ref(),
            code)?,
        planned_event(field(before, "competition_id"), "instance_lease", lease_id,
            "competition.instance.state_changed", command_id, state_payload),
    ];
    if put_records_with_events(state, field(before, "competition_id"),
        &expected, &records, &events)? {
        Ok(Some(terminal))
    } else {
        Ok(None)
    }
}

fn settle(
    state: &AppState,
    before: &Value,
    outbox: &Value,
    mut lease: Value,
    result: Option<&Value>,
    receipt: &str,
) -> ApiResult<Value> {
    let lease_id = field(before, "lease_id");
    let outbox_id = field(outbox, "outbox_id");
    let mut delivered = outbox.clone();
    let from = field(before, "state").to_owned();
    let op = field(outbox, "op");
    let hard = if let Some(result) = result {
        apply_result(state, &mut lease, result, op == "ensure")?
    } else {
        false
    };
    lease["state"] = json!(if op == "stop" { "released" } else { "active" });
    lease["updated_at"] = json!(now());
    if op == "stop" {
        lease["release_receipt"] = json!(receipt);
    }
    delivered["status"] = json!("delivered");
    delivered["release_receipt"] = if op == "stop" {
        json!(receipt)
    } else {
        Value::Null
    };
    delivered["updated_at"] = json!(now());
    if op == "renew" || op == "probe" {
        let prior_expiry = before["expires_at"]
            .as_str()
            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok());
        let next_expiry = lease["expires_at"]
            .as_str()
            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok());
        if prior_expiry
            .zip(next_expiry)
            .is_some_and(|(prior, next)| next <= prior)
        {
            lease["renew_deadline_at"] = lease["expires_at"].clone();
        }
    }
    let identity_changed = result.is_some()
        && (op == "ensure" || before["platform_instance_id"] != lease["platform_instance_id"]);
    let fencing_key = if identity_changed {
        format!(
            "{}\0{}",
            field(&lease, "connection_id"),
            field(&lease, "platform_instance_id")
        )
    } else {
        String::new()
    };
    let fencing_head = if identity_changed {
        state
            .store
            .get(FENCING, &fencing_key)
            .map_err(ApiError::internal)?
    } else {
        None
    };
    let next_fencing = if identity_changed {
        let peers = state.store.list(LEASES).map_err(ApiError::internal)?;
        let token = peers
            .iter()
            .filter(|row| {
                row["connection_id"] == lease["connection_id"]
                    && row["platform_instance_id"] == lease["platform_instance_id"]
            })
            .filter_map(|row| row["fencing_token"].as_u64())
            .max()
            .unwrap_or(0)
            .max(
                fencing_head
                    .as_ref()
                    .and_then(|row| row["fencing_token"].as_u64())
                    .unwrap_or(0),
            )
            + 1;
        let generation = peers
            .iter()
            .filter(|row| {
                row["connection_id"] == lease["connection_id"]
                    && row["platform_instance_id"] == lease["platform_instance_id"]
                    && row["lease_id"] != lease["lease_id"]
            })
            .filter_map(|row| row["generation"].as_u64())
            .max()
            .unwrap_or(0)
            .max(
                fencing_head
                    .as_ref()
                    .and_then(|row| row["generation"].as_u64())
                    .unwrap_or(0),
            )
            .max(before["generation"].as_u64().unwrap_or(0))
            + 1;
        lease["fencing_token"] = json!(token);
        lease["generation"] = json!(generation);
        json!({"fencing_token":token,"generation":generation,"lease_id":lease_id,"updated_at":now()})
    } else {
        Value::Null
    };
    let mut expected = vec![
        (LEASES, lease_id, Some(before)),
        (OUTBOX, outbox_id, Some(outbox)),
    ];
    let mut records = vec![(LEASES, lease_id, &lease), (OUTBOX, outbox_id, &delivered)];
    let command_id = field(outbox, "command_id");
    let effect_key = effect_id(outbox);
    let previous_effect = ensure_effect(state, &lease, outbox)?;
    let next_effect = previous_effect.as_ref().map(|prior| {
        let mut next = prior.clone();
        next["state"] = json!("completed");
        next["attempts"] = outbox["attempts"].clone();
        next["retry_after"] = Value::Null;
        next["error"] = Value::Null;
        next["output"] = if op == "stop" {
            json!({"lease_id":lease_id,"state":lease["state"]})
        } else {
            json!({"lease_id":lease_id,"state":lease["state"],"address":lease["address"],
                "fencing_token":lease["fencing_token"],"generation":lease["generation"]})
        };
        next["updated_at"] = json!(now());
        next["completed_at"] = json!(now());
        next
    });
    let previous_command = if command_id.is_empty() {
        None
    } else {
        state
            .store
            .get(COMMANDS, command_id)
            .map_err(ApiError::internal)?
    };
    let next_command = previous_command.as_ref().map(|before| {
        let mut next = before.clone();
        let receipt = &mut next["response"]["receipt"];
        receipt["state"] = json!("completed");
        receipt["completed_at"] = json!(now());
        receipt["error"] = Value::Null;
        receipt["effect_ids"] = json!([effect_id(outbox)]);
        receipt["output"] = if op == "stop" {
            json!({"lease_id":lease_id,"state":lease["state"],"outbox_ids":[outbox_id],"effect_ids":[effect_id(outbox)],"effect_state":"completed"})
        } else {
            json!({"lease_id":lease_id,"state":lease["state"],"address":lease["address"],
                "fencing_token":lease["fencing_token"],"generation":lease["generation"],
                "outbox_ids":[outbox_id],"effect_ids":[effect_id(outbox)],"effect_state":"completed"})
        };
        next["status"] = json!(StatusCode::OK.as_u16());
        next
    });
    let previous_claim = if command_id.is_empty() { None } else {
        state.store.get("platform_command_claims", command_id).map_err(ApiError::internal)?
    };
    let next_claim = previous_claim.as_ref().map(|prior| {
        let mut next = prior.clone();
        next["state"] = json!("completed");
        next["updated_at"] = json!(now());
        next
    });
    if let (Some(prior), Some(next)) = (&previous_effect, &next_effect) {
        expected.push((EFFECTS, effect_key.as_str(), Some(prior)));
        records.push((EFFECTS, effect_key.as_str(), next));
    }
    if let (Some(before), Some(next)) = (&previous_command, &next_command) {
        expected.push((COMMANDS, command_id, Some(before)));
        records.push((COMMANDS, command_id, next));
    }
    if let (Some(before), Some(next)) = (&previous_claim, &next_claim) {
        expected.push(("platform_command_claims", command_id, Some(before)));
        records.push(("platform_command_claims", command_id, next));
    }
    if identity_changed {
        expected.push((FENCING, fencing_key.as_str(), fencing_head.as_ref()));
        records.push((FENCING, fencing_key.as_str(), &next_fencing));
    }
    let competition_id = field(&lease, "competition_id");
    let output = next_effect.as_ref().map_or_else(
        || if op == "stop" {
            json!({"lease_id":lease_id,"state":lease["state"]})
        } else {
            json!({"lease_id":lease_id,"state":lease["state"],"address":lease["address"],
                "fencing_token":lease["fencing_token"],"generation":lease["generation"]})
        },
        |effect| effect["output"].clone(),
    );
    let events = [
        lifecycle_event(state, &lease, &delivered, next_effect.as_ref(),
            "competition.effect.completed",
            json!({"attempt":delivered["attempts"],"output":output}))?,
        planned_event(competition_id, "instance_lease", lease_id,
            "competition.instance.state_changed", command_id,
            json!({"lease_id":lease_id,"from":from,"to":lease["state"],
                "release_receipt":if op == "stop" {json!(receipt)} else {Value::Null},
                "hard_change":hard})),
    ];
    if !put_records_with_events(state, competition_id, &expected, &records, &events)? {
        return Err(conflict(
            "competition.lease.concurrent_update",
            "Instance lease changed concurrently",
        ));
    }
    if op=="stop" {
        if let Err(error)=restore_released_challenge(state,&lease){
            tracing::warn!(lease_id,code=%error.code,"Released instance challenge projection awaits maintenance");
        }
    }
    Ok(lease)
}

fn restore_released_challenge(state:&AppState,released:&Value)->ApiResult<()> {
    let lease_id=field(released,"lease_id");
    let challenge_id=field(released,"competition_challenge_id");
    let competition_id=field(released,"competition_id");
    for _ in 0..16 {
        let lease=state.store.get(LEASES,lease_id).map_err(ApiError::internal)?;
        if lease.as_ref().is_none_or(|lease|!matches!(field(lease,"state"),"released"|"failed")){return Ok(());}
        let head=state.store.get(HEADS,challenge_id).map_err(ApiError::internal)?;
        if head.as_ref().is_none_or(|head|head["lease_id"]!=lease_id){return Ok(());}
        let Some(challenge)=state.store.get(CHALLENGES,challenge_id).map_err(ApiError::internal)? else{return Ok(());};
        if challenge["state"]!="provisioning" || challenge["competition_id"]!=competition_id || challenge["tombstoned"]==true{return Ok(());}
        let queue=state.store.get(QUEUE,challenge_id).map_err(ApiError::internal)?;
        if queue.as_ref().is_none_or(|queue|queue["state"]!="queued"){return Ok(());}
        if scoped(state,BINDINGS,competition_id)?.iter().any(|binding|binding["competition_challenge_id"]==challenge_id && matches!(field(binding,"state"),"planned"|"creating"|"starting"|"running"|"resolving"|"paused")){return Ok(());}
        let mut next=challenge.clone();next["state"]=json!("queued");next["paused_from"]=Value::Null;next["updated_at"]=json!(now());
        let event=planned_event(competition_id,"challenge",challenge_id,"competition.challenge.state_changed",field(released,"release_command_id"),json!({"challenge_id":challenge_id,"from":"provisioning","to":"queued","reason":if released["state"]=="failed"{"unbound_instance_failed"}else{"unbound_instance_released"},"lease_id":lease_id}));
        if put_records_with_events(state,competition_id,&[(LEASES,lease_id,lease.as_ref()),(HEADS,challenge_id,head.as_ref()),(QUEUE,challenge_id,queue.as_ref()),(CHALLENGES,challenge_id,Some(&challenge))],&[(CHALLENGES,challenge_id,&next)],&[event])?{return Ok(());}
    }
    Err(conflict("competition.lease.concurrent_update","Released instance challenge changed concurrently"))
}

async fn resolve_hard_change(
    state: &Arc<AppState>,
    before: &Value,
    lease: &Value,
) -> ApiResult<()> {
    if before["platform_instance_id"] == lease["platform_instance_id"]
        && before["address"] == lease["address"]
    {
        return Ok(());
    }
    let competition_id = field(lease, "competition_id");
    let challenge_id = field(lease, "competition_challenge_id");
    let Some(binding) = scoped(state, BINDINGS, competition_id)?
        .into_iter()
        .find(|row| {
            row["competition_challenge_id"] == challenge_id
                && matches!(field(row, "state"), "running" | "paused" | "rejected")
        })
    else {
        return Ok(());
    };
    if binding["state"] == "running" {
        binding_command(
            state,
            competition_id,
            "run_binding.pause",
            "",
            &json!({"challenge_id":challenge_id,"binding_id":binding["binding_id"]}),
        )
        .await?;
    }
    let run_id = field(&binding, "run_id");
    let Some(run) = state
        .store
        .get("runs", run_id)
        .map_err(ApiError::internal)?
    else {
        return Ok(());
    };
    let challenge = json!({"mode":run["mode"],"name":run["original_name"].as_str().unwrap_or_else(||field(&run,"name")),
        "category":run["category"],"description":run["description"],"target":lease["address"],
        "flag_format":run["flag_format"],"expected_flags":run["expected_flags"],"multi_flag":run["multi_flag"],
        "attachments":run["attachments"]});
    binding_command(state,competition_id,"run_binding.resolve","",
        &json!({"challenge_id":challenge_id,"binding_id":binding["binding_id"],"challenge":challenge})).await?;
    Ok(())
}

async fn deliver(state: &Arc<AppState>, outbox: Value) -> ApiResult<Value> {
    let lease_id = field(&outbox, "lease_id");
    let outbox_id = field(&outbox, "outbox_id");
    let _lock = lease_lock(state, lease_id).await?;
    let Some(before) = state
        .store
        .get(LEASES, lease_id)
        .map_err(ApiError::internal)?
    else {
        return Err(ApiError::missing("instance lease"));
    };
    let Some(outbox) = state
        .store
        .get(OUTBOX, outbox_id)
        .map_err(ApiError::internal)?
    else {
        return Err(ApiError::missing("instance effect"));
    };
    if outbox["status"] != "pending" && !(outbox["status"] == "failed" && retry_due(&outbox)) {
        return Ok(before);
    }
    if superseded(state,&before)?{return retire_superseded(state,&before).await;}
    let effect = ensure_effect(state, &before, &outbox)?;
    let effect_ids = effect.as_ref().map(|_| json!([effect_id(&outbox)])).unwrap_or_else(|| json!([]));
    let effect_key = effect_id(&outbox);
    let op = field(&outbox, "op").to_owned();
    let mut claimed = outbox.clone();
    claimed["status"] = json!("processing");
    claimed["attempts"] = json!(outbox["attempts"].as_u64().unwrap_or(0) + 1);
    claimed["next_attempt_at"] = Value::Null;
    claimed["updated_at"] = json!(now());
    let mut in_flight = before.clone();
    if op == "ensure" {
        in_flight["state"] = json!("provisioning");
    }
    in_flight["updated_at"] = json!(now());
    let running_effect = effect.as_ref().map(|prior| {
        let mut next = prior.clone();
        next["state"] = json!("running");
        next["attempts"] = claimed["attempts"].clone();
        next["retry_after"] = Value::Null;
        next["error"] = Value::Null;
        next["updated_at"] = json!(now());
        next
    });
    let mut expected = vec![(LEASES, lease_id, Some(&before)), (OUTBOX, outbox_id, Some(&outbox))];
    let mut records = vec![(LEASES, lease_id, &in_flight), (OUTBOX, outbox_id, &claimed)];
    if let (Some(prior), Some(next)) = (&effect, &running_effect) {
        expected.push((EFFECTS, effect_key.as_str(), Some(prior)));
        records.push((EFFECTS, effect_key.as_str(), next));
    }
    let event = lifecycle_event(state, &in_flight, &claimed, running_effect.as_ref(),
        "competition.effect.running", json!({"attempt":claimed["attempts"]}))?;
    if !put_records_with_events(state, field(&in_flight, "competition_id"),
        &expected, &records, &[event])? {
        return Err(conflict(
            "competition.lease.effect_claimed",
            "Instance effect was claimed by another process",
        ));
    }
    let command_id = field(&claimed, "command_id");
    if !command_id.is_empty() {
        let _ = update_receipt(
            state,
            command_id,
            "running",
            json!({"lease_id":lease_id,"outbox_ids":[outbox_id],"effect_ids":effect_ids,"effect_state":"running"}),
            None,
        );
    }
    let remote_result = remote(state, &in_flight, &op).await;
    if superseded(state,&in_flight)?{return retire_superseded(state,&in_flight).await;}
    match remote_result {
        Ok((status, _)) if status == StatusCode::NOT_FOUND && op == "stop" => settle(
            state,
            &in_flight,
            &claimed,
            in_flight.clone(),
            None,
            "platform_not_found",
        ),
        Ok((status, _)) if status == StatusCode::NOT_FOUND => {
            terminal_not_found(state, &in_flight, &claimed,
                if op == "ensure" { "failed" } else { "lost" }, None)?
                .ok_or_else(|| conflict("competition.lease.concurrent_update",
                    "Instance lease changed concurrently"))?;
            Err(ApiError::missing("remote instance"))
        }
        Ok((_, _)) if op == "stop" => settle(
            state,
            &in_flight,
            &claimed,
            in_flight.clone(),
            None,
            "confirmed",
        ),
        Ok((_, result)) => {
            let settled = settle(
                state,
                &in_flight,
                &claimed,
                in_flight.clone(),
                Some(&result),
                "",
            )?;
            resolve_hard_change(state, &in_flight, &settled).await?;
            Ok(settled)
        }
        Err(error) => {
            if op == "ensure" && acquire_rejected(&error) {
                terminal_failure(state, &in_flight, &claimed, "failed", None,
                    error.code, &error.message)?
                    .ok_or_else(|| conflict("competition.lease.concurrent_update", "Instance lease changed concurrently"))?;
                return Err(error);
            }
            let auth = is_auth_error(error.code);
            let mut unknown = claimed.clone();
            unknown["last_error"] = json!(error.code);
            unknown["updated_at"] = json!(now());
            let mut lease = in_flight.clone();
            lease["last_error"] = json!(error.message);
            lease["updated_at"] = json!(now());
            if op != "ensure" || auth {
                let attempts = claimed["attempts"].as_u64().unwrap_or(1);
                unknown["status"] = json!(if attempts >= 8 {
                    "dead_letter"
                } else if auth {
                    "paused"
                } else {
                    "failed"
                });
                unknown["next_attempt_at"] = if attempts >= 8 || auth {
                    Value::Null
                } else {
                    retry_after(attempts)
                };
                if op == "renew" {
                    lease["state"] = json!("active");
                }
            }
            let events = if op == "ensure" && !auth { Vec::new() } else {
                vec![failure_event(state, &lease, &unknown, running_effect.as_ref(), error.code)?]
            };
            let previous_connection = if auth {
                state.store.get(CONNECTIONS, field(&lease, "connection_id"))
                    .map_err(ApiError::internal)?
            } else { None };
            let next_connection = previous_connection.as_ref().map(|prior| {
                let mut next = prior.clone();
                next["status"] = json!("auth_required");
                next["last_error"] = json!(error.message);
                next["updated_at"] = json!(now());
                next
            });
            let effect_state = if unknown["status"] == "dead_letter" { "dead_letter" }
                else if op == "ensure" && !auth { "running" } else { "waiting" };
            let next_effect = running_effect.as_ref().map(|prior| effect_transition(
                prior, &unknown, effect_state, json!({"lease_id":lease_id}),
                Some((error.code, &error.message)),
            ));
            let command_state = if unknown["status"] == "dead_letter" { "failed" }
                else if op == "ensure" && !auth { "running" } else { "waiting" };
            let previous_command = if command_id.is_empty() { None } else {
                state.store.get(COMMANDS, command_id).map_err(ApiError::internal)?
            };
            let next_command = previous_command.as_ref().map(|prior| receipt_transition(
                prior, command_state,
                json!({"lease_id":lease_id,"outbox_ids":[outbox_id],
                    "effect_ids":effect_ids,"effect_state":effect_state}),
                if command_state == "running" { None } else { Some((error.code, &error.message)) },
            ));
            let previous_claim = if command_id.is_empty() { None } else {
                state.store.get("platform_command_claims", command_id)
                    .map_err(ApiError::internal)?
            };
            let next_claim = previous_claim.as_ref().map(|prior| {
                let mut next = prior.clone();
                next["state"] = json!(if command_state == "failed" { "failed" } else { "accepted" });
                next["updated_at"] = json!(now());
                next
            });
            let mut expected = vec![(LEASES, lease_id, Some(&in_flight)),
                (OUTBOX, outbox_id, Some(&claimed))];
            let mut records = vec![(LEASES, lease_id, &lease), (OUTBOX, outbox_id, &unknown)];
            if let (Some(prior), Some(next)) = (&previous_connection, &next_connection) {
                expected.push((CONNECTIONS, field(&lease, "connection_id"), Some(prior)));
                records.push((CONNECTIONS, field(&lease, "connection_id"), next));
            }
            if let (Some(prior), Some(next)) = (&running_effect, &next_effect) {
                expected.push((EFFECTS, effect_key.as_str(), Some(prior)));
                records.push((EFFECTS, effect_key.as_str(), next));
            }
            if let (Some(prior), Some(next)) = (&previous_command, &next_command) {
                expected.push((COMMANDS, command_id, Some(prior)));
                records.push((COMMANDS, command_id, next));
            }
            if let (Some(prior), Some(next)) = (&previous_claim, &next_claim) {
                expected.push(("platform_command_claims", command_id, Some(prior)));
                records.push(("platform_command_claims", command_id, next));
            }
            let changed = put_records_with_events(state, field(&lease, "competition_id"),
                &expected, &records,
                &events)?;
            if !changed {
                return Err(conflict("competition.lease.concurrent_update", "Instance lease changed concurrently"));
            }
            Err(error)
        }
    }
}

async fn recover_accepted(state: &Arc<AppState>, command_id: &str) -> ApiResult<Value> {
    let command = state
        .store
        .get(COMMANDS, command_id)
        .map_err(ApiError::internal)?
        .ok_or_else(|| ApiError::missing("instance command"))?;
    let lease_id = field(&command["response"]["receipt"]["aggregate"], "id").to_owned();
    let _lock = lease_lock(state, &lease_id).await?;
    let command = state
        .store
        .get(COMMANDS, command_id)
        .map_err(ApiError::internal)?
        .ok_or_else(|| ApiError::missing("instance command"))?;
    if command["response"]["receipt"]["state"] != "accepted" {
        return Ok(command["response"]["receipt"].clone());
    }
    let kind = field(&command["request"], "command_type");
    let competition_id = field(&command["request"], "competition_id");
    let payload = &command["request"]["payload"];
    let applied = (|| -> ApiResult<Value> {
        if kind == "instance.ensure" {
            let (lease, outbox) = if let Some(lease) = state
                .store
                .get(LEASES, &lease_id)
                .map_err(ApiError::internal)?
            {
                if let Some(outbox) = lease_outbox(state, &lease_id)? {
                    (lease, outbox)
                } else if lease["state"] == "active" {
                    return update_receipt(state, command_id, "completed",
                        json!({"lease_id":lease_id,"state":"active"}), None);
                } else {
                    return Err(conflict("competition.lease.outbox_missing",
                        "Instance lease has no pending outbox"));
                }
            } else {
                let competition = live_competition(state, competition_id)?;
                let connection = live_connection(state, field(&competition, "connection_id"))?;
                let challenge_id = field(payload, "challenge_id");
                let challenge = state
                    .store
                    .get(CHALLENGES, challenge_id)
                    .map_err(ApiError::internal)?
                    .filter(|row| row["competition_id"] == competition_id)
                    .ok_or_else(|| ApiError::missing("challenge"))?;
                stage_ensure(state, competition_id, &challenge, &connection,
                    command_id, &lease_id, field(payload, "owner"),
                    payload["ttl_seconds"].as_u64().unwrap_or(0))?
            };
            let effect = ensure_effect(state, &lease, &outbox)?;
            let effect_ids = effect.map(|row| json!([row["effect_id"]]))
                .unwrap_or_else(|| json!([]));
            update_receipt(state, command_id, "waiting",
                json!({"lease_id":lease_id,"outbox_ids":[outbox["outbox_id"]],
                    "effect_ids":effect_ids,"effect_state":"waiting"}), None)
        } else if kind == "instance.stop" {
            let lease = state
                .store
                .get(LEASES, &lease_id)
                .map_err(ApiError::internal)?
                .ok_or_else(|| ApiError::missing("instance lease"))?;
            if lease["state"] == "released" && lease["release_command_id"] == command_id {
                return update_receipt(state, command_id, "completed",
                    json!({"lease_id":lease_id,"state":"released"}), None);
            }
            let (lease, outbox) = stage_stop(state, &lease, command_id)?;
            if let Some(outbox) = outbox {
                let effect = ensure_effect(state, &lease, &outbox)?;
                let effect_ids = effect.map(|row| json!([row["effect_id"]]))
                    .unwrap_or_else(|| json!([]));
                update_receipt(state, command_id, "waiting",
                    json!({"lease_id":lease_id,"outbox_ids":[outbox["outbox_id"]],
                        "effect_ids":effect_ids,"effect_state":"waiting"}), None)
            } else {
                update_receipt(state, command_id, "completed",
                    json!({"lease_id":lease_id,"state":lease["state"]}), None)
            }
        } else {
            Err(ApiError::invalid("Unknown instance command"))
        }
    })();
    match applied {
        Ok(receipt) => Ok(receipt),
        Err(error) if error.code == "competition.storage" => Err(error),
        Err(error) => update_receipt(state, command_id, "failed",
            json!({"lease_id":lease_id}), Some((error.code, &error.message))),
    }
}

pub(super) async fn command(
    state: &Arc<AppState>,
    competition_id: &str,
    kind: &str,
    command_id: &str,
    payload: &Value,
    admission: &Value,
) -> ApiResult<Value> {
    if !command_id.is_empty()
        && let Some(previous) = state
            .store
            .get(COMMANDS, command_id)
            .map_err(ApiError::internal)?
    {
        let request =
            json!({"command_type":kind,"competition_id":competition_id,"payload":payload});
        if previous["request"] != request {
            return Err(conflict(
                "competition.command.idempotency_conflict",
                "Command ID was used with different input",
            ));
        }
        let mut receipt = if previous["response"]["receipt"]["state"] == "accepted" {
            recover_accepted(state, command_id).await?
        } else {
            previous["response"]["receipt"].clone()
        };
        receipt["deduplicated"] = json!(true);
        return Ok(receipt);
    }
    let competition = live_competition(state, competition_id)?;
    let connection = live_connection(state, field(&competition, "connection_id"))?;
    if kind == "instance.ensure" {
        let challenge_id = field(payload, "challenge_id");
        let challenge = state
            .store
            .get(CHALLENGES, challenge_id)
            .map_err(ApiError::internal)?
            .filter(|row| row["competition_id"] == competition_id)
            .ok_or_else(|| ApiError::missing("challenge"))?;
        if state.store.get(REVISIONS,field(&challenge,"current_revision_id"))
            .map_err(ApiError::internal)?.is_some_and(|revision|revision["instance_required"]==false) {
            return Err(ApiError::invalid("This challenge does not use a dynamic instance"));
        }
        if let Some(existing) = active_for(state, competition_id, challenge_id)? {
            if command_id.is_empty() && existing["state"] == "active" {
                return receipt(state, command_id, kind, &existing);
            }
            return Err(conflict(
                "competition.lease.active_exists",
                "Challenge already has an active lease",
            ));
        }
        quota(state, competition_id, &connection, challenge_id)?;
        let lease_id = id("lease");
        let planned_id = if command_id.is_empty() {
            id("cmd")
        } else {
            command_id.to_owned()
        };
        let mut events = vec![planned_event(
            competition_id,
            "instance_lease",
            &lease_id,
            "competition.instance.lease_requested",
            &planned_id,
            json!({"lease_id":lease_id,"challenge_id":challenge_id,"competition_id":competition_id}),
        )];
        if challenge["state"] == "queued" {
            events.push(planned_event(
                competition_id,
                "challenge",
                challenge_id,
                "competition.challenge.state_changed",
                &planned_id,
                json!({"challenge_id":challenge_id,"from":"queued","to":"provisioning"}),
            ));
        }
        let (stored_id, accepted) = record_plan(
            state,
            competition_id,
            kind,
            &planned_id,
            &lease_id,
            payload,
            &events,
            admission,
        )?;
        let mut receipt = recover_accepted(state, &stored_id).await?;
        if accepted["deduplicated"] == true {
            receipt["deduplicated"] = json!(true);
        }
        Ok(receipt)
    } else {
        let lease = if !field(payload, "lease_id").is_empty() {
            state
                .store
                .get(LEASES, field(payload, "lease_id"))
                .map_err(ApiError::internal)?
                .filter(|row| row["competition_id"] == competition_id)
        } else {
            active_for(state, competition_id, field(payload, "challenge_id"))?
        }
        .ok_or_else(|| ApiError::missing("instance lease"))?;
        if !matches!(field(&lease, "state"), "requested" | "active" | "releasing") {
            return Err(conflict(
                "competition.lease.illegal_transition",
                "instance.stop is not allowed from the current lease state",
            ));
        }
        if let Some(expected) = payload["expected_fencing_token"]
            .as_u64()
            .or_else(|| payload["fencing_token"].as_u64())
        {
            if lease["fencing_token"].as_u64() != Some(expected) {
                return Err(conflict(
                    "competition.lease.stale_fencing",
                    "Stale instance fencing token",
                ));
            }
        }
        let lease_id = field(&lease, "lease_id");
        let planned_id = if command_id.is_empty() {
            id("cmd")
        } else {
            command_id.to_owned()
        };
        let events = vec![planned_event(
            competition_id,
            "instance_lease",
            lease_id,
            "competition.instance.release_requested",
            &planned_id,
            json!({"lease_id":lease_id,"from":lease["state"],"to":if lease["state"] == "requested" {"released"} else {"releasing"}}),
        )];
        let (stored_id, accepted) = record_plan(
            state,
            competition_id,
            kind,
            &planned_id,
            lease_id,
            payload,
            &events,
            admission,
        )?;
        let mut receipt = recover_accepted(state, &stored_id).await?;
        if accepted["deduplicated"] == true {
            receipt["deduplicated"] = json!(true);
        }
        Ok(receipt)
    }
}

fn pause_intent(state: &AppState, lease: &Value) -> ApiResult<Option<Value>> {
    let competition_id = field(lease,"competition_id");
    let challenge_id = field(lease,"competition_challenge_id");
    let binding = scoped(state,BINDINGS,competition_id)?.into_iter().find(|binding|
        binding["competition_challenge_id"] == challenge_id
        && matches!(field(binding,"state"),"running"|"active"|"paused"));
    let Some(binding) = binding else { return Ok(None) };
    let binding_id = field(&binding,"binding_id");
    let generation = binding["execution_generation"].as_u64().unwrap_or(0);
    let previous = &lease["reconcile_pause"];
    if previous["binding_id"] == binding_id && previous["execution_generation"] == generation
        && matches!(field(previous,"state"),"pending"|"confirmed") {
        return Ok(Some(previous.clone()));
    }
    let prior = state.store.list("competition_binding_outbox_live").map_err(ApiError::internal)?
        .into_iter().filter(|outbox|outbox["binding_id"] == binding_id
            && outbox["execution_generation"] == generation && outbox["op"] == "run_binding.pause")
        .max_by(|left,right|field(left,"created_at").cmp(field(right,"created_at")));
    let command_id = prior.as_ref().map(|outbox|field(outbox,"command_id")).filter(|id|!id.is_empty())
        .map(str::to_owned).unwrap_or_else(||format!("lease-degrade-pause:{}:{binding_id}:{generation}",field(lease,"lease_id")));
    Ok(Some(json!({"state":"pending","binding_id":binding_id,"run_id":binding["run_id"],
        "execution_generation":generation,"command_id":command_id,"requested_at":now()})))
}

async fn recover_pause(state: &Arc<AppState>, lease: &Value) -> ApiResult<()> {
    let pause = &lease["reconcile_pause"];
    if pause["state"] != "pending" { return Ok(()); }
    let lease_id = field(lease,"lease_id");
    let binding_id = field(pause,"binding_id");
    let command_id = field(pause,"command_id");
    if binding_id.is_empty() || command_id.is_empty() {
        return Err(conflict("competition.lease.pause_intent_invalid","Lease pause intent has no stable command identity"));
    }
    let binding = state.store.get(BINDINGS,binding_id).map_err(ApiError::internal)?;
    let command = state.store.get(COMMANDS,command_id).map_err(ApiError::internal)?;
    let mut next_pause = pause.clone();
    if command.as_ref().is_some_and(|command|command["response"]["receipt"]["state"] == "completed") {
        next_pause["state"] = json!("confirmed");
        next_pause["confirmed_at"] = json!(now());
        next_pause["last_error"] = Value::Null;
    } else if binding.as_ref().is_none_or(|binding|binding["execution_generation"] != pause["execution_generation"]
        || binding["run_id"] != pause["run_id"]) {
        next_pause["state"] = json!("superseded");
        next_pause["last_error"] = json!("The bound execution changed before pause confirmation");
    } else if let Some(command) = command {
        binding_outbox::recover_missing(state,command_id)?;
        next_pause["receipt_state"] = command["response"]["receipt"]["state"].clone();
        next_pause["last_error"] = command["response"]["receipt"]["error"].clone();
    } else {
        let result = Box::pin(binding_command(state,field(lease,"competition_id"),"run_binding.pause",command_id,
            &json!({"challenge_id":lease["competition_challenge_id"],"binding_id":binding_id,
                "expected_execution_generation":pause["execution_generation"],"expected_run_id":pause["run_id"]}))).await;
        match result {
            Ok(receipt) => {
                next_pause["receipt_state"] = receipt["state"].clone();
                if receipt["state"] == "completed" {
                    next_pause["state"] = json!("confirmed");
                    next_pause["confirmed_at"] = json!(now());
                    next_pause["last_error"] = Value::Null;
                }
            },
            Err(error) => {
                tracing::warn!(lease_id,command_id,code=%error.code,"Degraded lease pause remains durably pending");
                next_pause["last_error"] = json!({"code":error.code,"message":error.message});
            }
        }
    }
    if next_pause == *pause { return Ok(()); }
    for _ in 0..16 {
        let Some(current) = state.store.get(LEASES,lease_id).map_err(ApiError::internal)? else { return Ok(()); };
        if current["reconcile_pause"] != *pause {
            if current["reconcile_pause"] == next_pause { return Ok(()); }
            return Err(conflict("competition.lease.concurrent_update","Lease pause intent changed before settlement"));
        }
        let mut updated = current.clone();
        updated["reconcile_pause"] = next_pause.clone();
        updated["updated_at"] = json!(now());
        let events = if next_pause["state"] != pause["state"] {
            vec![planned_event(field(&current,"competition_id"),"instance_lease",lease_id,
                "competition.instance.pause_settled",command_id,json!({"lease_id":lease_id,"pause":next_pause}))]
        } else { Vec::new() };
        if put_records_with_events(state,field(&current,"competition_id"),
            &[(LEASES,lease_id,Some(&current))],&[(LEASES,lease_id,&updated)],&events)? { return Ok(()); }
    }
    Err(conflict("competition.lease.concurrent_update","Lease changed repeatedly during pause settlement"))
}

async fn degrade(state: &Arc<AppState>, lease: &Value, reason: &str) -> ApiResult<()> {
    let competition_id = field(lease, "competition_id");
    let lease_id = field(lease,"lease_id");
    let mut degraded = lease.clone();
    degraded["reconcile_degraded"] = json!(true);
    degraded["last_error"] = json!(reason);
    if let Some(pause) = pause_intent(state,lease)? { degraded["reconcile_pause"] = pause; }
    if degraded != *lease {
        degraded["updated_at"] = json!(now());
        let event = planned_event(competition_id,"instance_lease",lease_id,
            "competition.instance.reconcile_degraded","",json!({"lease_id":lease_id,
                "challenge_id":lease["competition_challenge_id"],"reason":reason,"pause":degraded["reconcile_pause"]}));
        if !put_records_with_events(state,competition_id,&[(LEASES,lease_id,Some(lease))],
            &[(LEASES,lease_id,&degraded)],&[event])? {
            return Err(conflict("competition.lease.concurrent_update","Lease changed before degradation was committed"));
        }
    }
    recover_pause(state,&degraded).await
}

async fn release_terminal_after_reconcile(state: &Arc<AppState>, lease: &Value) -> ApiResult<()> {
    let Some(challenge) = state
        .store
        .get(CHALLENGES, field(lease, "competition_challenge_id"))
        .map_err(ApiError::internal)?
    else {
        return Ok(());
    };
    if !matches!(
        field(&challenge, "state"),
        "solved" | "skipped" | "exhausted" | "failed" | "retired"
    ) {
        return Ok(());
    }
    let (releasing, outbox) = stage_stop(state, lease, "")?;
    let Some(outbox) = outbox else {
        return Ok(());
    };
    let outbox = claim_replay(state, &releasing, &outbox)?;
    let result = remote(state, &releasing, "stop").await;
    match result {
        Ok((status, _)) => {
            let _ = settle(
                state,
                &releasing,
                &outbox,
                releasing.clone(),
                None,
                if status == StatusCode::NOT_FOUND {
                    "platform_not_found"
                } else {
                    "confirmed"
                },
            )?;
        }
        Err(error) => {
            schedule_retry(state, &outbox, &error)?;
            degrade(state, &releasing, "terminal_release_failed").await?;
        }
    }
    Ok(())
}

pub(super) async fn reconcile_startup(state: &Arc<AppState>) -> ApiResult<()> {
    for command in state.store.list(COMMANDS).map_err(ApiError::internal)? {
        if !matches!(field(&command["request"], "command_type"), "instance.ensure" | "instance.stop")
            || command["response"]["receipt"]["state"] != "accepted"
        {
            continue;
        }
        let command_id = field(&command["response"]["receipt"], "command_id");
        if let Err(error) = recover_accepted(state, command_id).await {
            tracing::warn!(command_id,code=%error.code,
                "Instance local effect recovery remains pending");
        }
    }
    for candidate in state.store.list(LEASES).map_err(ApiError::internal)? {
        let lease_id = field(&candidate, "lease_id").to_owned();
        let _lock = lease_lock(state, &lease_id).await?;
        let Some(lease) = state
            .store
            .get(LEASES, &lease_id)
            .map_err(ApiError::internal)?
        else {
            continue;
        };
        if superseded(state,&lease)?{retire_superseded(state,&lease).await?;continue;}
        recover_pause(state,&lease).await?;
        let Some(lease) = state.store.get(LEASES,&lease_id).map_err(ApiError::internal)? else { continue };
        let op = field(&lease, "state");
        if !occupies(&lease) {
            continue;
        }
        let open_outbox = lease_outbox(state, &lease_id)?;
        if open_outbox.as_ref().is_some_and(|outbox| outbox["status"] == "paused") {
            continue;
        }
        if op == "provisioning"
            && open_outbox.as_ref().is_some_and(|outbox| outbox["status"] == "pending")
        {
            continue;
        }
        if op == "requested" {
            if open_outbox.as_ref().is_some_and(|outbox|
                outbox["status"] == "pending" && outbox["attempts"].as_u64() == Some(0)) {
                continue;
            }
            let mut failed = lease.clone();
            failed["state"] = json!("failed");
            failed["last_error"] = json!("restart reconcile: acquire never dispatched");
            failed["updated_at"] = json!(now());
            let pending = open_outbox.as_ref();
            let mut cancelled = pending.cloned();
            if let Some(outbox) = cancelled.as_mut() {
                outbox["status"] = json!("cancelled");
                outbox["next_attempt_at"] = Value::Null;
                outbox["updated_at"] = json!(now());
            }
            let previous_effect = if let Some(outbox) = pending {
                ensure_effect(state, &lease, outbox)?
            } else { None };
            let next_effect = previous_effect.as_ref().zip(cancelled.as_ref()).map(|(prior, outbox)| {
                effect_transition(prior, outbox, "failed",
                    json!({"lease_id":lease_id,"state":"failed"}),
                    Some(("competition.lease.never_dispatched",
                        "Instance request was not dispatched before restart")))
            });
            let command_id = pending.map(|outbox| field(outbox, "command_id")).unwrap_or("");
            let previous_command = if command_id.is_empty() { None } else {
                state.store.get(COMMANDS, command_id).map_err(ApiError::internal)?
            };
            let next_command = previous_command.as_ref().map(|prior| receipt_transition(
                prior, "failed", json!({"lease_id":lease_id,"state":"failed"}),
                Some(("competition.lease.never_dispatched",
                    "Instance request was not dispatched before restart")),
            ));
            let previous_claim = if command_id.is_empty() { None } else {
                state.store.get("platform_command_claims", command_id)
                    .map_err(ApiError::internal)?
            };
            let next_claim = previous_claim.as_ref().map(|prior| {
                let mut next = prior.clone();
                next["state"] = json!("failed");
                next["updated_at"] = json!(now());
                next
            });
            let mut expected = vec![(LEASES, lease_id.as_str(), Some(&lease))];
            let mut records = vec![(LEASES, lease_id.as_str(), &failed)];
            if let (Some(prior), Some(next)) = (pending, cancelled.as_ref()) {
                expected.push((OUTBOX, field(prior, "outbox_id"), Some(prior)));
                records.push((OUTBOX, field(next, "outbox_id"), next));
            }
            let effect_key = pending.map(effect_id).unwrap_or_default();
            if let (Some(prior), Some(next)) = (&previous_effect, &next_effect) {
                expected.push((EFFECTS, effect_key.as_str(), Some(prior)));
                records.push((EFFECTS, effect_key.as_str(), next));
            }
            if let (Some(prior), Some(next)) = (&previous_command, &next_command) {
                expected.push((COMMANDS, command_id, Some(prior)));
                records.push((COMMANDS, command_id, next));
            }
            if let (Some(prior), Some(next)) = (&previous_claim, &next_claim) {
                expected.push(("platform_command_claims", command_id, Some(prior)));
                records.push(("platform_command_claims", command_id, next));
            }
            let event = planned_event(field(&lease, "competition_id"), "instance_lease",
                &lease_id, "competition.instance.state_changed", "",
                json!({"lease_id":lease_id,"from":"requested","to":"failed",
                    "reconciled":"never_dispatched"}));
            let _ = put_records_with_events(state, field(&lease, "competition_id"),
                &expected, &records, &[event])?;
            continue;
        }
        if op == "releasing" {
            let outbox =
                lease_outbox(state, &lease_id)?.unwrap_or_else(|| new_outbox(&lease, "stop", ""));
            let outbox = claim_replay(state, &lease, &outbox)?;
            let result = remote(state, &lease, "stop").await;
            match result {
                Ok((status, _)) => {
                    settle(
                        state,
                        &lease,
                        &outbox,
                        lease.clone(),
                        None,
                        if status == StatusCode::NOT_FOUND {
                            "platform_not_found"
                        } else {
                            "confirmed"
                        },
                    )?;
                }
                Err(error) => {
                    schedule_retry(state, &outbox, &error)?;
                    degrade(state, &lease, "release_replay_failed").await?;
                }
            }
            continue;
        }
        let result = remote(state, &lease, "probe").await;
        match result {
            Ok((StatusCode::NOT_FOUND, _)) => {
                let pending = lease_outbox(state, &lease_id)?;
                let next_state = if op == "provisioning" { "failed" } else { "lost" };
                if let Some(outbox) = pending.as_ref() {
                    let _ = terminal_not_found(state, &lease, outbox, next_state,
                        Some("not_found"))?;
                } else {
                    let mut terminal = lease.clone();
                    terminal["state"] = json!(next_state);
                    terminal["updated_at"] = json!(now());
                    let event = planned_event(field(&lease, "competition_id"),
                        "instance_lease", &lease_id, "competition.instance.state_changed", "",
                        json!({"lease_id":lease_id,"from":op,"to":next_state,
                            "reconciled":"not_found"}));
                    let _ = put_records_with_events(state, field(&lease, "competition_id"),
                        &[(LEASES, &lease_id, Some(&lease))],
                        &[(LEASES, &lease_id, &terminal)], &[event])?;
                }
            }
            Ok((_, result)) => {
                let outbox = lease_outbox(state, &lease_id)?
                    .unwrap_or_else(|| new_outbox(&lease, "probe", ""));
                if lease_outbox(state, &lease_id)?.is_none() {
                    state
                        .store
                        .put(OUTBOX, field(&outbox, "outbox_id"), &outbox)
                        .map_err(ApiError::internal)?;
                }
                if let Ok(settled) = settle(
                    state,
                    &lease,
                    &outbox,
                    lease.clone(),
                    Some(&result),
                    "confirmed",
                ) {
                    if let Err(error) = resolve_hard_change(state, &lease, &settled).await {
                        tracing::warn!(lease_id,code=%error.code,"Recovered lease changed its bound Run input");
                    }
                    if let Err(error) = release_terminal_after_reconcile(state, &settled).await {
                        tracing::warn!(lease_id,code=%error.code,"Recovered terminal challenge still has an instance");
                    }
                }
            }
            Err(error) if op == "provisioning" && acquire_rejected(&error) => {
                if let Some(outbox)=open_outbox.as_ref() {
                    let _=terminal_failure(state,&lease,outbox,"failed",Some("acquire_rejected"),error.code,&error.message)?;
                }
            }
            Err(_) => {
                degrade(state, &lease, "probe_unknown").await?;
            }
        }
    }
    Ok(())
}

pub(super) async fn maintenance(state: &Arc<AppState>, competition_id: &str) -> ApiResult<()> {
    for candidate in scoped(state, LEASES, competition_id)? {
        if candidate["reconcile_degraded"] == true {
            if let Err(error) = degrade(state,&candidate,field(&candidate,"last_error")).await {
                tracing::warn!(lease_id=%field(&candidate,"lease_id"),code=%error.code,"Lease degradation pause remains pending");
            }
        } else if let Err(error) = recover_pause(state,&candidate).await {
            tracing::warn!(lease_id=%field(&candidate,"lease_id"),code=%error.code,"Lease pause receipt remains pending");
        }
        let Some(lease) = state.store.get(LEASES,field(&candidate,"lease_id")).map_err(ApiError::internal)? else { continue };
        if matches!(field(&lease,"state"),"released"|"failed") {restore_released_challenge(state,&lease)?;continue;}
        if lease["state"] == "requested" {
            if let Some(outbox) = lease_outbox(state, field(&lease, "lease_id"))?
                && outbox["op"] == "ensure"
                && outbox["status"] == "pending"
            {
                if let Err(error) = deliver(state, outbox).await {
                    tracing::warn!(lease_id=%field(&lease,"lease_id"),code=%error.code,"Lease provisioning failed");
                }
            }
            continue;
        }
        if lease["state"] == "provisioning" {
            recover_ambiguous(state, &lease).await?;
            if let Some(outbox) = lease_outbox(state, field(&lease, "lease_id"))?
                && outbox["op"] == "ensure"
                && outbox["status"] == "pending"
            {
                if let Err(error) = deliver(state, outbox).await {
                    tracing::warn!(lease_id=%field(&lease,"lease_id"),code=%error.code,"Lease provisioning retry failed");
                }
            }
            continue;
        }
        if lease["state"] == "releasing" {
            recover_ambiguous(state, &lease).await?;
            if let Some(outbox) = lease_outbox(state, field(&lease, "lease_id"))?
                && outbox["op"] == "stop"
                && (outbox["status"] == "pending"
                    || outbox["status"] == "failed" && retry_due(&outbox))
            {
                if let Err(error) = deliver(state, outbox).await {
                    tracing::warn!(lease_id=%field(&lease,"lease_id"),code=%error.code,"Lease release retry failed");
                }
            }
            continue;
        }
        if !matches!(field(&lease, "state"), "active" | "renewing") {
            continue;
        }
        let due = lease["renew_deadline_at"]
            .as_str()
            .and_then(|text| chrono::DateTime::parse_from_rfc3339(text).ok())
            .is_some_and(|at| at <= chrono::Utc::now());
        let previous = lease_outbox(state, field(&lease, "lease_id"))?
            .filter(|outbox| outbox["op"] == "renew");
        let outbox = match previous {
            Some(outbox) if outbox["status"] == "pending" => Some(outbox),
            Some(outbox) if outbox["status"] == "failed" && retry_due(&outbox) => Some(outbox),
            Some(_) => None,
            None if due && lease["state"] == "active" => stage_renew(state, &lease)?,
            None => None,
        };
        if let Some(outbox) = outbox {
            if let Err(error) = deliver(state, outbox).await {
                tracing::warn!(lease_id=%field(&lease,"lease_id"),code=%error.code,"Lease renewal failed");
            }
        }
        let Some(current) = state
            .store
            .get(LEASES, field(&lease, "lease_id"))
            .map_err(ApiError::internal)?
        else {
            continue;
        };
        let expired = current["expires_at"]
            .as_str()
            .and_then(|text| chrono::DateTime::parse_from_rfc3339(text).ok())
            .is_some_and(|at| at <= chrono::Utc::now());
        if expired && matches!(field(&current, "state"), "active" | "renewing") {
            expire(state, &current).await?;
        }
    }
    Ok(())
}

async fn recover_ambiguous(state: &Arc<AppState>, candidate: &Value) -> ApiResult<()> {
    let lease_id = field(candidate, "lease_id");
    let _lock = lease_lock(state, lease_id).await?;
    let Some(lease) = state
        .store
        .get(LEASES, lease_id)
        .map_err(ApiError::internal)?
    else {
        return Ok(());
    };
    if !matches!(field(&lease, "state"), "provisioning" | "releasing") {
        return Ok(());
    }
    let Some(outbox) = lease_outbox(state, lease_id)? else {
        return Ok(());
    };
    if outbox["status"] != "processing" {
        return Ok(());
    }
    let op = if lease["state"] == "releasing" {
        "stop"
    } else {
        "probe"
    };
    match remote(state, &lease, op).await {
        Ok((status, _)) if status == StatusCode::NOT_FOUND && op == "stop" => {
            settle(
                state,
                &lease,
                &outbox,
                lease.clone(),
                None,
                "platform_not_found",
            )?;
        }
        Ok((status, _)) if status == StatusCode::NOT_FOUND => {
            let _ = terminal_not_found(state, &lease, &outbox, "failed",
                Some("not_found"))?;
        }
        Ok((_, _)) if op == "stop" => {
            settle(state, &lease, &outbox, lease.clone(), None, "confirmed")?;
        }
        Ok((_, result)) => {
            let adopted = settle(
                state,
                &lease,
                &outbox,
                lease.clone(),
                Some(&result),
                "confirmed",
            )?;
            resolve_hard_change(state, &lease, &adopted).await?;
        }
        Err(error) => {
            if op == "probe" && acquire_rejected(&error) {
                let _=terminal_failure(state,&lease,&outbox,"failed",Some("acquire_rejected"),error.code,&error.message)?;
            } else {
                degrade(state, &lease, error.code).await?;
            }
        }
    }
    Ok(())
}

async fn expire(state: &Arc<AppState>, candidate: &Value) -> ApiResult<()> {
    let lease_id = field(candidate, "lease_id");
    let _lock = lease_lock(state, lease_id).await?;
    let Some(lease) = state
        .store
        .get(LEASES, lease_id)
        .map_err(ApiError::internal)?
    else {
        return Ok(());
    };
    if !matches!(field(&lease, "state"), "active" | "renewing")
        || !lease["expires_at"]
            .as_str()
            .and_then(|text| chrono::DateTime::parse_from_rfc3339(text).ok())
            .is_some_and(|at| at <= chrono::Utc::now())
    {
        return Ok(());
    }
    let error = remote(state, &lease, "stop").await.err();
    let mut expired = lease.clone();
    expired["state"] = json!("expired");
    expired["release_receipt"] = json!("expired");
    expired["updated_at"] = json!(now());
    if let Some(error) = &error {
        expired["last_error"] = json!(error.message);
    }
    let open_effect = lease_outbox(state, lease_id)?;
    let changed = if let Some(outbox) = &open_effect {
        let mut cancelled = outbox.clone();
        cancelled["status"] = json!("cancelled");
        cancelled["release_receipt"] = json!("expired");
        cancelled["updated_at"] = json!(now());
        state
            .store
            .put_many_if(
                &[
                    (LEASES, lease_id, Some(&lease)),
                    (OUTBOX, field(outbox, "outbox_id"), Some(outbox)),
                ],
                &[
                    (LEASES, lease_id, &expired),
                    (OUTBOX, field(outbox, "outbox_id"), &cancelled),
                ],
            )
            .map_err(ApiError::internal)?
    } else {
        state
            .store
            .put_many_if(
                &[(LEASES, lease_id, Some(&lease))],
                &[(LEASES, lease_id, &expired)],
            )
            .map_err(ApiError::internal)?
    };
    if changed {
        append_event(
            state,
            field(&lease, "competition_id"),
            "instance_lease",
            lease_id,
            "competition.instance.state_changed",
            "",
            json!({"lease_id":lease_id,"from":lease["state"],"to":"expired",
                "release_receipt":"expired","error":error.as_ref().map(|e|e.code)}),
        )?;
    }
    Ok(())
}
