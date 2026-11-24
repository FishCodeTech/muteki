//! Credential reference CommandAPI plan and local result.
use super::*;

pub(crate) fn link_credential(
    state: &Arc<AppState>,
    connection_id: &str,
    body: &Value,
) -> ApiResult<Value> {
    let reference = field(body, "credential_ref");
    if !reference.starts_with("secret://platform/") {
        return Err(ApiError::invalid(
            "credential_ref must use the secret://platform namespace",
        ));
    }
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
    let admission = &body["_planned_admission"];
    let previous = if let Some(command) = existing.as_ref() {
        let before = &command["plan"]["connection_before"];
        if before.is_object() {
            before.clone()
        } else {
            live_connection(state, connection_id)?
        }
    } else {
        loop {
            if let Some(receipt) = planned_key_receipt(state, admission)? {
                return Ok(json!({"receipt":receipt}));
            }
            let previous = live_connection(state, connection_id)?;
            if lifecycle::stage(
                state,
                &previous,
                connection_id,
                &command_id,
                "connection.credential.set",
                json!({"connection_id":connection_id,"credential_ref":reference}),
                "competition.connection.credential_updated",
                json!({"connection_id":connection_id,"credential_configured":true}),
                admission,
                body["expected_version"].as_u64(),
            )? {
                break previous;
            }
            if let Some(response) = lifecycle::previous_response(state, &command_id)? {
                return Ok(response);
            }
        }
    };
    let mut updated = previous.clone();
    updated["credential_ref"] = json!(reference);
    updated["status"] = json!("active");
    updated["last_error"] = json!("");
    updated["updated_at"] = json!(now());
    let result = lifecycle::result_record(
        &command_id,
        "completed",
        json!({"connection_id":connection_id,"credential_configured":true}),
        Value::Null,
    );
    let applied = match state.store.put_many_if(
        &[
            (CONNECTIONS, connection_id, Some(&previous)),
            (
                "competition_connection_effect_results",
                command_id.as_str(),
                None,
            ),
        ],
        &[
            (CONNECTIONS, connection_id, &updated),
            (
                "competition_connection_effect_results",
                command_id.as_str(),
                &result,
            ),
        ],
    ) {
        Ok(applied) => applied,
        Err(error) => {
            tracing::warn!(%error,command_id,"Credential local effect needs recovery");
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
            json!({"code":"competition.connection.changed",
                "message":"Connection changed during credential update",
                "category":"state","retryable":false}),
        );
    }
    lifecycle::finish_recorded(state, &command_id)
}

pub(crate) fn revoke_credential(
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
    let reference = field(body, "credential_ref");
    let admission = &body["_planned_admission"];
    let previous = if let Some(command) = existing.as_ref() {
        let before = &command["plan"]["connection_before"];
        if before.is_object() {
            before.clone()
        } else {
            live_connection(state, connection_id)?
        }
    } else {
        loop {
            if let Some(receipt) = planned_key_receipt(state, admission)? {
                return Ok(json!({"receipt":receipt}));
            }
            let previous = live_connection(state, connection_id)?;
            if !reference.is_empty() && reference != field(&previous, "credential_ref") {
                return Err(ApiError::new(
                    StatusCode::CONFLICT,
                    "competition.connection.credential_ref_conflict",
                    "the credential reference no longer matches this connection",
                ));
            }
            if lifecycle::stage(
                state,
                &previous,
                connection_id,
                &command_id,
                "connection.credential.revoke",
                json!({"connection_id":connection_id,"credential_ref":reference}),
                "competition.connection.credential_revoked",
                json!({"connection_id":connection_id,"credential_configured":false}),
                admission,
                body["expected_version"].as_u64(),
            )? {
                break previous;
            }
            if let Some(response) = lifecycle::previous_response(state, &command_id)? {
                return Ok(response);
            }
        }
    };
    let mut updated = previous.clone();
    updated["credential_ref"] = json!("");
    updated["status"] = json!("auth_required");
    updated["last_error"] = json!("credential revoked");
    updated["updated_at"] = json!(now());
    let result = lifecycle::result_record(
        &command_id,
        "completed",
        json!({"connection_id":connection_id,"credential_configured":false}),
        Value::Null,
    );
    let applied = match state.store.put_many_if(
        &[
            (CONNECTIONS, connection_id, Some(&previous)),
            (
                "competition_connection_effect_results",
                command_id.as_str(),
                None,
            ),
        ],
        &[
            (CONNECTIONS, connection_id, &updated),
            (
                "competition_connection_effect_results",
                command_id.as_str(),
                &result,
            ),
        ],
    ) {
        Ok(applied) => applied,
        Err(error) => {
            tracing::warn!(%error,command_id,"Credential revoke local effect needs recovery");
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
            json!({"code":"competition.connection.changed",
                "message":"Connection changed during credential revoke",
                "category":"state","retryable":false}),
        );
    }
    lifecycle::finish_recorded(state, &command_id)
}

