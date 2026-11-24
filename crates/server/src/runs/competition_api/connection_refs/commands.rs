//! Source-shaped connection creation and browser-session commands.
use super::*;

const IDENTITIES: &str = "competition_connection_identities";

pub(crate) fn create(state: &Arc<AppState>, body: &Value) -> ApiResult<Value> {
    let command_id = command_id(body);
    if let Some(response) = lifecycle::previous_response(state, &command_id)? {
        return Ok(response);
    }
    let admission = &body["_planned_admission"];
    let command_type = if field(body, "command_type").is_empty() {
        "connection.create"
    } else {
        field(body, "command_type")
    };
    let kind = field(body, "platform_kind");
    if !matches!(kind, "ctfd" | "rctf" | "gzctf" | "generic_browser")
        && !extension_adapter::supported(state, kind)?
    {
        return Err(ApiError::unsupported(format!(
            "Platform adapter {kind:?} is unavailable"
        )));
    }
    let base = canonical_url(field(body, "base_url"))?;
    let account = field(body, "account_key");
    if account.is_empty() {
        return Err(ApiError::invalid("account_key is required"));
    }
    let credential_ref = field(body, "credential_ref");
    if !credential_ref.is_empty() && !credential_ref.starts_with("secret://") {
        return Err(ApiError::invalid(
            "credential_ref must use the secret:// namespace",
        ));
    }
    let profile = body
        .get("site_profile")
        .map(validate_site_profile)
        .transpose()?;
    let identity_key =
        serde_json::to_string(&json!([kind, base, account])).map_err(ApiError::internal)?;
    loop {
        if let Some(response) = lifecycle::previous_response(state, &command_id)? {
            return Ok(response);
        }
        if let Some(receipt) = planned_key_receipt(state, admission)? {
            return Ok(json!({"receipt":receipt}));
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
        let identity = state
            .store
            .get(IDENTITIES, &identity_key)
            .map_err(ApiError::internal)?;
        let existing = if let Some(identity) = identity.as_ref() {
            let connection_id = field(identity, "connection_id");
            state
                .store
                .get(CONNECTIONS, connection_id)
                .map_err(ApiError::internal)?
        } else {
            state
                .store
                .list(CONNECTIONS)
                .map_err(ApiError::internal)?
                .into_iter()
                .find(|row| {
                    row["historical"] != true
                        && row["platform_kind"] == kind
                        && row["canonical_base_url"] == base
                        && row["account_key"] == account
                })
        };
        if identity.is_some() && existing.is_none() {
            return Err(ApiError::new(
                StatusCode::CONFLICT,
                "command.outcome_unknown",
                "Connection creation is accepted and awaiting its local effect",
            ));
        }
        let connection_id = existing
            .as_ref()
            .map(|row| field(row, "connection_id").to_owned())
            .unwrap_or_else(|| id("pconn"));
        let mut connection = existing.clone().unwrap_or_else(|| {
            json!({
            "connection_id":connection_id,"platform_kind":kind,"canonical_base_url":base,
            "account_key":account,"credential_ref":credential_ref,"site_profile":{},
            "capabilities":{},"status":"active","last_error":"","archived":false,
            "created_at":now(),"updated_at":now()})
        });
        if let Some(profile) = profile.as_ref() {
            connection["site_profile"] = profile.clone();
        }
        if existing.is_some() {
            if !credential_ref.is_empty() {
                connection["credential_ref"] = json!(credential_ref);
            }
            if connection["archived"] == true {
                connection["archived"] = json!(false);
                connection["status"] = json!("active");
                connection["last_error"] = json!("");
            }
            connection["updated_at"] = json!(now());
        }
        let accepted = lifecycle::receipt(&command_id, &connection_id, "accepted", json!({}));
        let mut stored = json!({"request":{"command_type":command_type,
            "aggregate_id":connection_id,"payload":{"platform_kind":kind,"base_url":base,
                "account_key":account,"credential_ref":credential_ref}},
            "plan":{"connection_before":existing,"connection_after":connection},
            "response":{"receipt":accepted}});
        let claim = admission.get("claim").filter(|value| value.is_object());
        if let Some(claim) = claim {
            stored["request_fingerprint"] = claim["request_fingerprint"].clone();
            stored["status"] = json!(StatusCode::ACCEPTED.as_u16());
        }
        let identity_record = json!({"connection_id":connection_id});
        let event = leases::planned_event(
            "",
            "connection",
            &connection_id,
            "competition.connection.created",
            &command_id,
            json!({"connection_id":connection_id,"platform_kind":kind,
                "canonical_base_url":base,"account_key":account,
                "credential_ref":credential_ref,"already_existed":existing.is_some()}),
        );
        let version_key = format!("connection\0{connection_id}");
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
                "expected_version does not match connection stream",
            ));
        }
        let mut expected = vec![
            (IDENTITIES, identity_key.as_str(), identity.as_ref()),
            (COMMANDS, command_id.as_str(), None),
            (
                "platform_aggregate_versions",
                version_key.as_str(),
                version.as_ref(),
            ),
        ];
        let mut records = vec![
            (IDENTITIES, identity_key.as_str(), &identity_record),
            (COMMANDS, command_id.as_str(), &stored),
        ];
        if let Some(claim) = claim {
            expected.push(("platform_command_claims", command_id.as_str(), None));
            records.push(("platform_command_claims", command_id.as_str(), claim));
            let key_id = field(admission, "key_id");
            if !key_id.is_empty() {
                expected.push(("platform_idempotency_keys", key_id, None));
                records.push(("platform_idempotency_keys", key_id, &admission["keyed"]));
            }
        }
        if !put_records_with_events(state, "", &expected, &records, &[event])? {
            continue;
        }
        return recover_create(state, &command_id)?
            .ok_or_else(|| ApiError::missing("command receipt"));
    }
}

