//! Remote node control plane. Administrative routes use the normal Web access gate.
pub mod broker;
pub mod client;
pub mod commands;
pub mod credentials;
pub mod files;
pub mod identity;
pub mod listener;
pub mod tls;
pub mod transfers;
pub mod transport;
mod workspace;
pub mod controller;

use crate::AppState;
use axum::{
    Json, Router,
    extract::{Path, State},
    http::{HeaderMap, HeaderValue, StatusCode},
    routing::{delete, get, post},
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;

type Reply = Result<(HeaderMap, Json<Value>), (StatusCode, Json<Value>)>;
fn reply(value: Value) -> Reply {
    let mut headers = HeaderMap::new();
    headers.insert("cache-control", HeaderValue::from_static("no-store"));
    Ok((headers, Json(value)))
}
fn failure(error: String) -> (StatusCode, Json<Value>) {
    (StatusCode::CONFLICT, Json(json!({"error":error})))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct EnrollmentRequest {
    name: String,
    projects: Vec<String>,
    ttl_seconds: i64,
}
pub(crate) async fn issue(
    State(state): State<Arc<AppState>>,
    Json(request): Json<EnrollmentRequest>,
) -> Reply {
    let now = chrono::Utc::now().timestamp();
    let token = identity::issue_enrollment(
        &state.store,
        request.name,
        request.projects,
        request.ttl_seconds,
        now,
    )
    .map_err(failure)?;
    let status = state.runner_listener.status();
    let commands = (status["running"] == true && status["public_url"].as_str().is_some_and(|v| !v.is_empty())
        && status["ca_fingerprint"].as_str().is_some_and(|v| !v.is_empty())).then(|| {
        let server = status["public_url"].as_str().unwrap_or_default();
        let server = server.replace('\'', "'\\''");
        let pin = status["ca_fingerprint"].as_str().unwrap_or_default();
        json!({
            "enroll": format!("printf '%s' '{token}' | muteki-server runner enroll --state-dir \"$HOME/.muteki-runner\" --server '{server}' --ca-sha256 {pin} --token-file /dev/stdin"),
            "start": "muteki-server runner --remote --state-dir \"$HOME/.muteki-runner\"",
        })
    });
    reply(json!({"token":token,"expires_at":now+request.ttl_seconds,"single_use":true,"commands":commands}))
}
pub(crate) async fn nodes(State(state): State<Arc<AppState>>) -> Reply {
    let mut nodes = identity::list(&state.store).map_err(failure)?;
    for node in &mut nodes {
        node["connected"] = json!(
            state
                .remote_runners
                .online(node["id"].as_str().unwrap_or(""))
        );
    }
    reply(json!({"nodes":nodes}))
}
async fn dispatch(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(command): Json<commands::Command>,
) -> Reply {
    if command.node_id != id {
        return Err(failure("Node identity mismatch".into()));
    }
    reply(
        state
            .remote_runners
            .request(&state, command)
            .await
            .map_err(failure)?,
    )
}
pub(crate) async fn rotate(State(state): State<Arc<AppState>>, Path(id): Path<String>) -> Reply {
    let credential = identity::rotate(&state.store, &id, chrono::Utc::now().timestamp()).map_err(failure)?;
    let document = serde_json::to_string(&json!({"credential":credential})).map_err(|e| failure(e.to_string()))?;
    reply(json!({
        "node_id": credential.node_id,
        "version": credential.version,
        "expires_at": credential.expires_at,
        "commands": {"install": format!("printf '%s' '{document}' | muteki-server runner install-credential --state-dir \"$HOME/.muteki-runner\" --credential-file /dev/stdin")},
    }))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct StateRequest {
    state: String,
}
pub(crate) async fn set_state(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(request): Json<StateRequest>,
) -> Reply {
    identity::set_state(
        &state.store,
        &id,
        &request.state,
        chrono::Utc::now().timestamp(),
    )
    .map_err(failure)?;
    reply(json!({"node_id":id,"state":request.state,"execution_stop_confirmed":false}))
}
pub(crate) async fn remove(State(state): State<Arc<AppState>>, Path(id): Path<String>) -> Reply {
    if state.remote_runners.online(&id) {
        return Err(failure("Node is still connected; revoke it and wait for the session to close".into()));
    }
    let active = state
        .store
        .list("runner_dispatches")
        .map_err(|e| failure(e.to_string()))?
        .into_iter()
        .any(|row| row["node_id"] == id && row["status"] != "released");
    if active {
        return Err(failure("Node still holds Worker admissions; stop its Runs first".into()));
    }
    identity::delete(&state.store, &id, chrono::Utc::now().timestamp()).map_err(failure)?;
    reply(json!({"node_id":id,"removed":true}))
}
async fn audit(State(state): State<Arc<AppState>>) -> Reply {
    let names: std::collections::HashMap<String, String> = identity::list(&state.store)
        .map_err(failure)?
        .into_iter()
        .filter_map(|n| Some((n["id"].as_str()?.to_owned(), n["name"].as_str()?.to_owned())))
        .collect();
    let mut rows = identity::audit_log(&state.store).map_err(failure)?;
    for row in &mut rows {
        if let Some(name) = row["node_id"].as_str().and_then(|id| names.get(id)) {
            row["node_name"] = json!(name);
        }
    }
    reply(json!({"events":rows}))
}
pub(crate) async fn listener_status(State(state): State<Arc<AppState>>) -> Reply {
    let config = listener::load(&state).map_err(failure)?;
    reply(json!({"config":config,"status":state.runner_listener.status()}))
}
pub(crate) async fn listener_update(
    State(state): State<Arc<AppState>>,
    Json(config): Json<listener::Config>,
) -> Reply {
    listener::validate(&config).map_err(|e| (StatusCode::UNPROCESSABLE_ENTITY, Json(json!({"error":e}))))?;
    let config = listener::Config {
        enabled: config.enabled,
        listen: config.listen.trim().into(),
        public_url: config.public_url.trim().trim_end_matches('/').into(),
    };
    listener::save(&state, &config).map_err(failure)?;
    match state.runner_listener.apply(state.clone(), &config).await {
        Ok(status) => reply(json!({"config":config,"status":status})),
        Err(error) => Err((StatusCode::CONFLICT, Json(json!({"error":error,"config":config,"status":state.runner_listener.status()})))),
    }
}
async fn authority(State(state): State<Arc<AppState>>) -> Result<(HeaderMap, String), (StatusCode, Json<Value>)> {
    let pem = tls::ca_pem(&crate::native_chat::state_dir(&state))
        .map_err(failure)?
        .ok_or_else(|| (StatusCode::NOT_FOUND, Json(json!({"error":"Runner CA has not been created; enable the listener first"}))))?;
    let mut headers = HeaderMap::new();
    headers.insert("content-type", HeaderValue::from_static("application/x-pem-file"));
    headers.insert("content-disposition", HeaderValue::from_static("attachment; filename=\"muteki-runner-ca.pem\""));
    Ok((headers, pem))
}
pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/runner/enrollments", post(issue))
        .route("/api/runner/nodes", get(nodes))
        .route("/api/runner/nodes/{id}/commands", post(dispatch))
        .route("/api/runner/nodes/{id}/rotate", post(rotate))
        .route("/api/runner/nodes/{id}/state", post(set_state))
        .route("/api/runner/nodes/{id}", delete(remove))
        .route("/api/runner/audit", get(audit))
        .route("/api/runner/listener", get(listener_status).put(listener_update))
        .route("/api/runner/ca.pem", get(authority))
        .merge(files::admin_router())
        .merge(credentials::admin_router())
}
