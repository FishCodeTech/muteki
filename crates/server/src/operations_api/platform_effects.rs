//! Durable receipts for synchronous external effects in the competition domain.
use super::*;

const EFFECTS: &str = "platform_effect_receipts";
const OUTBOX: &str = "platform_outbox_live";
const OUTBOX_INPUTS: &str = "platform_outbox_inputs";

pub(super) fn effectful(kind: &str) -> bool {
    kind.starts_with("ext.") || kind == "extension.command"
}
pub(super) struct Prepared {
    pub effect_id: String,
    outbox_id: String,
    envelope: Value,
    effect: Value,
    outbox: Value,
}
impl Prepared {
    pub fn records(&self) -> Vec<(&str,&str,&Value)> {
        vec![(OUTBOX_INPUTS,&self.outbox_id,&self.envelope),(EFFECTS,&self.effect_id,&self.effect),(OUTBOX,&self.outbox_id,&self.outbox)]
    }
}
pub(super) fn prepare(envelope: &Value) -> Option<Prepared> {
    let kind = envelope["command_type"].as_str().unwrap_or("");
    if !effectful(kind) {
        return None;
    }
    let command_id = envelope["command_id"].as_str().unwrap_or("");
    let effect_id = new_id("effect");
    let outbox_id = new_id("outbox");
    let domain = if kind.starts_with("ext.") || kind == "extension.command" {
        "extension"
    } else {
        "competition"
    };
    let destination = if kind.starts_with("ext.") || kind == "extension.command" {
        "extension.host"
    } else if kind.starts_with("run_binding.") {
        "run.gateway"
    } else {
        "competition.live"
    };
    let aggregate = json!({"type":envelope["aggregate_type"],"id":envelope["aggregate_id"]});
    let effect = json!({"effect_id":effect_id,"command_id":command_id,"acceptance_receipt_id":"","domain":domain,
        "state":"accepted","aggregate":aggregate,"correlation_id":command_id,"outbox_id":outbox_id,
        "destination":destination,"idempotency_key":command_id,"attempts":0,"max_attempts":8,"retry_after":null,
        "object_links":{},"output":{},"error":null,"created_at":now(),"updated_at":now(),"completed_at":null});
    let outbox = json!({"outbox_id":outbox_id,"command_id":command_id,"effect_id":effect_id,"correlation_id":command_id,
        "idempotency_key":command_id,"event_id":null,"aggregate_type":envelope["aggregate_type"],"aggregate_id":envelope["aggregate_id"],
        "event_type":kind,"destination":destination,"status":"pending","attempts":0,"next_attempt_at":null,
        "payload":{"command_type":kind},"created_at":now(),"delivered_at":null,"last_error":null});
    Some(Prepared { effect_id,outbox_id,envelope:envelope.clone(),effect,outbox })
}