pub(crate) fn recover_create(state: &AppState, command_id: &str) -> ApiResult<Option<Value>> {
    loop {
        let Some(command) = state
            .store
            .get(COMMANDS, command_id)
            .map_err(ApiError::internal)?
        else {
            return Ok(None);
        };
        if !matches!(
            field(&command["request"], "command_type"),
            "connection.create" | "competition.connection.create"
        ) {
            return Ok(None);
        }
        if command["response"]["receipt"]["state"] != "accepted"
            || state
                .store
                .get("competition_connection_effect_results", command_id)
                .map_err(ApiError::internal)?
                .is_some()
        {
            return lifecycle::previous_response(state, command_id);
        }
        let connection_id = field(&command["request"], "aggregate_id");
        let plan = &command["plan"];
        let current = state
            .store
            .get(CONNECTIONS, connection_id)
            .map_err(ApiError::internal)?;
        let before = plan["connection_before"]
            .as_object()
            .map(|_| &plan["connection_before"]);
        if !plan["connection_after"].is_object(){return Err(ApiError::invalid("connection create command lacks its canonical persisted plan"));}
        let after=plan["connection_after"].clone();let created=before.is_none();
        let output = json!({"connection_id":connection_id,"created":created});
        if current.as_ref() == Some(&after) {
            return lifecycle::journal_and_finish(
                state,
                command_id,
                "completed",
                output,
                Value::Null,
            )
            .map(Some);
        }
        if current.as_ref() != before {
            return lifecycle::journal_and_finish(
                state,
                command_id,
                "failed",
                json!({}),
                json!({"code":"command.side_effect_failed",
                    "message":"connection create could not be applied",
                    "category":"internal","retryable":true}),
            )
            .map(Some);
        }
        match state.store.put_many_if(
            &[(CONNECTIONS, connection_id, before)],
            &[(CONNECTIONS, connection_id, &after)],
        ) {
            Ok(true) => {
                return lifecycle::journal_and_finish(
                    state,
                    command_id,
                    "completed",
                    output,
                    Value::Null,
                )
                .map(Some);
            }
            Ok(false) => continue,
            Err(_) => {
                return lifecycle::journal_and_finish(
                    state,
                    command_id,
                    "failed",
                    json!({}),
                    json!({"code":"command.side_effect_failed",
                    "message":"connection create could not be applied",
                    "category":"internal","retryable":true}),
                )
                .map(Some);
            }
        }
    }
}

pub(crate) fn reconcile_create(state: &AppState) -> ApiResult<()> {
    for command in state.store.list(COMMANDS).map_err(ApiError::internal)? {
        if command["response"]["receipt"]["state"] != "accepted"
            || !matches!(
                field(&command["request"], "command_type"),
                "connection.create" | "competition.connection.create"
            )
        {
            continue;
        }
        let command_id = field(&command["response"]["receipt"], "command_id");
        if let Err(error) = recover_create(state, command_id) {
            tracing::warn!(command_id,code=%error.code,"Connection create local effect recovery pending");
        }
    }
    Ok(())
}

