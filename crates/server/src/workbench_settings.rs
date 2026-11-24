use crate::AppState;
use axum::{
    Json,
    extract::{Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde_json::{Value, json};
use std::{collections::HashMap, sync::Arc};

pub const TARGETS: &[&str] = &[
    "control",
    "control_policy",
    "execution_guardrail",
    "browser",
    "agent_version_policy",
    "competition_advisor",
    "runner_listener",
    "host_credential",
    "access",
    "system_update",
];

pub fn redact_request(value: &mut Value) {
    match value {
        Value::Object(fields) => {
            for (key, value) in fields {
                if matches!(
                    key.as_str(),
                    "secret"
                        | "api_key"
                        | "bearer_token"
                        | "codex_auth_json"
                        | "token"
                        | "access_token"
                        | "refresh_token"
                        | "password"
                        | "current_password"
                        | "new_password"
                        | "private_key"
                        | "client_secret"
                ) {
                    *value = json!("[REDACTED]");
                } else {
                    redact_request(value);
                }
            }
        }
        Value::Array(values) => values.iter_mut().for_each(redact_request),
        _ => {}
    }
}

async fn result(response: Response) -> Result<Value, crate::chat::Failure> {
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .map_err(crate::control::storage)?;
    let body: Value = serde_json::from_slice(&bytes).map_err(crate::control::storage)?;
    if status.is_success() {
        Ok(body)
    } else {
        Err((status, Json(body)))
    }
}

pub async fn read(s: &Arc<AppState>, target: &str) -> Result<Value, crate::chat::Failure> {
    let state = State(s.clone());
    let response = match target {
        "access" => crate::access_api::settings(State(
            s.access
                .get()
                .ok_or_else(|| {
                    crate::control::failure(
                        StatusCode::SERVICE_UNAVAILABLE,
                        "settings.access_unavailable",
                        "Access service is not initialized",
                    )
                })?
                .clone(),
        ))
        .await
        .into_response(),
        "system_update" => crate::system_api::update_status().await.into_response(),
        "control" => crate::control::get_settings(state).await.into_response(),
        "control_policy" | "host_credential" => crate::control_settings_api::read(state)
            .await
            .into_response(),
        "execution_guardrail" => crate::runs::execution_guard::settings::get(state).await.into_response(),
        "browser" => crate::browser::get_settings(state).await,
        "agent_version_policy" => {
            crate::agent_runtime::version_policy::get_version_policy(state, Query(HashMap::new()))
                .await
                .into_response()
        }
        "competition_advisor" => crate::runs::competition_api::advisor::get_config(state)
            .await
            .into_response(),
        "runner_listener" => crate::runner_remote::listener_status(state)
            .await
            .into_response(),
        _ => {
            return Err(crate::control::failure(
                StatusCode::BAD_REQUEST,
                "settings.target_unknown",
                format!("Unknown settings target: {target}"),
            ));
        }
    };
    result(response).await
}

pub async fn update(
    s: &Arc<AppState>,
    caller: &str,
    target: &str,
    values: Value,
) -> Result<Value, crate::chat::Failure> {
    let fields = values
        .as_object()
        .ok_or_else(|| {
            crate::control::failure(
                StatusCode::BAD_REQUEST,
                "settings.values_invalid",
                "values must be an object",
            )
        })?
        .keys()
        .cloned()
        .collect::<Vec<_>>();
    let state = State(s.clone());
    let response = match target {
        "access" => crate::access_api::update(
            State(
                s.access
                    .get()
                    .ok_or_else(|| {
                        crate::control::failure(
                            StatusCode::SERVICE_UNAVAILABLE,
                            "settings.access_unavailable",
                            "Access service is not initialized",
                        )
                    })?
                    .clone(),
            ),
            Json(values),
        )
        .await
        .into_response(),
        "system_update" => match values["action"].as_str() {
            Some("install") => crate::system_api::update_install(Some(Json(values))).await,
            Some("rollback") => crate::system_api::update_rollback().await,
            Some("check") => crate::system_api::update_check(Some(Json(values))).await,
            _ => {
                return Err(crate::control::failure(
                    StatusCode::BAD_REQUEST,
                    "settings.action_invalid",
                    "system_update action must be check, install or rollback",
                ));
            }
        },
        "thread_grant" => {
            let id = values["thread_id"]
                .as_str()
                .filter(|id| !id.trim().is_empty())
                .ok_or_else(|| {
                    crate::control::failure(
                        StatusCode::BAD_REQUEST,
                        "control.thread_id_required",
                        "thread_id is required",
                    )
                })?
                .to_owned();
            crate::control::save_thread_grant(s.clone(), id, values, caller)
                .await
                .into_response()
        }
        "control" => crate::control::save_settings(s.clone(), values, caller)
            .await
            .into_response(),
        "control_policy" => crate::control_settings_api::save(s.clone(), values, caller)
            .await
            .into_response(),
        "host_credential" => crate::control_settings_api::credential(state, Json(values))
            .await
            .into_response(),
        "execution_guardrail" => crate::runs::execution_guard::settings::put(state, Json(values)).await.into_response(),
        "browser" => crate::browser::put_settings(state, Json(values)).await,
        "agent_version_policy" => {
            crate::agent_runtime::version_policy::put_version_policy(state, Json(values))
                .await
                .into_response()
        }
        "competition_advisor" => {
            crate::runs::competition_api::advisor::put_config(state, Json(values))
                .await
                .into_response()
        }
        "runner_listener" => {
            let config = serde_json::from_value(values).map_err(|e| {
                crate::control::failure(
                    StatusCode::BAD_REQUEST,
                    "settings.values_invalid",
                    e.to_string(),
                )
            })?;
            crate::runner_remote::listener_update(state, Json(config))
                .await
                .into_response()
        }
        _ => {
            return Err(crate::control::failure(
                StatusCode::BAD_REQUEST,
                "settings.target_unknown",
                format!("Unknown settings target: {target}"),
            ));
        }
    };
    let saved = result(response).await?;
    if !matches!(target, "control" | "control_policy" | "thread_grant") {
        crate::control::audit(
            s,
            caller,
            caller,
            "settings_changed",
            json!({"target":target,"fields":fields}),
        );
    }
    Ok(saved)
}