pub(crate) fn reconcile_credential(state: &Arc<AppState>) -> ApiResult<()> {
    for command in state.store.list(COMMANDS).map_err(ApiError::internal)? {
        if command["response"]["receipt"]["state"] != "accepted" {
            continue;
        }
        let kind = field(&command["request"], "command_type");
        if !matches!(
            kind,
            "connection.credential.set" | "connection.credential.revoke"
        ) {
            continue;
        }
        let command_id = field(&command["response"]["receipt"], "command_id");
        let connection_id = field(&command["request"], "aggregate_id");
        let mut request = command["request"]["payload"].clone();
        request["command_id"] = json!(command_id);
        let response = if kind == "connection.credential.set" {
            link_credential(state, connection_id, &request)
        } else {
            revoke_credential(state, connection_id, &request)
        };
        if let Err(error) = response {
            tracing::warn!(command_id,code=%error.code,
                "Credential local effect recovery remains pending");
        }
    }
    Ok(())
}

pub(crate) async fn reconcile_direct_cleanup(state: &Arc<AppState>) -> ApiResult<()> {
    for intent in state
        .store
        .list(CREDENTIAL_CLEANUPS)
        .map_err(ApiError::internal)?
    {
        let command_id = field(&intent, "command_id");
        let reference = field(&intent, "reference");
        let set = field(&intent, "kind") == "set";
        let new_reference = field(&intent, "new_reference");
        let expected_type = if set {
            "connection.credential.set"
        } else {
            "connection.credential.revoke"
        };
        let expected_reference = if set { new_reference } else { reference };
        let command = state
            .store
            .get(COMMANDS, command_id)
            .map_err(ApiError::internal)?;
        if command.as_ref().is_some_and(|row| {
            field(&row["request"], "command_type") != expected_type
                || field(&row["request"]["payload"], "credential_ref") != expected_reference
        }) {
            state
                .store
                .delete(CREDENTIAL_CLEANUPS, command_id)
                .map_err(ApiError::internal)?;
            continue;
        }
        let status = command
            .as_ref()
            .map(|row| field(&row["response"]["receipt"], "state"));
        match status {
            Some("completed") => {
                if let Err(error) = remove_secret(state, reference) {
                    tracing::warn!(command_id,code=%error.code,
                        "Direct credential cleanup remains pending");
                    continue;
                }
                notify_secret_change(state, reference, new_reference).await;
                state
                    .store
                    .delete(CREDENTIAL_CLEANUPS, command_id)
                    .map_err(ApiError::internal)?;
            }
            Some("accepted") => {}
            _ => {
                if set && !new_reference.is_empty() {
                    let _ = remove_secret(state, new_reference);
                }
                state
                    .store
                    .delete(CREDENTIAL_CLEANUPS, command_id)
                    .map_err(ApiError::internal)?;
            }
        }
    }
    Ok(())
}
