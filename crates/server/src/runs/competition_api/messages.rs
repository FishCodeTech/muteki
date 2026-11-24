//! Competition message commands share one event and receipt transaction.
use super::*;
use sha2::{Digest, Sha256};

const VERSIONS: &str = "platform_aggregate_versions";

pub(super) fn semantic_fingerprint_for(
    competition_id: &str,
    author: &str,
    text: &str,
    linked_command_id: &str,
    attached_command: &Value,
) -> String {
    let canonical = json!({
        "command_type": "competition.message",
        "competition_id": competition_id,
        "author": author,
        "text": text,
        "linked_command_id": if attached_command.is_null() { linked_command_id } else { "" },
        "attached_command": attached_command,
    });
    format!(
        "{:x}",
        Sha256::digest(crate::runs::src_api::source_compact_json(&canonical).as_bytes())
    )
}

fn canonical_request(
    competition_id: &str,
    author: &str,
    text: &str,
    linked_command_id: &str,
) -> Value {
    let mut payload = json!({
        "competition_id": competition_id,
        "author": author,
        "text": text,
    });
    if !linked_command_id.is_empty() {
        payload["command_id"] = json!(linked_command_id);
    }
    json!({
        "command_type": "competition.message",
        "aggregate_type": "competition",
        "aggregate_id": competition_id,
        "payload": payload,
    })
}

fn normalized_request_without_receipt_state(request: &Value) -> Value {
    let mut copy = request.clone();
    if let Some(payload) = copy.get_mut("payload").and_then(Value::as_object_mut) {
        payload.remove("receipt_state");
    }
    copy
}

pub(super) fn same_message_semantics(
    prior: &Value,
    request: &Value,
    semantic_fingerprint: &str,
) -> bool {
    let prior_fp = field(&prior["plan"], "message_semantic_fingerprint");
    if !prior_fp.is_empty() && !semantic_fingerprint.is_empty() {
        return prior_fp == semantic_fingerprint;
    }
    normalized_request_without_receipt_state(&prior["request"])
        == normalized_request_without_receipt_state(request)
}

pub(super) fn ensure_receipt_cursor(
    state: &AppState,
    command_id: &str,
    stored: &Value,
) -> ApiResult<Value> {
    let mut receipt = stored["response"]["receipt"].clone();
    if receipt["state"] != "completed" || receipt["event_cursor"].is_string() {
        return Ok(receipt);
    }
    let competition_id = field(&stored["request"], "aggregate_id")
        .to_owned()
        .if_empty(field(&stored["plan"], "competition_id"));
    let scope = field(&stored["plan"], "cursor_scope")
        .to_owned()
        .if_empty("operator:local-user");
    let seq = stored["plan"]["event_coordinate"]["global_seq"]
        .as_u64()
        .or_else(|| stored["plan"]["event_seq"].as_u64())
        .or_else(|| {
            state
                .store
                .list(EVENTS)
                .ok()?
                .into_iter()
                .filter(|event| {
                    event["command_id"] == command_id
                        && event["event_type"] == "competition.message.posted"
                })
                .filter_map(|event| event["global_seq"].as_u64())
                .max()
        });
    if let Some(seq) = seq {
        let cursor = crate::operations_api::competition_event_cursor(
            state,
            "competition",
            &competition_id,
            seq,
            &scope,
        )
        .map_err(ApiError::internal)?;
        receipt["event_cursor"] = json!(cursor);
        let mut updated = stored.clone();
        updated["response"]["receipt"] = receipt.clone();
        let _ = state.store.put_many_if(
            &[(COMMANDS, command_id, Some(stored))],
            &[(COMMANDS, command_id, &updated)],
        );
    }
    Ok(receipt)
}

