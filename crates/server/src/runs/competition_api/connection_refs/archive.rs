//! Connection unregister plan, local archive, and cleanup recovery.
use super::*;

pub(crate) fn unregister(
    state: &Arc<AppState>,
    connection_id: &str,
    body: &Value,
) -> ApiResult<Value> {
    let command_id = command_id(body);
    let existing = state
        .store
        .get(COMMANDS, &command_id)
        .map_err(ApiError::internal)?;
    if let Some(response) = lifecycle::previous_response(state, &command_id)? {
        if response["receipt"]["state"] != "accepted" {
            return Ok(response);
        }
    }
    let previous = if let Some(command) = existing.as_ref() {
        let before = &command["plan"]["connection_before"];
        if before.is_object() {
            before.clone()
        } else {
            state
                .store
                .get(CONNECTIONS, connection_id)
                .map_err(ApiError::internal)?
                .ok_or_else(|| ApiError::missing("connection"))?
        }
    } else {
        loop {
            if let Some(receipt) = planned_key_receipt(state, &body["_planned_admission"])? {
                return Ok(json!({"receipt":receipt}));
            }
            let previous = state
                .store
                .get(CONNECTIONS, connection_id)
                .map_err(ApiError::internal)?
                .filter(|row| row["historical"] != true)
                .ok_or_else(|| ApiError::missing("connection"))?;
            if previous["archived"] != true {
                let bound = state
                    .store
                    .list(COMPETITIONS)
                    .map_err(ApiError::internal)?
                    .iter()
                    .filter(|row| {
                        row["connection_id"] == connection_id
                            && row["archived"] != true
                            && row["historical"] != true
                    })
                    .count();
                if bound != 0 {
                    return Err(ApiError::new(
                        StatusCode::CONFLICT,
                        "competition.connection.has_bound_competitions",
                        format!(
                            "connection {connection_id} still has {bound} registered competition(s); remove them from the inventory first"
                        ),
                    ));
                }
            }
            let event_type = if previous["archived"] == true {
                ""
            } else {
                "competition.connection.unregistered"
            };
            if lifecycle::stage(
                state,
                &previous,
                connection_id,
                &command_id,
                "connection.unregister",
                json!({"connection_id":connection_id}),
                event_type,
                json!({"connection_id":connection_id}),
                &body["_planned_admission"],
                body["expected_version"].as_u64(),
            )? {
                break previous;
            }
            if let Some(response) = lifecycle::previous_response(state, &command_id)? {
                return Ok(response);
            }
        }
    };
    let archived = previous["archived"] == true;
    let mut updated = previous.clone();
    if !archived {
        updated["archived"] = json!(true);
        updated["status"] = json!("disabled");
        updated["credential_ref"] = json!("");
        updated["last_error"] = json!("");
    }
    let result = lifecycle::result_record(&command_id, "completed", json!({}), Value::Null);
    let mut records = vec![(
        "competition_connection_effect_results",
        command_id.as_str(),
        &result,
    )];
    if !archived {
        records.push((CONNECTIONS, connection_id, &updated));
    }
    let applied = match state.store.put_many_if(
        &[
            (CONNECTIONS, connection_id, Some(&previous)),
            (
                "competition_connection_effect_results",
                command_id.as_str(),
                None,
            ),
        ],
        &records,
    ) {
        Ok(applied) => applied,
        Err(error) => {
            tracing::warn!(%error,command_id,"Connection archive local effect needs recovery");
            return lifecycle::pending(state, &command_id);
        }
    };
    if !applied {
        if state
            .store
            .get("competition_connection_effect_results", &command_id)
            .map_err(ApiError::internal)?
            .is_some()
        {
            return lifecycle::finish_recorded(state, &command_id);
        }
        return lifecycle::journal_and_finish(
            state,
            &command_id,
            "failed",
            json!({}),
            json!({"code":"command.side_effect_failed",
                "message":"connection unregister could not be applied",
                "category":"state","retryable":false}),
        );
    }
    if !archived {
        lifecycle::cleanup_secrets(state, connection_id, field(&previous, "credential_ref"));
    }
    lifecycle::finish_recorded(state, &command_id)
}

pub(crate) fn reconcile_unregistered(state: &Arc<AppState>) -> ApiResult<()> {
    for command in state.store.list(COMMANDS).map_err(ApiError::internal)? {
        if field(&command["request"], "command_type") != "connection.unregister" {
            continue;
        }
        let command_id = field(&command["response"]["receipt"], "command_id");
        let connection_id = field(&command["request"], "aggregate_id");
        if command["response"]["receipt"]["state"] == "accepted" {
            let mut request = command["request"]["payload"].clone();
            request["command_id"] = json!(command_id);
            if let Err(error) = unregister(state, connection_id, &request) {
                tracing::warn!(command_id,code=%error.code,
                    "Connection archive recovery remains pending");
            }
        }
        if state
            .store
            .get("competition_connection_effect_results", command_id)
            .map_err(ApiError::internal)?
            .is_some()
            && command["plan"]["connection_before"]["archived"] != true
            && state
                .store
                .get(CONNECTIONS, connection_id)
                .map_err(ApiError::internal)?
                .is_some_and(|row| row["archived"] == true)
        {
            lifecycle::cleanup_secrets(
                state,
                connection_id,
                field(&command["plan"]["connection_before"], "credential_ref"),
            );
        }
    }
    Ok(())
}
