//! Source Competition CommandAPI unregister plan and local archive result.
use super::*;

const RESULTS: &str = "competition_archive_effect_results";

fn accepted(command_id: &str, competition_id: &str) -> Value {
    json!({"schema_version":1,"command_id":command_id,"receipt_id":id("rcpt"),
        "state":"accepted","run_id":null,
        "aggregate":{"type":"competition","id":competition_id},
        "event_cursor":null,"deduplicated":false,"submitted_at":now(),
        "completed_at":null,"next":["receipt","snapshot","events","wait"],
        "output":{},"effect_ids":[],"error":null})
}

fn stage(
    state: &AppState,
    competition_id: &str,
    command_id: &str,
    previous: &Value,
    body: &Value,
) -> ApiResult<bool> {
    let receipt = accepted(command_id, competition_id);
    let mut stored = json!({"request":{"command_type":"competition.unregister",
        "aggregate_id":competition_id,"payload":{"competition_id":competition_id}},
        "plan":{"competition_before":previous,
            "cursor_scope":field(body,"_cursor_scope")},
        "response":{"receipt":receipt}});
    let admission = &body["_planned_admission"];
    let claim = admission.get("claim").filter(|value| value.is_object());
    if let Some(claim) = claim {
        stored["request_fingerprint"] = claim["request_fingerprint"].clone();
        stored["status"] = json!(StatusCode::ACCEPTED.as_u16());
    }
    let version_key = format!("competition\0{competition_id}");
    let version = state
        .store
        .get("platform_aggregate_versions", &version_key)
        .map_err(ApiError::internal)?;
    if body["expected_version"].as_u64().is_some_and(|expected| {
        version
            .as_ref()
            .and_then(|row| row["version"].as_u64())
            .unwrap_or(0)
            != expected
    }) {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "command.version_conflict",
            "expected_version does not match competition stream",
        ));
    }
    let mut expected = vec![
        (COMPETITIONS, competition_id, Some(previous)),
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
    let events = if previous["archived"] == true {
        Vec::new()
    } else {
        vec![leases::planned_event(
            competition_id,
            "competition",
            competition_id,
            "competition.unregistered",
            command_id,
            json!({"competition_id":competition_id,
                "previous_scheduler_state":previous["scheduler_state"]}),
        )]
    };
    put_records_with_events(state, competition_id, &expected, &records, &events)
}