pub(crate) async fn browser_session(
    state: &Arc<AppState>,
    connection_id: &str,
    body: &Value,
    renew: bool,
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
    let kind = if renew {
        "connection.browser_session.renew"
    } else {
        "connection.browser_session.revoke"
    };
    let expires_at = browser_expiry(body)?;
    let previous = if let Some(command) = existing.as_ref() {
        let before = &command["plan"]["connection_before"];
        if before.is_object() {
            before.clone()
        } else {
            live_connection(state, connection_id)?
        }
    } else {
        loop {
            if let Some(receipt) = planned_key_receipt(state, &body["_planned_admission"])? {
                return Ok(json!({"receipt":receipt}));
            }
            let previous = live_connection(state, connection_id)?;
            if previous["platform_kind"] != "generic_browser" {
                return Err(ApiError::invalid(
                    "browser storage state is only valid for generic_browser connections",
                ));
            }
            let event_type = if renew {
                "competition.connection.browser_session_updated"
            } else {
                "competition.connection.browser_session_revoked"
            };
            if lifecycle::stage(
                state,
                &previous,
                connection_id,
                &command_id,
                kind,
                json!({"connection_id":connection_id,"expires_at":expires_at}),
                event_type,
                json!({"connection_id":connection_id,"expires_at":expires_at,
                "operation":if renew {"renew"} else {"revoke"}}),
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
    let result = if renew {
        (|| -> ApiResult<(Value, String)> {
            let browser = state
                .store
                .get(BROWSER, connection_id)
                .map_err(ApiError::internal)?
                .ok_or_else(|| ApiError::missing("browser session"))?;
            let reference = field(&browser, "reference");
            let prior_metadata = state
                .store
                .get(SECRETS, reference)
                .map_err(ApiError::internal)?
                .ok_or_else(|| ApiError::missing("browser session secret"))?;
            let mut metadata = prior_metadata.clone();
            let mut updated_browser = browser.clone();
            let mut updated_connection = previous.clone();
            metadata["expires_at"] = json!(expires_at);
            metadata["updated_at"] = json!(now());
            updated_browser["expires_at"] = json!(expires_at);
            updated_connection["status"] = json!("active");
            updated_connection["last_error"] = json!("");
            updated_connection["updated_at"] = json!(now());
            let status = browser_status_record(Some(&updated_browser));
            let result = lifecycle::result_record(
                &command_id,
                "completed",
                json!({"connection_id":connection_id,"browser_session":status}),
                Value::Null,
            );
            if !state
                .store
                .put_many_if(
                    &[
                        (BROWSER, connection_id, Some(&browser)),
                        (SECRETS, reference, Some(&prior_metadata)),
                        (CONNECTIONS, connection_id, Some(&previous)),
                        (
                            "competition_connection_effect_results",
                            command_id.as_str(),
                            None,
                        ),
                    ],
                    &[
                        (BROWSER, connection_id, &updated_browser),
                        (SECRETS, reference, &metadata),
                        (CONNECTIONS, connection_id, &updated_connection),
                        (
                            "competition_connection_effect_results",
                            command_id.as_str(),
                            &result,
                        ),
                    ],
                )
                .map_err(ApiError::internal)?
            {
                return Err(ApiError::new(
                    StatusCode::CONFLICT,
                    "competition.connection.changed",
                    "Browser session changed during renewal",
                ));
            }
            Ok((status, String::new()))
        })()
    } else {
        (|| -> ApiResult<(Value, String)> {
            let browser = state
                .store
                .get(BROWSER, connection_id)
                .map_err(ApiError::internal)?;
            let removed = browser.is_some();
            let old_reference = browser
                .as_ref()
                .map(|row| field(row, "reference").to_owned())
                .unwrap_or_default();
            let old_metadata = if old_reference.is_empty() {
                None
            } else {
                state
                    .store
                    .get(SECRETS, &old_reference)
                    .map_err(ApiError::internal)?
            };
            let mut updated = previous.clone();
            updated["status"] = json!("auth_required");
            updated["last_error"] = json!("browser session revoked");
            updated["updated_at"] = json!(now());
            let status = json!({"present":false,"revoked":removed});
            let mut result = lifecycle::result_record(
                &command_id,
                "completed",
                json!({"connection_id":connection_id,"browser_session":status}),
                Value::Null,
            );
            result["revoked_reference"] = json!(old_reference);
            if !state
                .store
                .put_many_if_with_deletes(
                    &[
                        (CONNECTIONS, connection_id, Some(&previous)),
                        (BROWSER, connection_id, browser.as_ref()),
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
                    |_| {
                        Ok(Some({
                            let mut deletes = Vec::new();
                            if removed {
                                deletes.push((BROWSER.to_owned(), connection_id.to_owned()));
                            }
                            if old_metadata.is_some() {
                                deletes.push((SECRETS.to_owned(), old_reference.clone()));
                            }
                            deletes
                        }))
                    },
                )
                .map_err(ApiError::internal)?
            {
                return Err(ApiError::new(
                    StatusCode::CONFLICT,
                    "competition.connection.changed",
                    "Connection changed during browser revoke",
                ));
            }
            if !old_reference.is_empty() {
                let (secret_connection, secret_key) = parse_secret_ref(&old_reference)?;
                let path = secret_path(state, secret_connection, secret_key)?;
                if let Err(error) = fs::remove_file(&path) {
                    if error.kind() != std::io::ErrorKind::NotFound {
                        tracing::warn!(%error,"Revoked browser state file cleanup pending");
                    }
                }
            }
            Ok((status, old_reference))
        })()
    };
    match result {
        Ok((browser_session, old_reference)) => {
            if !old_reference.is_empty() {
                notify_secret_change(state, &old_reference, "").await;
            }
            let _ = browser_session;
            lifecycle::finish_recorded(state, &command_id)
        }
        Err(error) => {
            if state
                .store
                .get("competition_connection_effect_results", &command_id)
                .map_err(ApiError::internal)?
                .is_some()
            {
                return lifecycle::finish_recorded(state, &command_id);
            }
            lifecycle::journal_and_finish(
                state,
                &command_id,
                "failed",
                json!({}),
                json!({"code":error.code,"message":error.message,
                    "category":"state","retryable":false}),
            )
        }
    }
}

pub(super) async fn import(
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
    let state_ref = field(body, "state_ref").to_owned();
    if !state_ref.starts_with("secret://platform/") {
        return Err(ApiError::invalid(
            "state_ref must be a one-time secret://platform reference",
        ));
    }
    let expires_at = browser_expiry(body)?;
    let previous = if let Some(command) = existing.as_ref() {
        let before = &command["plan"]["connection_before"];
        if before.is_object() {
            before.clone()
        } else {
            live_connection(state, connection_id)?
        }
    } else {
        loop {
            if let Some(receipt) = planned_key_receipt(state, &body["_planned_admission"])? {
                return Ok(json!({"receipt":receipt}));
            }
            let previous = live_connection(state, connection_id)?;
            if previous["platform_kind"] != "generic_browser" {
                return Err(ApiError::invalid(
                    "browser storage state is only valid for generic_browser connections",
                ));
            }
            if lifecycle::stage(
                state,
                &previous,
                connection_id,
                &command_id,
                "connection.browser_session.import",
                json!({"connection_id":connection_id,"state_ref":state_ref,"expires_at":expires_at}),
                "competition.connection.browser_session_updated",
                json!({"connection_id":connection_id,"expires_at":expires_at,"operation":"import"}),
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
    let result = (|| -> ApiResult<(Value, String, String)> {
        let serialized = read_secret(state, &state_ref)?;
        let mut storage: Value = serde_json::from_str(&serialized)
            .map_err(|_| ApiError::invalid("browser storage state must be JSON"))?;
        if storage.is_object() {
            if storage.get("cookies").is_none() {
                storage["cookies"] = json!([])
            }
            if storage.get("origins").is_none() {
                storage["origins"] = json!([])
            }
        }
        if !storage.is_object() || !storage["cookies"].is_array() || !storage["origins"].is_array()
        {
            return Err(ApiError::invalid(
                "storage_state requires cookies and origins arrays",
            ));
        }
        let serialized = serde_json::to_string(&storage).map_err(ApiError::internal)?;
        let prior_browser = state
            .store
            .get(BROWSER, connection_id)
            .map_err(ApiError::internal)?;
        let old_reference = prior_browser
            .as_ref()
            .map(|row| field(row, "reference").to_owned())
            .unwrap_or_default();
        let secret_key = format!("browser-state-{}", uuid::Uuid::new_v4().simple());
        let metadata = write_secret(
            state,
            connection_id,
            &secret_key,
            &serialized,
            &expires_at,
            1,
        )?;
        let new_reference = field(&metadata, "reference").to_owned();
        let browser = json!({"reference":new_reference,"imported_at":now(),"expires_at":expires_at,
            "cookies":storage["cookies"].as_array().map_or(0,Vec::len),
            "origins":storage["origins"].as_array().map_or(0,Vec::len)});
        let status = browser_status_record(Some(&browser));
        let mut connection = previous.clone();
        connection["status"] = json!("active");
        connection["last_error"] = json!("");
        connection["updated_at"] = json!(now());
        let mut result = lifecycle::result_record(
            &command_id,
            "completed",
            json!({"connection_id":connection_id,"browser_session":status}),
            Value::Null,
        );
        result["old_reference"] = json!(old_reference);
        result["new_reference"] = json!(new_reference);
        let applied = match state.store.put_many_if(
            &[
                (BROWSER, connection_id, prior_browser.as_ref()),
                (CONNECTIONS, connection_id, Some(&previous)),
                (
                    "competition_connection_effect_results",
                    command_id.as_str(),
                    None,
                ),
            ],
            &[
                (BROWSER, connection_id, &browser),
                (CONNECTIONS, connection_id, &connection),
                (
                    "competition_connection_effect_results",
                    command_id.as_str(),
                    &result,
                ),
            ],
        ) {
            Ok(applied) => applied,
            Err(error) => {
                if state
                    .store
                    .get("competition_connection_effect_results", &command_id)
                    .map_err(ApiError::internal)?
                    .is_none()
                {
                    let _ = remove_secret(state, &new_reference);
                }
                return Err(ApiError::internal(error));
            }
        };
        if !applied {
            let _ = remove_secret(state, &new_reference);
            return Err(ApiError::new(
                StatusCode::CONFLICT,
                "competition.connection.changed",
                "Browser session changed during import",
            ));
        }
        if !old_reference.is_empty() && old_reference != new_reference {
            if let Err(error) = remove_secret(state, &old_reference) {
                tracing::warn!(code=%error.code,"Previous browser state secret cleanup pending");
            }
        }
        Ok((status, old_reference, new_reference))
    })();
    match result {
        Ok((_status, old_reference, new_reference)) => {
            let _ = remove_secret(state, &state_ref);
            notify_secret_change(state, &old_reference, &new_reference).await;
            lifecycle::finish_recorded(state, &command_id)
        }
        Err(error) => {
            if state
                .store
                .get("competition_connection_effect_results", &command_id)
                .map_err(ApiError::internal)?
                .is_some()
            {
                let _ = remove_secret(state, &state_ref);
                return lifecycle::finish_recorded(state, &command_id);
            }
            let response = lifecycle::journal_and_finish(
                state,
                &command_id,
                "failed",
                json!({}),
                json!({"code":error.code,"message":error.message,
                    "category":"state","retryable":false}),
            )?;
            if response["receipt"]["state"] != "accepted" {
                let _ = remove_secret(state, &state_ref);
            }
            Ok(response)
        }
    }
}

pub(crate) async fn reconcile_browser_import(state: &Arc<AppState>) -> ApiResult<()> {
    for command in state.store.list(COMMANDS).map_err(ApiError::internal)? {
        if field(&command["request"], "command_type") != "connection.browser_session.import" {
            continue;
        }
        let command_id = field(&command["response"]["receipt"], "command_id");
        let connection_id = field(&command["request"], "aggregate_id");
        let mut request = command["request"]["payload"].clone();
        if let Some(result) = state
            .store
            .get("competition_connection_effect_results", command_id)
            .map_err(ApiError::internal)?
        {
            let _ = remove_secret(state, field(&request, "state_ref"));
            let old_reference = field(&result, "old_reference");
            if !old_reference.is_empty() && old_reference != field(&result, "new_reference") {
                let _ = remove_secret(state, old_reference);
            }
            continue;
        }
        if command["response"]["receipt"]["state"] != "accepted" {
            continue;
        }
        request["command_id"] = json!(command_id);
        if let Err(error) = import(state, connection_id, &request).await {
            tracing::warn!(command_id,code=%error.code,
                "Browser import local effect recovery remains pending");
        }
    }
    Ok(())
}

pub(crate) async fn reconcile_browser_session(state: &Arc<AppState>) -> ApiResult<()> {
    for command in state.store.list(COMMANDS).map_err(ApiError::internal)? {
        let kind = field(&command["request"], "command_type");
        let renew = match kind {
            "connection.browser_session.renew" => true,
            "connection.browser_session.revoke" => false,
            _ => continue,
        };
        let command_id = field(&command["response"]["receipt"], "command_id");
        if let Some(result) = state
            .store
            .get("competition_connection_effect_results", command_id)
            .map_err(ApiError::internal)?
        {
            if !renew {
                let reference = field(&result, "revoked_reference");
                if !reference.is_empty() {
                    let _ = remove_secret(state, reference);
                }
            }
            continue;
        }
        if command["response"]["receipt"]["state"] != "accepted" {
            continue;
        }
        let connection_id = field(&command["request"], "aggregate_id");
        let mut request = command["request"]["payload"].clone();
        request["command_id"] = json!(command_id);
        if let Err(error) = browser_session(state, connection_id, &request, renew).await {
            tracing::warn!(command_id,code=%error.code,
                "Browser session local effect recovery remains pending");
        }
    }
    Ok(())
}
