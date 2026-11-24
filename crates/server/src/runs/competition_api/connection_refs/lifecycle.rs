//! Connection CommandAPI's accepted plan, local effect, and result journal.
use super::*;

const RESULTS: &str = "competition_connection_effect_results";

pub(super) fn result_record(command_id: &str, status: &str, output: Value, error: Value) -> Value {
    json!({"command_id":command_id,"state":status,"output":output,
        "error":error,"created_at":now()})
}

pub(super) fn receipt(command_id: &str, connection_id: &str, state: &str, output: Value) -> Value {
    let timestamp = now();
    json!({"schema_version":1,"command_id":command_id,"receipt_id":id("rcpt"),
        "state":state,"run_id":null,"aggregate":{"type":"connection","id":connection_id},
        "event_cursor":null,"deduplicated":false,"submitted_at":timestamp,
        "completed_at":if state == "accepted" { Value::Null } else { json!(timestamp) },
        "next":if state == "accepted" { json!(["receipt"]) } else { json!([]) },
        "output":output,"effect_ids":[],"error":null})
}

pub(super) fn stage(
    state: &AppState,
    previous: &Value,
    connection_id: &str,
    command_id: &str,
    kind: &str,
    request: Value,
    event_type: &str,
    event_payload: Value,
    admission: &Value,
    expected_version: Option<u64>,
) -> ApiResult<bool> {
    let accepted = receipt(command_id, connection_id, "accepted", json!({}));
    let mut stored = json!({"request":{"command_type":kind,"aggregate_id":connection_id,
        "payload":request},"plan":{"connection_before":previous},
        "response":{"receipt":accepted}});
    let claim = admission.get("claim").filter(|value| value.is_object());
    if let Some(claim) = claim {
        stored["request_fingerprint"] = claim["request_fingerprint"].clone();
        stored["status"] = json!(StatusCode::ACCEPTED.as_u16());
    }
    let events = if event_type.is_empty() {
        Vec::new()
    } else {
        vec![leases::planned_event(
            "",
            "connection",
            connection_id,
            event_type,
            command_id,
            event_payload,
        )]
    };
    let version_key = format!("connection\0{connection_id}");
    let version = state
        .store
        .get("platform_aggregate_versions", &version_key)
        .map_err(ApiError::internal)?;
    if expected_version.is_some_and(|expected| {
        version
            .as_ref()
            .and_then(|row| row["version"].as_u64())
            .unwrap_or(0)
            != expected
    }) {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "command.version_conflict",
            "expected_version does not match connection stream",
        ));
    }
    let mut expected = vec![
        (CONNECTIONS, connection_id, Some(previous)),
        (COMMANDS, command_id, None),
        (
            "platform_aggregate_versions",
            version_key.as_str(),
            version.as_ref(),
        ),
    ];
    let mut records = vec![(COMMANDS, command_id, &stored)];
    if let Some(claim) = claim {
        expected.push(("platform_command_claims", command_id, None));
        records.push(("platform_command_claims", command_id, claim));
        let key_id = field(admission, "key_id");
        if !key_id.is_empty() {
            expected.push(("platform_idempotency_keys", key_id, None));
            records.push(("platform_idempotency_keys", key_id, &admission["keyed"]));
        }
    }
    put_records_with_events(state, "", &expected, &records, &events)
}

pub(super) fn finish(state: &AppState, command_id: &str) -> ApiResult<Option<Value>> {
    let Some(result) = state
        .store
        .get(RESULTS, command_id)
        .map_err(ApiError::internal)?
    else {
        return Ok(None);
    };
    loop {
        let Some(previous) = state
            .store
            .get(COMMANDS, command_id)
            .map_err(ApiError::internal)?
        else {
            return Ok(None);
        };
        if previous["response"]["receipt"]["state"] != "accepted" {
            return Ok(Some(previous["response"]["receipt"].clone()));
        }
        let mut updated = previous.clone();
        let final_receipt = &mut updated["response"]["receipt"];
        final_receipt["state"] = result["state"].clone();
        final_receipt["output"] = result["output"].clone();
        final_receipt["error"] = result["error"].clone();
        final_receipt["completed_at"] = json!(now());
        final_receipt["next"] = json!([]);
        let status = if result["state"] == "completed" {
            200
        } else if result["state"] == "conflict" {
            409
        } else {
            match field(&result["error"], "category") {
                "not_found" => 404,
                "validation" => 400,
                "permission" => 403,
                "state" => 409,
                _ => 502,
            }
        };
        updated["status"] = json!(status);
        let claim = state
            .store
            .get("platform_command_claims", command_id)
            .map_err(ApiError::internal)?;
        let mut updated_claim = claim.clone();
        if let Some(value) = updated_claim.as_mut() {
            value["state"] = json!(if result["state"] == "completed" {
                "completed"
            } else {
                "failed"
            });
            value["updated_at"] = json!(now());
        }
        let mut expected = vec![(COMMANDS, command_id, Some(&previous))];
        let mut records = vec![(COMMANDS, command_id, &updated)];
        if let (Some(before), Some(after)) = (claim.as_ref(), updated_claim.as_ref()) {
            expected.push(("platform_command_claims", command_id, Some(before)));
            records.push(("platform_command_claims", command_id, after));
        }
        if state
            .store
            .put_many_if(&expected, &records)
            .map_err(ApiError::internal)?
        {
            return Ok(Some(updated["response"]["receipt"].clone()));
        }
    }
}

