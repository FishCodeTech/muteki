//! Direct HTTP writes share the command dispatch lock and advance the same
//! durable aggregate version used by `/api/commands`.
use super::*;

macro_rules! path_write {
    ($wrapper:ident, $handler:path, $aggregate:literal) => {
        pub(super) async fn $wrapper(
            State(state): State<Arc<AppState>>,
            Path(id): Path<String>,
            Json(body): Json<Value>,
        ) -> ApiResult<Json<Value>> {
            let _dispatch = crate::operations_api::command_dispatch_lock().lock().await;
            let duplicate = prior_receipt(&state, &command_id(&body))?.is_some();
            let before = crate::operations_api::current_aggregate_version(&state, $aggregate, &id)
                .map_err(ApiError::internal)?;
            let result = $handler(State(state.clone()), Path(id.clone()), Json(body)).await?;
            if !duplicate {
                let after =
                    crate::operations_api::current_aggregate_version(&state, $aggregate, &id)
                        .map_err(ApiError::internal)?;
                if after == before {
                    crate::operations_api::advance_direct_version(&state, $aggregate, &id)
                        .map_err(ApiError::internal)?;
                }
            }
            Ok(result)
        }
    };
}
pub(super) async fn create_connection(
    State(state): State<Arc<AppState>>,
    Json(body): Json<Value>,
) -> Response {
    let platform_kind = if field(&body, "platform_kind").is_empty() {
        field(&body, "platform")
    } else {
        field(&body, "platform_kind")
    };
    let mut payload = json!({"platform_kind":platform_kind,"base_url":field(&body,"base_url"),
        "account_key":field(&body,"account_key")});
    if !field(&body, "credential_ref").is_empty() {
        payload["credential_ref"] = body["credential_ref"].clone();
    }
    if body.get("site_profile").is_some() {
        payload["site_profile"] = body["site_profile"].clone();
    }
    let envelope = json!({"command_type":"connection.create","command_id":body["command_id"],
        "idempotency_key":body["idempotency_key"],"aggregate_type":"connection",
        "aggregate_id":"","payload":payload});
    let response =
        crate::operations_api::dispatch_local_competition_command(state.clone(), envelope).await;
    let Some(credential) = body["credential"].as_str() else {
        return response;
    };
    let status = response.status();
    let bytes = match axum::body::to_bytes(response.into_body(), usize::MAX).await {
        Ok(bytes) => bytes,
        Err(error) => return ApiError::internal(error).into_response(),
    };
    let result: Value = match serde_json::from_slice(&bytes) {
        Ok(result) => result,
        Err(error) => return ApiError::internal(error).into_response(),
    };
    if result["receipt"]["state"] != "completed" {
        return (status, Json(result)).into_response();
    }
    if !field(&body, "credential_ref").is_empty() {
        return ApiError::new(
            StatusCode::BAD_REQUEST,
            "competition.secret.input_conflict",
            "provide credential or credential_ref, not both",
        )
        .into_response();
    }
    let connection_id = field(&result["receipt"]["aggregate"], "id");
    let key = field(&body, "credential_key").to_owned().if_empty("token");
    let secret = match write_secret(&state, connection_id, &key, credential, "", 1) {
        Ok(secret) => secret,
        Err(error) => return error.into_response(),
    };
    let parent_command = field(&result["receipt"], "command_id");
    let idempotency_key = field(&body, "idempotency_key");
    let child = json!({"command_type":"connection.credential.set",
        "command_id":format!("{parent_command}:credential"),
        "idempotency_key":if idempotency_key.is_empty() {String::new()}
            else {format!("{idempotency_key}:credential")},
        "aggregate_type":"connection","aggregate_id":connection_id,
        "payload":{"connection_id":connection_id,"credential_ref":secret["reference"]}});
    let child_response =
        crate::operations_api::dispatch_local_competition_command(state, child).await;
    let child_status = child_response.status();
    let child_bytes = match axum::body::to_bytes(child_response.into_body(), usize::MAX).await {
        Ok(bytes) => bytes,
        Err(error) => return ApiError::internal(error).into_response(),
    };
    let child_result: Value = match serde_json::from_slice(&child_bytes) {
        Ok(result) => result,
        Err(error) => return ApiError::internal(error).into_response(),
    };
    (
        child_status,
        Json(json!({"receipt":child_result["receipt"],
        "connection_receipt":result["receipt"],"secret":secret})),
    )
        .into_response()
}
path_write!(patch_connection, super::patch_connection, "connection");
pub(super) async fn delete_connection(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    body: Option<Json<Value>>,
) -> Response {
    let body = body.map(|Json(value)| value).unwrap_or_else(|| json!({}));
    if body["confirm"] != true {
        return ApiError::new(
            StatusCode::BAD_REQUEST,
            "connection.unregister.confirm_required",
            "删除连接需要 confirm=true",
        )
        .into_response();
    }
    let envelope = json!({"command_type":"connection.unregister",
        "command_id":body["command_id"],"idempotency_key":body["idempotency_key"],
        "aggregate_type":"connection","aggregate_id":id,
        "payload":{"connection_id":id}});
    crate::operations_api::dispatch_local_competition_command(state, envelope).await
}
pub(super) async fn probe_connection(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    body: Option<Json<Value>>,
) -> Response {
    let body = body.map(|Json(body)| body).unwrap_or_else(|| json!({}));
    let envelope = json!({"command_type":"connection.test","command_id":body["command_id"],
        "idempotency_key":body["idempotency_key"],
        "aggregate_type":"connection","aggregate_id":id,
        "payload":{"connection_id":id}});
    crate::operations_api::dispatch_local_competition_command(state, envelope).await
}
pub(super) async fn put_connection_secret(
    State(state): State<Arc<AppState>>,
    Path(connection_id): Path<String>,
    Json(body): Json<Value>,
) -> Response {
    match super::put_connection_secret(State(state), Path(connection_id), Json(body)).await {
        Ok(Json(result)) => {
            let status = match field(&result["receipt"], "state") {
                "accepted" | "waiting" | "running" => StatusCode::ACCEPTED,
                "conflict" | "failed" => StatusCode::CONFLICT,
                _ => StatusCode::OK,
            };
            (status, Json(result)).into_response()
        }
        Err(error) => error.into_response(),
    }
}
pub(super) async fn delete_connection_secret(
    State(state): State<Arc<AppState>>,
    Path(connection_id): Path<String>,
    Json(body): Json<Value>,
) -> Response {
    let connection = match live_connection(&state, &connection_id) {
        Ok(value) => value,
        Err(error) => return error.into_response(),
    };
    if body["confirm"] != true {
        return ApiError::new(
            StatusCode::BAD_REQUEST,
            "competition.secret.confirmation_required",
            "credential revocation requires confirm=true",
        )
        .into_response();
    }
    let reference = field(&connection, "credential_ref").to_owned();
    if reference.is_empty() {
        return ApiError::new(
            StatusCode::NOT_FOUND,
            "competition.secret.not_found",
            "this connection has no active credential",
        )
        .into_response();
    }
    let command_id = field(&body, "command_id")
        .to_owned()
        .if_empty(&super::id("cmd"));
    let intent = json!({"command_id":command_id,"reference":reference,"created_at":now()});
    if let Err(error) = state.store.put(CREDENTIAL_CLEANUPS, &command_id, &intent) {
        return ApiError::internal(error).into_response();
    }
    let envelope = json!({"command_type":"connection.credential.revoke",
        "command_id":command_id,"idempotency_key":body["idempotency_key"],
        "aggregate_type":"connection","aggregate_id":connection_id,
        "payload":{"connection_id":connection_id,"credential_ref":reference}});
    let response =
        crate::operations_api::dispatch_local_competition_command(state.clone(), envelope).await;
    let status = response.status();
    let bytes = match axum::body::to_bytes(response.into_body(), usize::MAX).await {
        Ok(value) => value,
        Err(error) => return ApiError::internal(error).into_response(),
    };
    let result: Value = match serde_json::from_slice(&bytes) {
        Ok(value) => value,
        Err(error) => return ApiError::internal(error).into_response(),
    };
    if result["receipt"].is_null() {
        if field(&result["error"], "code") != "command.outcome_unknown" {
            let _ = state.store.delete(CREDENTIAL_CLEANUPS, &command_id);
        }
        return (status, Json(result)).into_response();
    }
    if result["receipt"]["state"] == "completed" {
        if let Err(error) = remove_secret(&state, &reference) {
            return error.into_response();
        }
        notify_secret_change(&state, &reference, "").await;
        let _ = state.store.delete(CREDENTIAL_CLEANUPS, &command_id);
    } else if result["receipt"]["state"] != "accepted"
        && field(&result["error"], "code") != "command.outcome_unknown"
    {
        let _ = state.store.delete(CREDENTIAL_CLEANUPS, &command_id);
    }
    (
        status,
        Json(json!({"receipt":result["receipt"],
        "revoked_reference":reference,"change_effect":{}})),
    )
        .into_response()
}
pub(super) async fn put_browser_session(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> Response {
    if let Err(error) = live_connection(&state, &id) {
        return error.into_response();
    }
    let renew = body["renew"] == true;
    let state_ref = if renew {
        String::new()
    } else {
        let storage = body
            .get("storage_state")
            .cloned()
            .unwrap_or_else(|| json!({}));
        let serialized = match serde_json::to_string(&storage) {
            Ok(value) => value,
            Err(error) => return ApiError::internal(error).into_response(),
        };
        let key = super::id("browser_state").replace('-', "_");
        match write_secret(&state, &id, &key, &serialized, "", 1) {
            Ok(metadata) => field(&metadata, "reference").to_owned(),
            Err(error) => return error.into_response(),
        }
    };
    let command_id = field(&body, "command_id")
        .to_owned()
        .if_empty(&super::id("cmd"));
    let envelope = json!({"command_type":if renew {"connection.browser_session.renew"}
        else {"connection.browser_session.import"},
        "command_id":command_id,"idempotency_key":body["idempotency_key"],
        "aggregate_type":"connection","aggregate_id":id,
        "payload":{"connection_id":id,"state_ref":state_ref,
            "expires_at":body.get("expires_at").cloned().unwrap_or(Value::Null)}});
    let response =
        crate::operations_api::dispatch_local_competition_command(state.clone(), envelope).await;
    if renew || !response.status().is_client_error() {
        return response;
    }
    let status = response.status();
    let bytes = match axum::body::to_bytes(response.into_body(), usize::MAX).await {
        Ok(value) => value,
        Err(error) => return ApiError::internal(error).into_response(),
    };
    let result: Value = match serde_json::from_slice(&bytes) {
        Ok(value) => value,
        Err(error) => return ApiError::internal(error).into_response(),
    };
    if field(&result["error"], "code") != "command.outcome_unknown" {
        let _ = remove_secret(&state, &state_ref);
    }
    (status, Json(result)).into_response()
}
pub(super) async fn delete_browser_session(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    body: Option<Json<Value>>,
) -> Response {
    let body = body.map(|Json(body)| body).unwrap_or_else(|| json!({}));
    if body["confirm"] != true {
        return ApiError::new(
            StatusCode::BAD_REQUEST,
            "competition.connection.browser_confirmation_required",
            "browser session revocation requires confirm=true",
        )
        .into_response();
    }
    let envelope = json!({"command_type":"connection.browser_session.revoke",
        "command_id":body["command_id"],"idempotency_key":body["idempotency_key"],
        "aggregate_type":"connection","aggregate_id":id,
        "payload":{"connection_id":id}});
    crate::operations_api::dispatch_local_competition_command(state, envelope).await
}
pub(super) async fn create_competition(
    State(state): State<Arc<AppState>>,
    Json(body): Json<Value>,
) -> Response {
    let mut payload = json!({"connection_id":field(&body,"connection_id"),
        "external_competition_id":field(&body,"external_competition_id")});
    for key in ["title", "description", "starts_at", "ends_at"] {
        if body
            .get(key)
            .is_some_and(|value| value.as_str().is_some_and(|text| !text.is_empty()))
        {
            payload[key] = body[key].clone();
        }
    }
    let envelope = json!({"command_type":"competition.sync","command_id":body["command_id"],
        "idempotency_key":body["idempotency_key"],
        "aggregate_type":"competition","aggregate_id":"","payload":payload});
    crate::operations_api::dispatch_local_competition_command(state, envelope).await
}
path_write!(patch_competition, super::patch_competition, "competition");
pub(super) async fn delete_competition(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    body: Option<Json<Value>>,
) -> Response {
    let body = body.map(|Json(value)| value).unwrap_or_else(|| json!({}));
    if body["confirm"] != true {
        return ApiError::new(
            StatusCode::BAD_REQUEST,
            "competition.unregister.confirm_required",
            "删除比赛需要 confirm=true",
        )
        .into_response();
    }
    let envelope = json!({"command_type":"competition.unregister",
        "command_id":body["command_id"],"idempotency_key":body["idempotency_key"],
        "aggregate_type":"competition","aggregate_id":id,
        "payload":{"competition_id":id}});
    crate::operations_api::dispatch_local_competition_command(state, envelope).await
}
pub(super) async fn competition_command(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(mut body): Json<Value>,
) -> Response {
    let raw_kind = if field(&body, "command_type").is_empty() {
        field(&body, "kind")
    } else {
        field(&body, "command_type")
    }
    .to_owned();
    let kind = gateway::canonical_command_type(&raw_kind).to_owned();
    body["command_type"] = json!(kind);
    let planned = binding_outbox::is_binding_command(&kind)
        || matches!(
            kind.as_str(),
            "challenge.select"
                | "challenge.queue"
                | "competition.challenge.queue"
                | "challenge.skip"
                | "policy.update"
                | "competition.policy.update"
                | "scheduler.start"
                | "scheduler.pause"
                | "scheduler.resume"
                | "instance.ensure"
                | "instance.stop"
                | "competition.submission.submit"
                | "platform_submission.approve"
                | "platform_submission.retry"
                | "competition.submission.manual_override"
                | "competition.message"
                | "competition.projection.rebuild"
                | "competition.projection.rebuild_all"
        );
    if planned {
        if body["payload"].is_null() {
            body["payload"] = json!({});
        }
        if body["payload"].is_object() && body["payload"].get("competition_id").is_none() {
            body["payload"]["competition_id"] = json!(id);
        }
        let default_agg = if binding_outbox::is_binding_command(&kind)
            && !field(&body["payload"], "binding_id").is_empty()
        {
            "run_binding"
        } else {
            "competition"
        };
        let aggregate_type = field(&body, "aggregate_type")
            .to_owned()
            .if_empty(default_agg);
        let default_agg_id = if aggregate_type == "run_binding"
            && !field(&body["payload"], "binding_id").is_empty()
        {
            field(&body["payload"], "binding_id")
        } else {
            id.as_str()
        };
        let aggregate_id = field(&body, "aggregate_id")
            .to_owned()
            .if_empty(default_agg_id);
        let envelope = json!({"command_type":kind,"command_id":body["command_id"],
            "aggregate_type":aggregate_type,"aggregate_id":aggregate_id,
            "payload":body["payload"],"idempotency_key":body["idempotency_key"],
            "expected_version":body["expected_version"]});
        return crate::operations_api::dispatch_local_competition_command(state, envelope).await;
    }
    let _dispatch = crate::operations_api::command_dispatch_lock().lock().await;
    let duplicate = match prior_receipt(&state, &command_id(&body)) {
        Ok(value) => value.is_some(),
        Err(error) => return error.into_response(),
    };
    let before = match crate::operations_api::current_aggregate_version(&state, "competition", &id)
    {
        Ok(value) => value,
        Err(error) => return ApiError::internal(error).into_response(),
    };
    match super::competition_command(State(state.clone()), Path(id.clone()), Json(body.clone()))
        .await
    {
        Ok(Json(value)) => {
            if !duplicate {
                let after = match crate::operations_api::current_aggregate_version(
                    &state,
                    "competition",
                    &id,
                ) {
                    Ok(value) => value,
                    Err(error) => return ApiError::internal(error).into_response(),
                };
                if after == before {
                    if let Err(error) =
                        crate::operations_api::advance_direct_version(&state, "competition", &id)
                    {
                        return ApiError::internal(error).into_response();
                    }
                }
            }
            Json(value).into_response()
        }
        Err(error) => error.into_response(),
    }
}
pub(super) async fn competition_message(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> Response {
    super::competition_message_response(state, id, body).await
}
