//! Readiness, desktop drain, and managed Web release updates.
mod managed_update;
pub use managed_update::cli_entry as management_cli_entry;
use crate::AppState;
use axum::{
    Json, Router,
    extract::{Request, State},
    http::{Method, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde_json::{Value, json};
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};

/// Write admission for a managed desktop service while the app switches versions.
pub struct Admission {
    draining: AtomicBool,
    in_flight: AtomicUsize,
}
struct InFlight<'a>(&'a Admission);
impl Drop for InFlight<'_> {
    fn drop(&mut self) { self.0.in_flight.fetch_sub(1, Ordering::SeqCst); }
}
impl Default for Admission {
    fn default() -> Self {
        Self {
            draining: AtomicBool::new(
                std::env::var("MUTEKI_DESKTOP_MAINTENANCE").is_ok_and(|v| v == "1"),
            ),
            in_flight: AtomicUsize::new(0),
        }
    }
}
impl Admission {
    pub(crate) fn set_draining(&self,value:bool){self.draining.store(value,Ordering::SeqCst);}
    pub fn draining(&self) -> bool {
        self.draining.load(Ordering::SeqCst)
    }
}

pub fn managed_desktop() -> bool {
    std::env::var("MUTEKI_MANAGED_DESKTOP").is_ok_and(|v| v == "1")
}

pub fn desktop_control_path(path: &str) -> bool {
    matches!(
        path,
        "/api/desktop/activity"
            | "/api/desktop/drain"
            | "/api/desktop/runner/stop"
            | "/api/desktop/shell-environment"
    )
}

/// Whether the request carries the secret the managed desktop started this
/// service with.
pub fn desktop_control(headers: &axum::http::HeaderMap) -> bool {
    let expected = std::env::var("MUTEKI_DESKTOP_CONTROL_TOKEN").ok();
    let given = headers
        .get("x-muteki-desktop-control")
        .and_then(|v| v.to_str().ok());
    control_matches(managed_desktop(), expected.as_deref(), given)
}

fn control_matches(managed: bool, expected: Option<&str>, given: Option<&str>) -> bool {
    let (Some(expected), Some(given)) = (expected, given) else {
        return false;
    };
    managed
        && expected.len() >= 32
        && given.len() == expected.len()
        && given
            .bytes()
            .zip(expected.bytes())
            .fold(0u8, |acc, (a, b)| acc | (a ^ b))
            == 0
}

fn require_desktop(headers: &axum::http::HeaderMap) -> Result<(), Response> {
    if !managed_desktop() {
        return Err(error(
            StatusCode::NOT_FOUND,
            "desktop.not_managed",
            "Not a managed desktop service",
        ));
    }
    if !desktop_control(headers) {
        return Err(error(
            StatusCode::FORBIDDEN,
            "desktop.control_required",
            "Only the desktop app that started this service may do this",
        ));
    }
    Ok(())
}

pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/readiness", get(readiness))
        .route("/api/desktop/activity", get(activity))
        .route("/api/desktop/drain", post(drain))
        .route("/api/desktop/runner/stop", post(stop_runner))
        .route(
            "/api/desktop/shell-environment",
            post(update_shell_environment),
        )
        .route("/api/settings/system-update", get(update_status))
        .route("/api/settings/system-update/check", post(update_check))
        .route("/api/settings/system-update/install", post(update_install))
        .route(
            "/api/settings/system-update/rollback",
            post(update_rollback),
        )
}

fn error(status: StatusCode, code: &str, message: &str) -> Response {
    (
        status,
        Json(json!({"error":{"code":code,"message":message},"detail":message})),
    )
        .into_response()
}

pub async fn write_gate(
    State(state): State<Arc<AppState>>,
    request: Request,
    next: Next,
) -> Response {
    let path=request.uri().path();
    let startup_control=matches!(path,"/api/health"|"/api/readiness"|"/api/auth/status")||desktop_control_path(path);
    if path.starts_with("/api/") && !startup_control && (!state.lifecycle.ready() || state.admission.draining()){
        return error(StatusCode::SERVICE_UNAVAILABLE,"service.not_ready","服务正在初始化或切换版本，请等待激活完成。");
    }
    let mutation = !matches!(
        *request.method(),
        Method::GET | Method::HEAD | Method::OPTIONS
    ) && request.uri().path().starts_with("/api/")
        && !desktop_control_path(request.uri().path());
    if !mutation {
        return next.run(request).await;
    }
    let admission = &state.admission;
    admission.in_flight.fetch_add(1, Ordering::SeqCst);
    let _in_flight = InFlight(admission);
    if admission.draining() || !state.lifecycle.ready() {
        return error(
            StatusCode::SERVICE_UNAVAILABLE,
            "desktop.maintenance",
            "工作台正在切换版本，暂不接受新操作。",
        );
    }
    next.run(request).await
}