pub(super) fn pending(state: &AppState, command_id: &str) -> ApiResult<Value> {
    let mut receipt = state
        .store
        .get(COMMANDS, command_id)
        .map_err(ApiError::internal)?
        .ok_or_else(|| ApiError::missing("command receipt"))?["response"]["receipt"]
        .clone();
    receipt["error"] = json!({"code":"command.result_pending",
        "message":"Local effect result needs receipt recovery","category":"state","retryable":true});
    Ok(json!({"receipt":receipt}))
}

pub(super) fn journal_and_finish(
    state: &AppState,
    command_id: &str,
    status: &str,
    output: Value,
    error: Value,
) -> ApiResult<Value> {
    let result = result_record(command_id, status, output, error);
    if let Err(failure) = state.store.put(RESULTS, command_id, &result) {
        tracing::warn!(%failure,command_id,"Connection effect result could not be journaled");
        return pending(state, command_id);
    }
    match finish(state, command_id) {
        Ok(Some(receipt)) => Ok(json!({"receipt":receipt})),
        Ok(None) => pending(state, command_id),
        Err(failure) => {
            tracing::warn!(code=%failure.code,command_id,"Connection receipt needs result recovery");
            pending(state, command_id)
        }
    }
}

pub(super) fn finish_recorded(state: &AppState, command_id: &str) -> ApiResult<Value> {
    match finish(state, command_id) {
        Ok(Some(receipt)) => Ok(json!({"receipt":receipt})),
        Ok(None) => pending(state, command_id),
        Err(failure) => {
            tracing::warn!(code=%failure.code,command_id,"Connection receipt needs result recovery");
            pending(state, command_id)
        }
    }
}

pub(super) fn previous_response(state: &AppState, command_id: &str) -> ApiResult<Option<Value>> {
    if state
        .store
        .get(RESULTS, command_id)
        .map_err(ApiError::internal)?
        .is_some()
    {
        let _ = finish(state, command_id);
    }
    prior_receipt(state, command_id)
        .map(|receipt| receipt.map(|receipt| json!({"receipt":receipt})))
}

pub(crate) fn reconcile(state: &AppState) -> ApiResult<()> {
    for result in state.store.list(RESULTS).map_err(ApiError::internal)? {
        let command_id = field(&result, "command_id");
        if let Err(failure) = finish(state, command_id) {
            tracing::warn!(command_id,code=%failure.code,"Connection result recovery failed");
        }
    }
    Ok(())
}

pub(super) fn cleanup_secrets(state: &AppState, connection_id: &str, old_reference: &str) {
    let metadata = match state.store.list(SECRETS) {
        Ok(rows) => rows,
        Err(error) => {
            tracing::warn!(%error,"Connection SecretStore cleanup could not list references");
            Vec::new()
        }
    };
    let mut references = metadata
        .into_iter()
        .filter(|row| row["connection_id"] == connection_id)
        .filter_map(|row| row["reference"].as_str().map(str::to_owned))
        .collect::<Vec<_>>();
    if !old_reference.is_empty()
        && !references
            .iter()
            .any(|reference| reference == old_reference)
    {
        references.push(old_reference.to_owned());
    }
    if let Ok(Some(browser)) = state.store.get(BROWSER, connection_id) {
        let reference = field(&browser, "reference");
        if !reference.is_empty() && !references.iter().any(|value| value == reference) {
            references.push(reference.to_owned());
        }
    }
    for reference in references {
        let _ = remove_secret(state, &reference);
    }
    if validated_segment(connection_id).is_ok() {
        let directory = native_chat::state_dir(state)
            .join("control/competition/secrets")
            .join(connection_id);
        if let Ok(entries) = fs::read_dir(&directory) {
            for entry in entries.flatten() {
                if fs::symlink_metadata(entry.path()).is_ok_and(|meta| meta.file_type().is_file()) {
                    let _ = fs::remove_file(entry.path());
                }
            }
            let _ = fs::remove_dir(directory);
        }
    }
    let _ = state.store.delete(BROWSER, connection_id);
}
