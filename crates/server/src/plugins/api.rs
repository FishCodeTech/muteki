//! `/api/chat-plugins` and its `/api/agent-extensions` alias: management of
//! managed Agent extension packages for chat and Worker modes.

use super::{PluginError, Registry, VisualFailure, registry};
use crate::{AppState, agent_runtime::descriptors, chat};
use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, patch, post, put},
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{collections::HashMap, sync::Arc};

type Shared = Arc<AppState>;

pub fn router() -> Router<Shared> {
    let routes = || {
        Router::new()
            .route("/visualizations/{thread_id}", get(visualization))
            .route("/bundled/muteki-visualize", post(install_visualize))
            .route("/install", post(install))
            .route("/mcp", post(add_mcp))
            .route("/packages/{package_id}", patch(update).delete(uninstall))
            .route("/check/{package_id}", post(check))
            .route("/control", put(control))
    };
    let mut router = Router::new();
    for prefix in ["/api/chat-plugins", "/api/agent-extensions"] {
        router = router.route(prefix, get(listing)).nest(prefix, routes());
    }
    router
}

fn detail(status: StatusCode, detail: Value) -> Response {
    (status, Json(json!({"detail": detail}))).into_response()
}

fn coded(error: &PluginError) -> Response {
    detail(StatusCode::BAD_REQUEST, json!({"code": error.code, "message": error.message}))
}

fn unavailable(error: &PluginError) -> Response {
    detail(StatusCode::SERVICE_UNAVAILABLE, json!({"code": error.code, "message": error.message}))
}

async fn open(s: &Shared) -> Result<Arc<Registry>, Response> {
    let state = s.clone();
    tokio::task::spawn_blocking(move || registry(&state))
        .await
        .map_err(|_| unavailable(&PluginError::new("chat_plugin.registry_unavailable", "扩展注册表不可用")))?
        .map_err(|error| unavailable(&error))
}

async fn blocking<T: Send + 'static>(work: impl FnOnce() -> T + Send + 'static) -> Result<T, Response> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|_| unavailable(&PluginError::new("chat_plugin.registry_unavailable", "扩展注册表不可用")))
}

/// After any change: drop cached tool catalogs and stopped packages' MCP
/// connections. Idle sessions pick up the new revision at their next turn.
async fn changed(s: &Shared, registry: &Registry) {
    registry.invalidate().await;
    crate::native_chat::refresh_all_capability_catalogs(s).await;
}