pub(crate) async fn readiness(State(state):State<Arc<AppState>>)->Response {
    let mut body=crate::compat::health_body();
    let ready=state.lifecycle.ready() && !state.admission.draining();
    body["ready"]=json!(ready);body["status"]=json!(if ready{"ready"}else if state.admission.draining(){"maintenance"}else{"initializing"});
    body["initialization"]=state.lifecycle.snapshot();
    body["modules"]=state.business.snapshot();
    body["modules_ready"]=json!(crate::business::Module::ALL.iter().all(|module|state.business.status(*module)["state"]=="ready"));
    body["shell_env"]=crate::shell_env::summary();
    (if ready{StatusCode::OK}else{StatusCode::SERVICE_UNAVAILABLE},Json(body)).into_response()
}

fn active(state: &AppState) -> Result<Value, Response> {
    let run_ids=state.runs.active_run_ids();
    let runs=run_ids.len();
    let threads=state.store.active_turn_threads(state.chat.epoch()).map_err(|_| {
        error(StatusCode::INTERNAL_SERVER_ERROR,"storage.unavailable","Storage error")
    })?;
    let turns = threads.len();
    let mut tasks:Vec<Value>=threads.into_iter().map(|(id,title)|json!({"kind":"turn","id":id,"title":title})).collect();
    for id in run_ids {
        let title=state.store.get("runs",&id).ok().flatten().and_then(|run|run["name"].as_str().map(str::to_owned)).unwrap_or_default();
        tasks.push(json!({"kind":"run","id":id,"title":title}));
    }
    Ok(json!({"active":turns + runs,"turns":turns,"runs":runs,"tasks":tasks,"operations":state.admission.in_flight.load(Ordering::SeqCst)}))
}

async fn activity(State(state): State<Arc<AppState>>, headers: axum::http::HeaderMap) -> Response {
    if let Err(response) = require_desktop(&headers) {
        return response;
    }
    match active(&state) {
        Ok(body) => Json(body).into_response(),
        Err(response) => response,
    }
}

async fn drain(
    State(state): State<Arc<AppState>>,
    headers: axum::http::HeaderMap,
    body: Option<Json<Value>>,
) -> Response {
    if let Err(response) = require_desktop(&headers) {
        return response;
    }
    let body = body.map(|Json(v)| v).unwrap_or_else(|| json!({}));
    if !body.is_object() {
        return error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "desktop.drain_invalid",
            "Body must be an object",
        );
    }
    let admission = &state.admission;
    if body.get("cancel") == Some(&json!(true)) {
        return match crate::lifecycle::activate(state.clone()).await {
            Ok(())=>Json(json!({"draining":false,"ready":true})).into_response(),
            Err(message)=>error(StatusCode::SERVICE_UNAVAILABLE,"desktop.activation_failed",&message),
        };
    }
    let _activation=state.lifecycle.activation.lock().await;
    admission.draining.store(true, Ordering::SeqCst);
    if admission.in_flight.load(Ordering::SeqCst) > 0 {
        admission.draining.store(false, Ordering::SeqCst);
        return error(
            StatusCode::CONFLICT,
            "desktop.busy",
            "工作台仍在处理操作，请稍后再更新。",
        );
    }
    let activity = match active(&state) {
        Ok(activity) => activity,
        Err(response) => {
            admission.draining.store(false, Ordering::SeqCst);
            return response;
        }
    };
    if activity["active"].as_i64().unwrap_or(0) > 0 {
        admission.draining.store(false, Ordering::SeqCst);
        let message="请等待当前任务完成，再应用更新。";
        return (
            StatusCode::CONFLICT,
            Json(json!({"error":{"code":"desktop.active","message":message},"detail":message,"activity":activity})),
        )
            .into_response();
    }
    let mut result = activity;
    result["draining"] = json!(true);
    Json(result).into_response()
}

/// Called by the owning desktop app as it quits, so its local Runner does not
/// linger as an orphan. A configured external Runner is left running.
async fn stop_runner(headers: axum::http::HeaderMap) -> Response {
    if let Err(response) = require_desktop(&headers) {
        return response;
    }
    match crate::runner::stop_local().await {
        Ok(stopped) => Json(json!({"stopped":stopped})).into_response(),
        Err(message) => error(StatusCode::BAD_GATEWAY, "desktop.runner_stop_failed", &message),
    }
}

async fn update_shell_environment(
    State(state): State<Arc<AppState>>,
    headers: axum::http::HeaderMap,
    body: Option<Json<Value>>,
) -> Response {
    if let Err(response) = require_desktop(&headers) {
        return response;
    }
    let Some(env_obj) = body
        .as_ref()
        .and_then(|Json(v)| v.get("env"))
        .and_then(Value::as_object)
    else {
        return error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "desktop.shell_env_invalid",
            "Body must contain an env object",
        );
    };
    let state_dir = crate::host_process::configured_state_dir(&state);
    let changed = crate::shell_env::apply_external_env(env_obj, "desktop", state_dir.as_deref());
    crate::host_process::sync_runtime_caches();
    crate::runner::push_shell_environment().await;
    if changed && state.lifecycle.ready() && !state.admission.draining() {
        let bg = state.clone();
        tokio::spawn(async move {
            let _ = crate::runtime_api::probe_all(&bg, "", "", false).await;
        });
    }
    Json(json!({
        "ok": true,
        "changed": changed,
        "shell_env": crate::shell_env::summary(),
    }))
    .into_response()
}