pub(super) fn mark_processing(state: &AppState, effect_id: &str) -> Result<(), Failure> {
    let mut effect = state
        .store
        .get(EFFECTS, effect_id)
        .map_err(storage)?
        .ok_or_else(|| {
            fail(
                StatusCode::NOT_FOUND,
                "effect.receipt_not_found",
                "Effect receipt was not recorded",
            )
        })?;
    let outbox_id = effect["outbox_id"].as_str().unwrap_or("").to_owned();
    let mut outbox = state
        .store
        .get(OUTBOX, &outbox_id)
        .map_err(storage)?
        .ok_or_else(|| {
            fail(
                StatusCode::NOT_FOUND,
                "effect.outbox_not_found",
                "Effect delivery record was not recorded",
            )
        })?;
    if !matches!(
        outbox["status"].as_str().unwrap_or(""),
        "pending" | "failed"
    ) {
        return Err(fail(
            StatusCode::CONFLICT,
            "effect.retry_not_allowed",
            "Effect is already being delivered",
        ));
    }
    let attempts = outbox["attempts"].as_u64().unwrap_or(0) + 1;
    outbox["status"] = json!("processing");
    outbox["attempts"] = json!(attempts);
    outbox["updated_at"] = json!(now());
    effect["state"] = json!("running");
    effect["attempts"] = json!(attempts);
    effect["updated_at"] = json!(now());
    state
        .store
        .put_many(&[(OUTBOX, &outbox_id, &outbox), (EFFECTS, effect_id, &effect)])
        .map_err(storage)?;
    Ok(())
}
/// Public command error carried on an effect receipt. Keeps the caller's
/// classification and the contract fields; a missing category follows the HTTP
/// status instead of collapsing to a generic internal error.
pub(super) fn public_effect_error(body: &Value, status: StatusCode, code: &str, retryable: bool) -> Value {
    let nested = if body["error"].is_object() {
        &body["error"]
    } else if body["receipt"]["error"].is_object() {
        &body["receipt"]["error"]
    } else {
        &Value::Null
    };
    let message = nested["message"].as_str().filter(|value| !value.is_empty())
        .unwrap_or("External effect failed");
    let category = nested["category"].as_str().filter(|value| !value.is_empty())
        .unwrap_or(match status {
            StatusCode::BAD_REQUEST | StatusCode::UNPROCESSABLE_ENTITY => "validation",
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => "permission",
            StatusCode::NOT_FOUND => "not_found",
            StatusCode::CONFLICT => "state",
            StatusCode::TOO_MANY_REQUESTS => "rate_limit",
            StatusCode::REQUEST_TIMEOUT | StatusCode::GATEWAY_TIMEOUT => "timeout",
            StatusCode::BAD_GATEWAY => "runtime",
            StatusCode::SERVICE_UNAVAILABLE => "state",
            _ => "state",
        });
    let correlation = nested["correlation_id"].as_str().filter(|value| !value.is_empty());
    json!({
        "schema_version": 1,
        "code": code,
        "message": message,
        "category": category,
        "recovery_hint": nested["recovery_hint"].as_str().unwrap_or(""),
        "correlation_id": correlation,
        "retryable": retryable,
        "detail": nested["detail"].as_object().cloned().unwrap_or_default(),
    })
}
fn known_not_delivered(code: &str) -> bool {
    matches!(
        code,
        "extension.capability_unavailable"
            | "extension.process_unavailable"
            | "extension.not_found"
            | "extension.capability_not_found"
            | "extension.command_denied"
            | "extension.aggregate_mismatch"
            | "extension.invalid_payload"
    )
}
pub(super) async fn finish_response(
    state: &AppState,
    effect_id: &str,
    response: Response,
) -> Result<Response, Failure> {
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .map_err(storage)?;
    let mut body: Value = serde_json::from_slice(&bytes).map_err(storage)?;
    let mut effect = state
        .store
        .get(EFFECTS, effect_id)
        .map_err(storage)?
        .ok_or_else(|| {
            fail(
                StatusCode::INTERNAL_SERVER_ERROR,
                "effect.receipt_not_found",
                "Effect receipt was not recorded",
            )
        })?;
    let outbox_id = effect["outbox_id"].as_str().unwrap_or("").to_owned();
    let mut outbox = state
        .store
        .get(OUTBOX, &outbox_id)
        .map_err(storage)?
        .ok_or_else(|| {
            fail(
                StatusCode::INTERNAL_SERVER_ERROR,
                "effect.outbox_not_found",
                "Effect delivery record was not recorded",
            )
        })?;
    let success = status.is_success() && body["receipt"]["state"] != "failed";
    let code = body["error"]["code"].as_str().or_else(||body["receipt"]["error"]["code"].as_str()).unwrap_or("effect.failed");
    let not_delivered = !success && known_not_delivered(code);
    let attempts = outbox["attempts"].as_u64().unwrap_or(1);
    let exhausted = not_delivered && attempts >= effect["max_attempts"].as_u64().unwrap_or(8);
    let delay_base = (0.5 * 2_f64.powi(attempts.saturating_sub(1).min(12) as i32)).min(300.0);
    let jitter = uuid::Uuid::new_v4().as_u128() as u64 as f64 / u64::MAX as f64;
    let delay_ms = ((delay_base + jitter * delay_base * 0.2) * 1000.0) as i64;
    let retry_after = if not_delivered && !exhausted {
        json!((chrono::Utc::now() + chrono::Duration::milliseconds(delay_ms)).to_rfc3339())
    } else {
        Value::Null
    };
    effect["state"] = json!(if success {
        "completed"
    } else if exhausted {
        "dead_letter"
    } else if not_delivered {
        "waiting"
    } else {
        "running"
    });
    effect["updated_at"] = json!(now());
    effect["completed_at"] = if success { json!(now()) } else { Value::Null };
    effect["retry_after"] = retry_after.clone();
    effect["acceptance_receipt_id"] = body["receipt"]["receipt_id"].clone();
    effect["output"] =
        json!({"http_status":status.as_u16(),"receipt_id":body["receipt"]["receipt_id"]});
    if !success {
        effect["error"] = public_effect_error(&body, status, code, not_delivered && !exhausted);
    }
    outbox["status"] = json!(if success {
        "delivered"
    } else if exhausted {
        "dead_letter"
    } else if not_delivered {
        "failed"
    } else {
        "processing"
    });
    outbox["delivered_at"] = if success { json!(now()) } else { Value::Null };
    outbox["last_error"] = if success { Value::Null } else { json!(code) };
    outbox["next_attempt_at"] = retry_after;
    outbox["updated_at"] = json!(now());
    state
        .store
        .put_many(&[(EFFECTS, effect_id, &effect), (OUTBOX, &outbox_id, &outbox)])
        .map_err(storage)?;
    if body["receipt"].is_object() {
        body["receipt"]["effect_ids"] = json!([effect_id]);
        let command_id = effect["command_id"].as_str().unwrap_or("");
        if let Some(mut stored) = state.store.get(COMMANDS, command_id).map_err(storage)? {
            stored["response"]["receipt"]["effect_ids"] = json!([effect_id]);
            state
                .store
                .put(COMMANDS, command_id, &stored)
                .map_err(storage)?;
        }
    }
    Ok((status, Json(body)).into_response())
}
fn all(state: &AppState, collection: &str) -> Result<Vec<Value>, Failure> {
    state.store.list(collection).map_err(storage)
}
pub(super) fn metrics(state: &AppState) -> Result<Value, Failure> {
    let effects = all(state, EFFECTS)?;
    let outbox = all(state, OUTBOX)?;
    let mut effect_states = serde_json::Map::new();
    let mut outbox_states = serde_json::Map::new();
    let mut durations: std::collections::HashMap<String, (f64, u64)> =
        std::collections::HashMap::new();
    let mut orphan_effects = 0_u64;
    for row in &effects {
        let key = row["state"].as_str().unwrap_or("unknown").to_owned();
        let count = effect_states.get(&key).and_then(Value::as_u64).unwrap_or(0) + 1;
        effect_states.insert(key, json!(count));
        if let (Some(created), Some(completed)) = (
            row["created_at"]
                .as_str()
                .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok()),
            row["completed_at"]
                .as_str()
                .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok()),
        ) {
            let value = durations
                .entry(row["destination"].as_str().unwrap_or("").to_owned())
                .or_default();
            value.0 += (completed - created).num_milliseconds().max(0) as f64 / 1000.0;
            value.1 += 1;
        }
        let found = if let Some(command_id) = row["command_id"].as_str() {
            state
                .store
                .command_receipt(command_id)
                .map_err(storage)?
                .is_some()
        } else {
            false
        };
        if !found {
            orphan_effects += 1;
        }
    }
    for row in &outbox {
        let key = row["status"].as_str().unwrap_or("unknown").to_owned();
        let count = outbox_states.get(&key).and_then(Value::as_u64).unwrap_or(0) + 1;
        outbox_states.insert(key, json!(count));
    }
    let oldest_pending_seconds = outbox
        .iter()
        .filter(|row| {
            matches!(
                row["status"].as_str().unwrap_or(""),
                "pending" | "processing" | "paused"
            )
        })
        .filter_map(|row| {
            row["created_at"]
                .as_str()
                .and_then(|text| chrono::DateTime::parse_from_rfc3339(text).ok())
        })
        .map(|created| {
            chrono::Utc::now()
                .signed_duration_since(created)
                .num_seconds()
                .max(0) as u64
        })
        .max()
        .unwrap_or(0);
    let duration_average = durations
        .into_iter()
        .map(|(destination, (total, count))| {
            (
                destination,
                json!((total / count as f64 * 1000.0).round() / 1000.0),
            )
        })
        .collect::<serde_json::Map<String, Value>>();
    Ok(
        json!({"effects":effect_states,"outbox":outbox_states,"pending_effects":effects.iter().filter(|v|matches!(v["state"].as_str().unwrap_or(""),"accepted"|"running"|"waiting")).count(),
        "pending_outbox":outbox.iter().filter(|v|matches!(v["status"].as_str().unwrap_or(""),"pending"|"processing"|"paused")).count(),
        "oldest_pending_seconds":oldest_pending_seconds,"orphan_effects":orphan_effects,
        "effect_duration_avg_seconds":duration_average}),
    )
}
pub(super) fn receipt_effects(state: &AppState, command_id: &str) -> Result<Json<Value>, Failure> {
    if state
        .store
        .command_receipt(command_id)
        .map_err(storage)?
        .is_none()
    {
        return Err(fail(
            StatusCode::NOT_FOUND,
            "command.receipt_not_found",
            format!("no receipt for command '{command_id}'"),
        ));
    }
    let effects = all(state, EFFECTS)?
        .into_iter()
        .filter(|v| v["command_id"] == command_id)
        .collect::<Vec<_>>();
    Ok(Json(json!({"effects":effects})))
}
pub(super) fn effects(
    state: &AppState,
    filter: Option<&str>,
    limit: usize,
) -> Result<Json<Value>, Failure> {
    const STATES: [&str; 7] = [
        "accepted",
        "running",
        "waiting",
        "completed",
        "failed",
        "cancelled",
        "dead_letter",
    ];
    let allowed: Vec<&str> = filter
        .unwrap_or("")
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
    if let Some(bad) = allowed.iter().find(|state| !STATES.contains(state)) {
        return Err(fail(
            StatusCode::BAD_REQUEST,
            "platform.request.invalid",
            format!("unknown effect state: {bad}"),
        ));
    }
    let mut effects = all(state, EFFECTS)?;
    effects.retain(|effect| {
        allowed.is_empty() || allowed.iter().any(|state| effect["state"] == *state)
    });
    effects.sort_by(|a, b| b["created_at"].as_str().cmp(&a["created_at"].as_str()));
    effects.truncate(limit.clamp(1, 1000));
    Ok(Json(json!({"effects":effects})))
}
pub(super) fn effect(state: &AppState, id: &str) -> Result<Json<Value>, Failure> {
    state
        .store
        .get(EFFECTS, id)
        .map_err(storage)?
        .map(|effect| Json(json!({"effect":effect})))
        .ok_or_else(|| {
            fail(
                StatusCode::NOT_FOUND,
                "effect.receipt_not_found",
                format!("no effect receipt '{id}'"),
            )
        })
}
pub(super) fn cancel(state: &AppState, id: &str, confirm: bool) -> Result<Json<Value>, Failure> {
    if !confirm {
        return Err(fail(
            StatusCode::BAD_REQUEST,
            "effect.confirm_required",
            "confirm=true is required",
        ));
    }
    let mut effect = state
        .store
        .get(EFFECTS, id)
        .map_err(storage)?
        .ok_or_else(|| {
            fail(
                StatusCode::NOT_FOUND,
                "effect.receipt_not_found",
                format!("no effect receipt '{id}'"),
            )
        })?;
    let outbox_id = effect["outbox_id"].as_str().unwrap_or("").to_owned();
    let mut outbox = state
        .store
        .get(OUTBOX, &outbox_id)
        .map_err(storage)?
        .ok_or_else(|| {
            fail(
                StatusCode::NOT_FOUND,
                "effect.outbox_not_found",
                "Effect delivery record not found",
            )
        })?;
    if matches!(
        outbox["status"].as_str().unwrap_or(""),
        "processing" | "delivered"
    ) {
        return Err(fail(
            StatusCode::CONFLICT,
            "effect.irreversible",
            "Remote delivery may have started; inspect the result before cancellation",
        ));
    }
    if outbox["status"] == "cancelled" {
        return Ok(Json(json!({"effect":effect,"outbox_status":"cancelled"})));
    }
    effect["state"] = json!("cancelled");
    effect["updated_at"] = json!(now());
    effect["completed_at"] = json!(now());
    effect["error"] = Value::Null;
    outbox["status"] = json!("cancelled");
    outbox["last_error"] = json!("cancelled by operator before delivery");
    outbox["updated_at"] = json!(now());
    state
        .store
        .put_many(&[(EFFECTS, id, &effect), (OUTBOX, &outbox_id, &outbox)])
        .map_err(storage)?;
    let original_id = effect["command_id"].as_str().unwrap_or("");
    if let Some(mut stored) = state.store.get(COMMANDS, original_id).map_err(storage)? {
        stored["response"]["receipt"]["state"] = json!("cancelled");
        stored["response"]["receipt"]["completed_at"] = json!(now());
        stored["response"]["receipt"]["effect_ids"] = json!([id]);
        state
            .store
            .put(COMMANDS, original_id, &stored)
            .map_err(storage)?;
    }
    Ok(Json(
        json!({"effect":effect,"outbox_status":"cancelled","original_command_id":original_id}),
    ))
}
pub(super) async fn retry(state: &Arc<AppState>, id: &str) -> Result<Json<Value>, Failure> {
    let mut effect = state
        .store
        .get(EFFECTS, id)
        .map_err(storage)?
        .ok_or_else(|| {
            fail(
                StatusCode::NOT_FOUND,
                "effect.receipt_not_found",
                format!("no effect receipt '{id}'"),
            )
        })?;
    let outbox_id = effect["outbox_id"].as_str().unwrap_or("").to_owned();
    let mut outbox = state
        .store
        .get(OUTBOX, &outbox_id)
        .map_err(storage)?
        .ok_or_else(|| {
            fail(
                StatusCode::NOT_FOUND,
                "effect.outbox_not_found",
                "Effect delivery record not found",
            )
        })?;
    let status = outbox["status"].as_str().unwrap_or("");
    if matches!(status, "processing" | "delivered" | "cancelled")
        || (outbox["attempts"].as_u64().unwrap_or(0) > 0
            && !outbox["last_error"]
                .as_str()
                .is_some_and(known_not_delivered))
    {
        return Err(fail(
            StatusCode::CONFLICT,
            "effect.retry_not_allowed",
            "Remote delivery may have started; inspect its result before issuing another command",
        ));
    }
    let mut envelope = state
        .store
        .get(OUTBOX_INPUTS, &outbox_id)
        .map_err(storage)?
        .ok_or_else(|| {
            fail(
                StatusCode::CONFLICT,
                "effect.input_missing",
                "Original effect input is unavailable",
            )
        })?;
    let original_id = effect["command_id"].as_str().unwrap_or("").to_owned();
    let original_envelope = envelope.clone();
    let attempt = outbox["attempts"].as_u64().unwrap_or(0) + 1;
    envelope["command_id"] = json!(format!("{original_id}:retry:{attempt}"));
    outbox["status"] = json!("pending");
    outbox["next_attempt_at"] = Value::Null;
    outbox["updated_at"] = json!(now());
    effect["state"] = json!("waiting");
    effect["error"] = Value::Null;
    effect["retry_after"] = Value::Null;
    effect["updated_at"] = json!(now());
    state
        .store
        .put_many(&[(OUTBOX, &outbox_id, &outbox), (EFFECTS, id, &effect)])
        .map_err(storage)?;
    mark_processing(state, id)?;
    let kind = envelope["command_type"].as_str().unwrap_or("");
    let response = if kind == "extension.command" {
        let response = crate::extensions::dispatch_lifecycle_command(state,&envelope).await;
        normalize_extension_response(state,&original_envelope,response).await?
    } else if competition_api::supports_command(kind) {
        competition_api::dispatch_command_envelope(state.clone(), envelope.clone()).await
    } else if kind.starts_with("ext.") || kind == "extension.command" {
        let response = crate::extensions::dispatch_command(state, &envelope)
            .await
            .ok_or_else(|| {
                fail(
                    StatusCode::NOT_IMPLEMENTED,
                    "capability.unavailable",
                    "Extension command is not registered",
                )
            })?;
        normalize_extension_response(state, &original_envelope, response).await?
    } else {
        return Err(fail(
            StatusCode::CONFLICT,
            "effect.input_invalid",
            "Original command type is unavailable",
        ));
    };
    let http_status = response.status();
    let prior = state.store.get(COMMANDS,&original_id).map_err(storage)?.unwrap_or(Value::Null);
    let principal = prior["principal"].as_str().unwrap_or("operator:local-user");
    let before = prior["version_before"].as_u64().unwrap_or(0);
    let journal_id = format!("{original_id}:retry:{attempt}");
    command_journal::record_attempt(state,&journal_id,&original_envelope,Some(id),principal,before,response).await?;
    command_journal::recover(state,&journal_id).await?;
    let effect = state.store.get(EFFECTS,id).map_err(storage)?.unwrap_or(Value::Null);
    let outbox = state.store.get(OUTBOX,&outbox_id).map_err(storage)?.unwrap_or(Value::Null);
    Ok(Json(
        json!({"effect":effect,"outbox_status":outbox["status"],
        "original_command_id":original_id,"delivery_status":http_status.as_u16()}),
    ))
}