fn valid_run_id(run_id: &str) -> bool {
    run_id.len() <= 100 && run_id.starts_with("run-") && run_id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

async fn listing(State(s): State<Shared>, Query(query): Query<HashMap<String, String>>) -> Response {
    listing_with(&s, &query).await
}

pub(crate) async fn listing_with(s: &Shared, query: &HashMap<String, String>) -> Response {
    let engine = query.get("engine").map(String::as_str).unwrap_or("codex").to_owned();
    let mode = query.get("mode").map(String::as_str).unwrap_or("chat").to_owned();
    let run_id = query.get("run_id").cloned().unwrap_or_default();
    if descriptors::find(&engine).is_none() {
        return detail(StatusCode::BAD_REQUEST, json!("未知 Agent"));
    }
    if !super::MODES.contains(&mode.as_str()) {
        return detail(StatusCode::BAD_REQUEST, json!("未知使用场景"));
    }
    if !run_id.is_empty() && !valid_run_id(&run_id) {
        return detail(StatusCode::BAD_REQUEST, json!("Run ID 无效"));
    }
    let registry = match open(&s).await {
        Ok(registry) => registry,
        Err(response) => return response,
    };
    let scope = if mode == "chat" { String::new() } else { run_id };
    let capability_browser = crate::capabilities::enabled(&s, "skills", "agent-browser");
    let result = blocking(move || -> Result<Value, PluginError> {
        let native_skills: Vec<Value> = crate::composer::native_skill_rows(&engine);
        let descriptor = descriptors::find(&engine).map(|d| d.public().clone()).unwrap_or(Value::Null);
        let packages: Vec<Value> = registry.records()?.iter().map(|r| registry.public(r)).collect();
        Ok(json!({
            "engines": descriptors::engine_ids(), "modes": super::MODES, "engine": engine,
            "revision": registry.revision(&engine, "chat")?,
            "host_discovery_enabled": super::host_discovery_enabled(),
            "packages": packages, "native_skills": native_skills,
            "bundled_plugins": [{"id": "muteki-visualize", "name": "Muteki Visualize", "modes": ["chat"]}],
            "builtin_skills": [{"id": "agent-browser", "modes": ["pentest"], "enabled": capability_browser, "scope": "new_workers"}],
            "native_mcp": super::native_mcp(&engine),
            "transport": descriptor["identity"]["transport_label"],
            "control_enabled": registry.control_enabled(),
            "runtime_scope": scope,
            "runtime_mcp": registry.runtime_mcp_health(&engine, &mode, &scope)?,
        }))
    })
    .await;
    match result {
        Ok(Ok(catalog)) => Json(catalog).into_response(),
        Ok(Err(error)) => coded(&error),
        Err(response) => response,
    }
}

#[derive(Deserialize)]
struct VisualQuery {
    path: String,
    #[serde(default)]
    message_id: String,
}

async fn visualization(State(s): State<Shared>, Path(thread_id): Path<String>, Query(query): Query<VisualQuery>) -> Response {
    let not_found = || detail(StatusCode::NOT_FOUND, json!("对话不存在"));
    let doc = {
        let _guard = s.chat.active.lock().await;
        chat::load(&s, &thread_id)
    };
    let Ok(doc) = doc else { return not_found() };
    if doc["thread"]["mode"] != "conversation" {
        return not_found();
    }
    let registry = match open(&s).await {
        Ok(registry) => registry,
        Err(response) => return response,
    };
    let state = s.clone();
    let result = blocking(move || {
        let context = super::visual_context(&state, &thread_id, &doc);
        registry.visualization_document(
            &thread_id,
            &context.scope,
            &chat::current_messages(&doc),
            &context.engine,
            &context.workspace_root,
            &query.path,
            &query.message_id,
        )
    })
    .await;
    match result {
        Ok(Ok(document)) => Json(document).into_response(),
        Ok(Err(VisualFailure::Rejected(error))) => {
            let status = if error.code == "visualization.not_attached" { StatusCode::NOT_FOUND } else { StatusCode::UNPROCESSABLE_ENTITY };
            detail(status, json!({"code": error.code, "message": error.message}))
        }
        Ok(Err(VisualFailure::Store(error))) => {
            detail(StatusCode::INTERNAL_SERVER_ERROR, json!({"code": "visualization.store_failed", "message": error.to_string()}))
        }
        Err(response) => response,
    }
}

async fn install_visualize(State(s): State<Shared>) -> Response {
    install_visualize_with(&s).await
}

pub(crate) async fn install_visualize_with(s: &Shared) -> Response {
    let registry = match open(&s).await {
        Ok(registry) => registry,
        Err(response) => return response,
    };
    let worker = registry.clone();
    match blocking(move || worker.install_visualize()).await {
        Ok(Ok(result)) => {
            changed(&s, &registry).await;
            Json(result).into_response()
        }
        Ok(Err(error)) => coded(&error),
        Err(response) => response,
    }
}

#[derive(Deserialize)]
struct InstallBody {
    source: Value,
    #[serde(default)]
    modes: Option<Value>,
}

async fn install(State(s): State<Shared>, body: Result<Json<InstallBody>, axum::extract::rejection::JsonRejection>) -> Response {
    let Ok(Json(body)) = body else {
        return detail(StatusCode::UNPROCESSABLE_ENTITY, json!("请求体无效"));
    };
    install_with(&s, body.source, body.modes).await
}

pub(crate) async fn install_with(s: &Shared, source: Value, modes: Option<Value>) -> Response {
    if !source.is_object() {
        return detail(StatusCode::UNPROCESSABLE_ENTITY, json!("source 必须为对象"));
    }
    let registry = match open(s).await {
        Ok(registry) => registry,
        Err(response) => return response,
    };
    let worker = registry.clone();
    match blocking(move || worker.install(&source, modes.as_ref())).await {
        Ok(Ok(result)) => {
            changed(s, &registry).await;
            Json(result).into_response()
        }
        Ok(Err(error)) => coded(&error),
        Err(_) => coded(&PluginError::new("chat_plugin.install_failed", "插件导入失败，请稍后重试")),
    }
}

#[derive(Deserialize)]
struct McpBody {
    name: String,
    servers: Value,
    #[serde(default)]
    modes: Option<Value>,
}

async fn add_mcp(State(s): State<Shared>, body: Result<Json<McpBody>, axum::extract::rejection::JsonRejection>) -> Response {
    let Ok(Json(body)) = body else {
        return detail(StatusCode::UNPROCESSABLE_ENTITY, json!("请求体无效"));
    };
    add_mcp_with(&s, body.name, body.servers, body.modes).await
}

pub(crate) async fn add_mcp_with(s: &Shared, name: String, servers: Value, modes: Option<Value>) -> Response {
    let registry = match open(s).await {
        Ok(registry) => registry,
        Err(response) => return response,
    };
    let worker = registry.clone();
    match blocking(move || worker.add_mcp(&name, &servers, modes.as_ref())).await {
        Ok(Ok(result)) => {
            changed(s, &registry).await;
            Json(result).into_response()
        }
        Ok(Err(error)) => coded(&error),
        Err(_) => detail(StatusCode::BAD_REQUEST, json!("MCP 配置无效，请检查名称、command/URL 与参数")),
    }
}

#[derive(Deserialize, Default)]
struct UpdateBody {
    enabled: Option<bool>,
    #[serde(default)]
    rollback: bool,
    native_hooks: Option<bool>,
    #[serde(default)]
    snapshot_id: String,
    #[serde(default)]
    digest: String,
    #[serde(default)]
    modes: Option<Value>,
}

async fn update(State(s): State<Shared>, Path(package_id): Path<String>, body: Result<Json<UpdateBody>, axum::extract::rejection::JsonRejection>) -> Response {
    let Ok(Json(body)) = body else {
        return detail(StatusCode::UNPROCESSABLE_ENTITY, json!("请求体无效"));
    };
    update_with(&s, package_id, body.enabled, body.rollback, body.native_hooks,
        if body.snapshot_id.is_empty() { body.digest } else { body.snapshot_id }, body.modes).await
}

pub(crate) async fn update_with(
    s: &Shared,
    package_id: String,
    enabled: Option<bool>,
    rollback: bool,
    native_hooks: Option<bool>,
    snapshot_or_digest: String,
    modes: Option<Value>,
) -> Response {
    let registry = match open(s).await {
        Ok(registry) => registry,
        Err(response) => return response,
    };
    let worker = registry.clone();
    let result = blocking(move || {
        worker.update(&package_id, enabled, rollback, native_hooks, &snapshot_or_digest, modes.as_ref())
    })
    .await;
    match result {
        Ok(Ok(result)) => {
            changed(s, &registry).await;
            Json(result).into_response()
        }
        Ok(Err(error)) => detail(StatusCode::BAD_REQUEST, json!(error.message)),
        Err(response) => response,
    }
}

async fn uninstall(State(s): State<Shared>, Path(package_id): Path<String>) -> Response {
    uninstall_with(&s, package_id).await
}

pub(crate) async fn uninstall_with(s: &Shared, package_id: String) -> Response {
    let registry = match open(s).await {
        Ok(registry) => registry,
        Err(response) => return response,
    };
    let worker = registry.clone();
    match blocking(move || worker.uninstall(&package_id)).await {
        Ok(Ok(())) => {
            changed(s, &registry).await;
            Json(json!({"removed": true})).into_response()
        }
        Ok(Err(error)) => unavailable(&error),
        Err(response) => response,
    }
}

async fn check(State(s): State<Shared>, Path(package_id): Path<String>) -> Response {
    check_with(&s, package_id).await
}

pub(crate) async fn check_with(s: &Shared, package_id: String) -> Response {
    let registry = match open(s).await {
        Ok(registry) => registry,
        Err(response) => return response,
    };
    let worker = registry.clone();
    let record = match blocking(move || worker.get(&package_id)).await {
        Ok(Ok(Some(record))) => record,
        Ok(Ok(None)) => return detail(StatusCode::NOT_FOUND, json!("扩展不存在")),
        Ok(Err(error)) => return unavailable(&error),
        Err(response) => return response,
    };
    Json(registry.check(&record).await).into_response()
}

async fn control(State(s): State<Shared>, body: Result<Json<Value>, axum::extract::rejection::JsonRejection>) -> Response {
    let Some(enabled) = body.ok().and_then(|Json(v)| v["enabled"].as_bool()) else {
        return detail(StatusCode::BAD_REQUEST, json!("启用状态无效"));
    };
    control_with(&s, enabled).await
}

pub(crate) async fn control_with(s: &Shared, enabled: bool) -> Response {
    let registry = match open(s).await {
        Ok(registry) => registry,
        Err(response) => return response,
    };
    let worker = registry.clone();
    match blocking(move || worker.set_control(enabled)).await {
        Ok(Ok(())) => {
            changed(s, &registry).await;
            Json(json!({"enabled": enabled})).into_response()
        }
        Ok(Err(error)) => unavailable(&error),
        Err(response) => response,
    }
}