pub(super) fn find_prior_by_idempotency_key(
    state: &AppState,
    idempotency_key: &str,
    admission: &Value,
) -> ApiResult<Option<Value>> {
    let key_id = field(admission, "key_id");
    if !key_id.is_empty()
        && let Some(keyed) = state
            .store
            .get("platform_idempotency_keys", key_id)
            .map_err(ApiError::internal)?
    {
        let prior_id = field(&keyed, "command_id");
        if !prior_id.is_empty() {
            return state
                .store
                .get(COMMANDS, prior_id)
                .map_err(ApiError::internal);
        }
    }
    if idempotency_key.is_empty() {
        return Ok(None);
    }
    for row in state
        .store
        .list("platform_idempotency_keys")
        .map_err(ApiError::internal)?
    {
        if field(&row, "idempotency_key") == idempotency_key {
            let prior_id = field(&row, "command_id");
            if !prior_id.is_empty() {
                return state
                    .store
                    .get(COMMANDS, prior_id)
                    .map_err(ApiError::internal);
            }
        }
    }
    Ok(None)
}

#[allow(clippy::too_many_arguments)]
fn persist_terminal_failure(
    state: &AppState,
    competition_id: &str,
    command_id: &str,
    idempotency_key: &str,
    cursor_scope: &str,
    admission: &Value,
    request: &Value,
    plan: &Value,
    semantic_fingerprint: &str,
    receipt_state: &str,
    error_code: &str,
    error_message: &str,
    error_category: &str,
) -> ApiResult<Value> {
    let timestamp = now();
    let error = json!({
        "schema_version": 1,
        "code": error_code,
        "message": error_message,
        "category": error_category,
        "recovery_hint": "",
        "correlation_id": command_id,
        "retryable": false,
        "detail": {},
    });
    let receipt = json!({
        "schema_version": 1,
        "command_id": command_id,
        "receipt_id": id("rcpt"),
        "state": receipt_state,
        "run_id": null,
        "aggregate": {"type": "competition", "id": competition_id},
        "event_cursor": null,
        "deduplicated": false,
        "submitted_at": timestamp,
        "completed_at": timestamp,
        "next": [],
        "output": {},
        "effect_ids": [],
        "error": error,
    });
    let status = crate::operations_api::planned_receipt_status(&receipt).as_u16();
    let req_fp = field(&admission["claim"], "request_fingerprint")
        .to_owned()
        .if_empty(semantic_fingerprint);
    let mut stored_plan = plan.clone();
    stored_plan["competition_id"] = json!(competition_id);
    stored_plan["cursor_scope"] = json!(cursor_scope);
    stored_plan["idempotency_key"] = json!(idempotency_key);
    stored_plan["message_semantic_fingerprint"] = json!(semantic_fingerprint);
    let stored = json!({
        "request": request,
        "request_fingerprint": req_fp,
        "plan": stored_plan,
        "status": status,
        "response": {"receipt": receipt},
    });
    let existing_claim = state
        .store
        .get("platform_command_claims", command_id)
        .map_err(ApiError::internal)?;
    let mut claim_record = existing_claim
        .clone()
        .or_else(|| admission.get("claim").filter(|v| v.is_object()).cloned())
        .unwrap_or_else(|| {
            json!({
                "command_id": command_id,
                "request_fingerprint": req_fp,
                "created_at": timestamp,
            })
        });
    claim_record["state"] = json!("failed");
    claim_record["updated_at"] = json!(timestamp);
    let key_id = field(admission, "key_id").to_owned();
    let mut keyed_record = admission
        .get("keyed")
        .filter(|v| v.is_object())
        .cloned()
        .unwrap_or_else(|| {
            json!({
                "command_id": command_id,
                "idempotency_key": idempotency_key,
                "semantic_fingerprint": semantic_fingerprint,
                "created_at": timestamp,
            })
        });
    keyed_record["command_id"] = json!(command_id);
    if keyed_record.get("idempotency_key").is_none() && !idempotency_key.is_empty() {
        keyed_record["idempotency_key"] = json!(idempotency_key);
    }
    if keyed_record.get("semantic_fingerprint").is_none() {
        keyed_record["semantic_fingerprint"] = json!(semantic_fingerprint);
    }
    let mut expected = vec![
        (COMMANDS, command_id, None),
        ("platform_command_claims", command_id, existing_claim.as_ref()),
    ];
    let mut records = vec![
        (COMMANDS, command_id, &stored),
        ("platform_command_claims", command_id, &claim_record),
    ];
    if !key_id.is_empty() {
        expected.push(("platform_idempotency_keys", key_id.as_str(), None));
        records.push(("platform_idempotency_keys", key_id.as_str(), &keyed_record));
    }
    let committed = state
        .store
        .put_many_if(&expected, &records)
        .map_err(ApiError::internal)?;
    if !committed {
        let prior = if let Some(prior) = state.store.get(COMMANDS, command_id)
            .map_err(ApiError::internal)? {
            Some(prior)
        } else {
            find_prior_by_idempotency_key(state, idempotency_key, admission)?
        };
        let Some(prior) = prior else {
            return Err(ApiError::new(StatusCode::CONFLICT,
                "competition.message.concurrent_update",
                "Message changed concurrently without a stored receipt"));
        };
        if !same_message_semantics(&prior, request, semantic_fingerprint) {
            return Err(ApiError::new(StatusCode::CONFLICT,
                "command.idempotency_conflict",
                "idempotency key was used with different message content"));
        }
        let mut receipt = prior["response"]["receipt"].clone();
        receipt["deduplicated"] = json!(true);
        return Ok(receipt);
    }
    Ok(receipt)
}