pub(super) async fn drain_once(state: &Arc<AppState>,module:Option<crate::business::Module>) -> Result<usize, Failure> {
    let _dispatch=crate::business::dispatch_lock(state,module).lock().await;
    super::command_journal::recover_all(state,module).await?;
    let mut records = all(state, OUTBOX)?;
    records.sort_by(|a, b| a["created_at"].as_str().cmp(&b["created_at"].as_str()));
    let mut handled = 0;
    for row in records {
        let envelope=json!({"aggregate_id":row["aggregate_id"],"params":{},"payload":{}});
        let owner=crate::business::operation_module(state,row["event_type"].as_str().unwrap_or(""),&envelope).map_err(|e|e.failure())?;
        if owner!=module{continue;}
        if row["destination"] == "run.gateway"
            && row["event_type"]
                .as_str()
                .is_some_and(|kind| kind.starts_with("run."))
        {
            if matches!(
                row["status"].as_str().unwrap_or(""),
                "pending" | "processing"
            ) {
                if let Some(run)=state.store.get("runs",row["aggregate_id"].as_str().unwrap_or("")).map_err(storage)?{
                    if !crate::business::run_available(state,&run){continue;}
                }
                match super::run_commands::recover(state, &row).await {
                    Ok(true) => handled += 1,
                    Ok(false) => {}
                    Err(error) => tracing::warn!(outbox_id=%row["outbox_id"],
                        code=%error.1.0["error"]["code"], "Run effect recovery failed"),
                }
            }
            if handled >= 100 {
                break;
            }
            continue;
        }
        let attempts = row["attempts"].as_u64().unwrap_or(0);
        let status = row["status"].as_str().unwrap_or("");
        let due = row["next_attempt_at"]
            .as_str()
            .and_then(|at| chrono::DateTime::parse_from_rfc3339(at).ok())
            .is_some_and(|at| at <= chrono::Utc::now());
        if !(status == "pending"
            && (attempts == 0 || row["last_error"].as_str().is_some_and(known_not_delivered))
            || status == "failed"
                && due
                && row["last_error"].as_str().is_some_and(known_not_delivered))
        {
            continue;
        }
        let effect_id = row["effect_id"].as_str().unwrap_or("");
        let Some(effect) = state.store.get(EFFECTS, effect_id).map_err(storage)? else {
            continue;
        };
        if attempts >= effect["max_attempts"].as_u64().unwrap_or(8) {
            continue;
        }
        if let Err(error) = retry(state, effect_id).await {
            tracing::warn!(effect_id, code=%error.1.0["error"]["code"], "Outbox delivery was not claimed");
        }
        handled += 1;
        if handled >= 100 {
            break;
        }
    }
    Ok(handled)
}