pub(crate) fn finish(state: &AppState, command_id: &str) -> ApiResult<Option<Value>> {
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
        let receipt = &mut updated["response"]["receipt"];
        receipt["state"] = result["state"].clone();
        receipt["output"] = result["output"].clone();
        receipt["error"] = result["error"].clone();
        receipt["completed_at"] = json!(now());
        receipt["next"] = json!([]);
        let competition_id = field(&previous["request"], "aggregate_id");
        if let Some(seq) = state
            .store
            .list(EVENTS)
            .map_err(ApiError::internal)?
            .iter()
            .filter(|event| {
                event["command_id"] == command_id
                    && event["event_type"] == "competition.unregistered"
            })
            .filter_map(|event| event["global_seq"].as_u64())
            .max()
        {
            receipt["event_cursor"] = json!(
                crate::operations_api::competition_event_cursor(
                    state,
                    "competition",
                    competition_id,
                    seq,
                    field(&previous["plan"], "cursor_scope")
                )
                .map_err(ApiError::internal)?
            );
        }
        updated["status"] = json!(
            crate::operations_api::planned_receipt_status(&updated["response"]["receipt"]).as_u16()
        );
        let claim = state
            .store
            .get("platform_command_claims", command_id)
            .map_err(ApiError::internal)?;
        let mut next_claim = claim.clone();
        if let Some(value) = next_claim.as_mut() {
            value["state"] = json!(if result["state"] == "completed" {
                "completed"
            } else {
                "failed"
            });
            value["updated_at"] = json!(now());
        }
        let mut expected = vec![(COMMANDS, command_id, Some(&previous))];
        let mut records = vec![(COMMANDS, command_id, &updated)];
        if let (Some(before), Some(after)) = (claim.as_ref(), next_claim.as_ref()) {
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

fn pending(state: &AppState, command_id: &str) -> ApiResult<Value> {
    let mut receipt = state
        .store
        .get(COMMANDS, command_id)
        .map_err(ApiError::internal)?
        .ok_or_else(|| ApiError::missing("command receipt"))?["response"]["receipt"]
        .clone();
    receipt["error"] = json!({"code":"command.result_pending",
        "message":"Local archive result needs receipt recovery",
        "category":"state","retryable":true});
    Ok(json!({"receipt":receipt}))
}

fn journal_failure(state: &AppState, command_id: &str, error: Value) -> ApiResult<Value> {
    let result = json!({"command_id":command_id,"state":"failed","output":{},
        "error":error,"created_at":now()});
    match state.store.put_many_if(
        &[(RESULTS, command_id, None)],
        &[(RESULTS, command_id, &result)],
    ) {
        Ok(true) | Ok(false) => {}
        Err(failure) => {
            tracing::warn!(%failure,command_id,"Competition archive result needs recovery");
            return pending(state, command_id);
        }
    }
    finish(state, command_id)?
        .map(|receipt| json!({"receipt":receipt}))
        .ok_or_else(|| ApiError::missing("command receipt"))
}

fn apply(state: &AppState, command_id: &str) -> ApiResult<Value> {
    let command = state
        .store
        .get(COMMANDS, command_id)
        .map_err(ApiError::internal)?
        .ok_or_else(|| ApiError::missing("command receipt"))?;
    if command["response"]["receipt"]["state"] != "accepted" {
        return Ok(json!({"receipt":command["response"]["receipt"]}));
    }
    if state
        .store
        .get(RESULTS, command_id)
        .map_err(ApiError::internal)?
        .is_some()
    {
        return finish(state, command_id)?
            .map(|receipt| json!({"receipt":receipt}))
            .ok_or_else(|| ApiError::missing("command receipt"));
    }
    let competition_id = field(&command["request"], "aggregate_id");
    let previous = &command["plan"]["competition_before"];
    let mut updated = previous.clone();
    let archived = previous["archived"] == true;
    if !archived {
        updated["archived"] = json!(true);
        updated["scheduler_state"] = json!("stopped");
    }
    let result = json!({"command_id":command_id,"state":"completed","output":{},
        "error":null,"created_at":now()});
    let mut records = vec![(RESULTS, command_id, &result)];
    if !archived {
        records.push((COMPETITIONS, competition_id, &updated));
    }
    let applied = match state.store.put_many_if(
        &[
            (COMPETITIONS, competition_id, Some(previous)),
            (RESULTS, command_id, None),
        ],
        &records,
    ) {
        Ok(applied) => applied,
        Err(error) => {
            tracing::warn!(%error,command_id,"Competition archive local effect needs recovery");
            return pending(state, command_id);
        }
    };
    if !applied {
        if state
            .store
            .get(RESULTS, command_id)
            .map_err(ApiError::internal)?
            .is_some()
        {
            return finish(state, command_id)?
                .map(|receipt| json!({"receipt":receipt}))
                .ok_or_else(|| ApiError::missing("command receipt"));
        }
        return journal_failure(
            state,
            command_id,
            json!({"code":"command.side_effect_failed",
            "message":"competition unregister could not be applied",
            "category":"state","retryable":false}),
        );
    }
    finish(state, command_id)?
        .map(|receipt| json!({"receipt":receipt}))
        .ok_or_else(|| ApiError::missing("command receipt"))
}

pub(crate) fn unregister(
    state: &Arc<AppState>,
    competition_id: &str,
    body: &Value,
) -> ApiResult<Value> {
    let command_id = command_id(body);
    if let Some(command) = state
        .store
        .get(COMMANDS, &command_id)
        .map_err(ApiError::internal)?
    {
        if field(&command["request"], "command_type") != "competition.unregister" {
            return Err(ApiError::new(
                StatusCode::CONFLICT,
                "command.idempotency_conflict",
                "command_id was already used with different content",
            ));
        }
        return apply(state, &command_id);
    }
    loop {
        if let Some(receipt) = planned_key_receipt(state, &body["_planned_admission"])? {
            return Ok(json!({"receipt":receipt}));
        }
        let previous = live_competition(state, competition_id)?;
        if stage(state, competition_id, &command_id, &previous, body)? {
            return apply(state, &command_id);
        }
        if let Some(receipt) = prior_receipt(state, &command_id)? {
            return Ok(json!({"receipt":receipt}));
        }
    }
}

pub(crate) fn reconcile(state: &Arc<AppState>) -> ApiResult<()> {
    for command in state.store.list(COMMANDS).map_err(ApiError::internal)? {
        if field(&command["request"], "command_type") != "competition.unregister"
            || command["response"]["receipt"]["state"] != "accepted"
        {
            continue;
        }
        let command_id = field(&command["response"]["receipt"], "command_id");
        if let Err(error) = apply(state, command_id) {
            tracing::warn!(command_id,code=%error.code,
                "Competition archive recovery remains pending");
        }
    }
    Ok(())
}