#[allow(clippy::too_many_arguments)]
pub(super) fn execute(
    state: &AppState,
    competition_id: &str,
    requested_command_id: &str,
    idempotency_key: &str,
    expected_version: Option<u64>,
    cursor_scope: &str,
    admission: &Value,
    payload: &Value,
) -> ApiResult<Value> {
    let text = field(payload, "text").trim();
    let linked_command_id = field(payload, "command_id");
    let author = field(payload, "author").to_owned().if_empty("operator");
    let plan_meta = payload
        .get("_message_plan")
        .filter(|v| v.is_object())
        .cloned()
        .unwrap_or_else(|| json!({}));
    let attached_spec = plan_meta
        .get("attached_command")
        .cloned()
        .unwrap_or(Value::Null);
    let effective_idempotency_key = if !idempotency_key.is_empty() {
        idempotency_key
    } else {
        field(&plan_meta, "idempotency_key")
    };
    let semantic_fingerprint = {
        let from_plan = field(&plan_meta, "message_semantic_fingerprint");
        if !from_plan.is_empty() {
            from_plan.to_owned()
        } else {
            semantic_fingerprint_for(
                competition_id,
                &author,
                text,
                linked_command_id,
                &attached_spec,
            )
        }
    };
    let request = canonical_request(competition_id, &author, text, linked_command_id);
    let command_id = if requested_command_id.is_empty() {
        id("cmd")
    } else {
        requested_command_id.to_owned()
    };
    let mut event_payload = json!({
        "competition_id": competition_id,
        "author": author,
        "text": text,
    });
    if !linked_command_id.is_empty() {
        event_payload["command_id"] = json!(linked_command_id);
    }
    if !field(payload, "receipt_state").is_empty() {
        event_payload["receipt_state"] = payload["receipt_state"].clone();
    }
    loop {
        if let Some(prior) = state
            .store
            .get(COMMANDS, &command_id)
            .map_err(ApiError::internal)?
        {
            if !same_message_semantics(&prior, &request, &semantic_fingerprint) {
                return Err(ApiError::new(
                    StatusCode::CONFLICT,
                    "command.idempotency_conflict",
                    "command_id was used with different message content",
                ));
            }
            let mut receipt = ensure_receipt_cursor(state, &command_id, &prior)?;
            receipt["deduplicated"] = json!(true);
            return Ok(receipt);
        }
        if let Some(prior) =
            find_prior_by_idempotency_key(state, effective_idempotency_key, admission)?
        {
            if !same_message_semantics(&prior, &request, &semantic_fingerprint) {
                return Err(ApiError::new(
                    StatusCode::CONFLICT,
                    "command.idempotency_conflict",
                    "idempotency_key was used with different message content",
                ));
            }
            let prior_id = field(&prior["response"]["receipt"], "command_id");
            let mut receipt = ensure_receipt_cursor(state, prior_id, &prior)?;
            receipt["deduplicated"] = json!(true);
            return Ok(receipt);
        }
        if admission.is_object()
            && state
                .store
                .get("platform_command_claims", &command_id)
                .map_err(ApiError::internal)?
                .is_some()
        {
            return Err(ApiError::new(
                StatusCode::CONFLICT,
                "command.outcome_unknown",
                "Prior command outcome must be inspected before retry",
            ));
        }

        let competition = match live_competition(state, competition_id) {
            Ok(comp) if comp["archived"] != true => comp,
            Ok(_) => {
                return persist_terminal_failure(
                    state,
                    competition_id,
                    &command_id,
                    effective_idempotency_key,
                    cursor_scope,
                    admission,
                    &request,
                    &plan_meta,
                    &semantic_fingerprint,
                    "failed",
                    "competition.competition.archived",
                    &format!("competition {competition_id:?} was removed from the inventory"),
                    "not_found",
                );
            }
            Err(_) => {
                return persist_terminal_failure(
                    state,
                    competition_id,
                    &command_id,
                    effective_idempotency_key,
                    cursor_scope,
                    admission,
                    &request,
                    &plan_meta,
                    &semantic_fingerprint,
                    "failed",
                    "competition.competition.not_found",
                    &format!("unknown competition {competition_id:?}"),
                    "not_found",
                );
            }
        };

        if text.is_empty() && linked_command_id.is_empty() {
            return persist_terminal_failure(
                state,
                competition_id,
                &command_id,
                effective_idempotency_key,
                cursor_scope,
                admission,
                &request,
                &plan_meta,
                &semantic_fingerprint,
                "failed",
                "competition.message.text_required",
                "competition.message requires payload.text or an attached command receipt reference",
                "validation",
            );
        }

        let version_key = format!("competition\0{competition_id}");
        let version = state
            .store
            .get(VERSIONS, &version_key)
            .map_err(ApiError::internal)?;
        let current_version = version
            .as_ref()
            .and_then(|row| row["version"].as_u64())
            .unwrap_or(0);
        let expected_ver = expected_version.or_else(|| admission["expected_version"].as_u64());
        if let Some(expected) = expected_ver
            && expected != current_version
        {
            return Err(ApiError::new(
                StatusCode::CONFLICT,
                "command.version_conflict",
                format!("Expected version {expected}, current version {current_version}"),
            ));
        }

        let next_global_seq = state
            .store
            .list(EVENTS)
            .map_err(ApiError::internal)?
            .iter()
            .filter_map(|event| event["global_seq"].as_u64())
            .max()
            .unwrap_or(0)
            + 1;
        let stream_seq = current_version + 1;
        let cursor = crate::operations_api::competition_event_cursor(
            state,
            "competition",
            competition_id,
            next_global_seq,
            cursor_scope,
        )
        .map_err(ApiError::internal)?;

        let timestamp = now();
        let receipt = json!({
            "schema_version": 1,
            "command_id": command_id,
            "receipt_id": id("rcpt"),
            "state": "completed",
            "run_id": null,
            "aggregate": {"type": "competition", "id": competition_id},
            "event_cursor": cursor,
            "deduplicated": false,
            "submitted_at": timestamp,
            "completed_at": timestamp,
            "next": [],
            "output": {},
            "effect_ids": [],
            "error": null,
        });
        let req_fp = field(&admission["claim"], "request_fingerprint")
            .to_owned()
            .if_empty(&semantic_fingerprint);
        let mut plan = plan_meta.clone();
        plan["competition_id"] = json!(competition_id);
        plan["cursor_scope"] = json!(cursor_scope);
        plan["idempotency_key"] = json!(effective_idempotency_key);
        plan["message_semantic_fingerprint"] = json!(semantic_fingerprint);
        plan["linked_command_id"] = json!(linked_command_id);
        plan["event_seq"] = json!(next_global_seq);
        plan["event_coordinate"] = json!({
            "aggregate_type": "competition",
            "aggregate_id": competition_id,
            "competition_id": competition_id,
            "global_seq": next_global_seq,
            "stream_seq": stream_seq,
            "event_cursor": cursor,
        });
        let stored = json!({
            "request": request,
            "request_fingerprint": req_fp,
            "plan": plan,
            "status": StatusCode::OK.as_u16(),
            "response": {"receipt": receipt},
        });
        let existing_claim = state
            .store
            .get("platform_command_claims", &command_id)
            .map_err(ApiError::internal)?;
        let mut claim_record = existing_claim
            .clone()
            .or_else(|| admission.get("claim").filter(|v| v.is_object()).cloned())
            .unwrap_or_else(|| {
                json!({
                    "command_id": command_id,
                    "request_fingerprint": req_fp,
                    "created_at": timestamp,
                })
            });
        claim_record["state"] = json!("completed");
        claim_record["updated_at"] = json!(timestamp);

        let key_id = field(admission, "key_id").to_owned();
        let mut keyed_record = admission
            .get("keyed")
            .filter(|v| v.is_object())
            .cloned()
            .unwrap_or_else(|| {
                json!({
                    "command_id": command_id,
                    "idempotency_key": effective_idempotency_key,
                    "semantic_fingerprint": semantic_fingerprint,
                    "created_at": timestamp,
                })
            });
        keyed_record["command_id"] = json!(command_id);
        if keyed_record.get("idempotency_key").is_none() && !effective_idempotency_key.is_empty() {
            keyed_record["idempotency_key"] = json!(effective_idempotency_key);
        }
        if keyed_record.get("semantic_fingerprint").is_none() {
            keyed_record["semantic_fingerprint"] = json!(semantic_fingerprint);
        }

        let mut expected = vec![
            (COMPETITIONS, competition_id, Some(&competition)),
            (VERSIONS, version_key.as_str(), version.as_ref()),
            (COMMANDS, command_id.as_str(), None),
            (
                "platform_command_claims",
                command_id.as_str(),
                existing_claim.as_ref(),
            ),
        ];
        let mut records = vec![
            (COMMANDS, command_id.as_str(), &stored),
            (
                "platform_command_claims",
                command_id.as_str(),
                &claim_record,
            ),
        ];
        if !key_id.is_empty() {
            expected.push(("platform_idempotency_keys", key_id.as_str(), None));
            records.push(("platform_idempotency_keys", key_id.as_str(), &keyed_record));
        }
        let event = leases::planned_event(
            competition_id,
            "competition",
            competition_id,
            "competition.message.posted",
            &command_id,
            event_payload.clone(),
        );
        if let Some(committed) = put_records_with_events_returning(
            state,
            competition_id,
            &expected,
            &records,
            &[event],
        )? {
            let actual_seq = committed
                .last()
                .and_then(|ev| ev["global_seq"].as_u64())
                .unwrap_or(next_global_seq);
            if actual_seq != next_global_seq {
                let actual_cursor = crate::operations_api::competition_event_cursor(
                    state,
                    "competition",
                    competition_id,
                    actual_seq,
                    cursor_scope,
                )
                .map_err(ApiError::internal)?;
                let mut fixed_receipt = receipt;
                fixed_receipt["event_cursor"] = json!(actual_cursor);
                let mut fixed_stored = stored.clone();
                fixed_stored["plan"]["event_seq"] = json!(actual_seq);
                fixed_stored["plan"]["event_coordinate"]["global_seq"] = json!(actual_seq);
                fixed_stored["plan"]["event_coordinate"]["event_cursor"] = json!(actual_cursor);
                fixed_stored["response"]["receipt"] = fixed_receipt.clone();
                let _ = state.store.put_many_if(
                    &[(COMMANDS, command_id.as_str(), Some(&stored))],
                    &[(COMMANDS, command_id.as_str(), &fixed_stored)],
                );
                return Ok(fixed_receipt);
            }
            return Ok(receipt);
        }
    }
}