#[cfg(test)]
mod tests {
    use super::public_effect_error;
    use axum::http::StatusCode;
    use serde_json::json;

    const CONTRACT: [&str; 8] = [
        "schema_version", "code", "message", "category", "recovery_hint",
        "correlation_id", "retryable", "detail",
    ];

    #[test]
    fn effect_receipt_keeps_permission_timeout_and_runtime_classes() {
        let permission = public_effect_error(&json!({"error":{
            "code":"capability.binding.required","message":"Exact binding_id is required",
            "category":"permission","recovery_hint":"Present the current binding.",
            "correlation_id":"cmd-1","retryable":false,"detail":{"binding":"missing"}
        }}), StatusCode::FORBIDDEN, "capability.binding.required", false);
        let timeout = public_effect_error(&json!({"error":{"code":"runtime.timed_out","message":"Engine timed out"}}),
            StatusCode::GATEWAY_TIMEOUT, "runtime.timed_out", true);
        let runtime = public_effect_error(&json!({"error":{"code":"extension.process_unavailable","message":"Extension process is not running","category":"runtime"}}),
            StatusCode::BAD_GATEWAY, "extension.process_unavailable", false);
        for error in [&permission, &timeout, &runtime] {
            for field in CONTRACT {
                assert!(error.get(field).is_some(), "{field} missing in {error}");
            }
        }
        assert_eq!(permission["category"], "permission");
        assert_eq!(permission["message"], "Exact binding_id is required");
        assert_eq!(permission["detail"]["binding"], "missing");
        assert_eq!(timeout["category"], "timeout");
        assert_eq!(timeout["retryable"], true);
        assert_ne!(timeout["category"], "internal");
        assert_eq!(runtime["category"], "runtime");
        assert_ne!(runtime["message"], "External effect failed");
    }
}