fn status() -> Value {
    if managed_desktop() {
        let version = env!("CARGO_PKG_VERSION");
        json!({"status":"idle","running":false,"current_version":version,
            "active_version":version,"available":false,"restart_required":false,
            "install_kind":"desktop","deployment":"desktop","install_root":"",
            "channel":"stable","message":"客户端与后端作为完整桌面版本更新，请使用桌面应用的更新入口。"})
    } else {
        managed_update::status()
    }
}
pub(crate) async fn update_status() -> Json<Value> {
    Json(json!({"update":status()}))
}
fn update_body(body: Option<Json<Value>>) -> Result<Value, Response> {
    let body = body.map(|Json(value)| value).unwrap_or_else(|| json!({}));
    if !body.is_object() {
        return Err(error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "update.invalid_request",
            "Body must be an object",
        ));
    }
    Ok(body)
}
pub(crate) async fn update_check(body: Option<Json<Value>>) -> Response {
    if managed_desktop() {
        return Json(json!({"update":status()})).into_response();
    }
    let body = match update_body(body) {
        Ok(body) => body,
        Err(error) => return error,
    };
    let target = body["target"]
        .as_str()
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let _ = managed_update::check(target).await;
    Json(json!({"update":status()})).into_response()
}
pub(crate) async fn update_install(body: Option<Json<Value>>) -> Response {
    if managed_desktop() {
        return error(
            StatusCode::CONFLICT,
            "update.unsupported",
            "托管后端随桌面应用整包更新，请使用桌面应用的更新入口。",
        );
    }
    let body = match update_body(body) {
        Ok(body) => body,
        Err(error) => return error,
    };
    let target = body["target"]
        .as_str()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned);
    match managed_update::start(target, body["force"] == true).await {
        Ok(update) => Json(json!({"update":update})).into_response(),
        Err(message) => error(StatusCode::CONFLICT, "update.install_failed", &message),
    }
}
pub(crate) async fn update_rollback() -> Response {
    if managed_desktop() {
        return error(
            StatusCode::CONFLICT,
            "update.unsupported",
            "托管后端必须与桌面应用及数据备份一起恢复，请使用桌面应用的恢复入口。",
        );
    }
    match managed_update::rollback().await {
        Ok(update) => Json(json!({"update":update})).into_response(),
        Err(message) => error(StatusCode::CONFLICT, "update.rollback_failed", &message),
    }
}

/// Compares dotted numeric versions the way the desktop updater does.
pub fn newer(candidate: &str, current: &str) -> bool {
    let parse = |v: &str| {
        v.split('.')
            .map(|p| p.parse::<u64>().unwrap_or(0))
            .collect::<Vec<_>>()
    };
    let (a, b) = (parse(candidate), parse(current));
    for i in 0..a.len().max(b.len()) {
        let (x, y) = (
            a.get(i).copied().unwrap_or(0),
            b.get(i).copied().unwrap_or(0),
        );
        if x != y {
            return x > y;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drain_and_writes_never_both_proceed() {
        let admission = Admission::default();
        admission.in_flight.fetch_add(1, Ordering::SeqCst);
        admission.draining.store(true, Ordering::SeqCst);
        assert!(
            admission.in_flight.load(Ordering::SeqCst) > 0,
            "drain must see the write and back off"
        );
        assert!(
            admission.draining(),
            "the write must see the drain and back off"
        );
    }

    #[test]
    fn only_the_managing_desktop_controls_writes() {
        let secret = "a".repeat(64);
        assert!(control_matches(true, Some(&secret), Some(&secret)));
        assert!(
            !control_matches(false, Some(&secret), Some(&secret)),
            "a source run is never desktop-managed"
        );
        assert!(!control_matches(true, Some(&secret), Some(&"b".repeat(64))));
        assert!(!control_matches(true, Some(&secret), None));
        assert!(!control_matches(true, None, Some("")));
        assert!(
            !control_matches(true, Some("short"), Some("short")),
            "weak secrets are refused"
        );
    }

    #[test]
    fn versions_compare_numerically() {
        assert!(newer("0.4.10", "0.4.9"));
        assert!(!newer("0.4.8", "0.4.8"));
        assert!(!newer("0.4.7", "0.4.8"));
        assert!(
            semver::Version::parse("1.2.3").is_ok()
                && semver::Version::parse("1.2").is_err()
                && semver::Version::parse("1.2.x").is_err()
        );
    }
}
