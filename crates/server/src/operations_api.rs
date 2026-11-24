//! Operations overview, maintenance and diagnostics, plus the domain-module
//! registry and the generic command/query entries for migrated domains.
//!
//! Every metric comes from this service's own store.
use crate::{AppState, runs::competition_api};
mod agent_dispatch;
mod command_journal;
mod capability_tools;
mod settings_dispatch;
mod workbench_dispatch;
pub(crate) async fn invoke_bound_tool(state: &Arc<AppState>, token: &str, binding: &Value, name: &str, arguments: &Value) -> Result<Value,Failure> {
    capability_tools::invoke(state,token,binding,name,arguments).await
}
pub(crate) mod domain_events;
mod platform_auth;
mod platform_effects;
mod platform_events;
mod run_queries;
mod src_queries;
pub(crate) use src_queries::run_list_projection;
pub(crate) fn src_tool_query(
    state: &Arc<AppState>, token: &str, owner: &str, kind: &str, args: &Value,
) -> Result<Value, Value> {
    src_queries::tool_query(state, token, owner, kind, args)
}
pub(crate) async fn src_tool_command(
    state: &Arc<AppState>, token: &str, owner: &str, kind: &str, args: &Value,
) -> Result<Value, Value> {
    src_queries::tool_command(state, token, owner, kind, args).await
}
mod task_dispatch;
mod run_commands;
mod outbox_view;
use axum::{
    Extension, Json, Router,
    extract::{Path, Query, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use hmac::{Hmac, Mac};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    path::{Path as FsPath, PathBuf},
    sync::{Arc, OnceLock},
    time::{Duration, Instant, SystemTime},
};

const COMMANDS: &str = "platform_commands";
const MODULE_FLAGS: &str = "domain_modules";
const COMMAND_CLAIMS: &str = "platform_command_claims";
const IDEMPOTENCY_KEYS: &str = "platform_idempotency_keys";
const AGGREGATE_VERSIONS: &str = "platform_aggregate_versions";
const TEMP_AGE: Duration = Duration::from_secs(3600);
static STARTED: OnceLock<Instant> = OnceLock::new();
static COMMAND_DISPATCH_LOCK: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
pub(crate) fn command_dispatch_lock() -> &'static tokio::sync::Mutex<()> {
    COMMAND_DISPATCH_LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
}
pub(crate) async fn dispatch_local_competition_command(state: Arc<AppState>, envelope: Value) -> Response {
    dispatch_local_command(state,envelope).await
}
pub(crate) async fn dispatch_local_command(state: Arc<AppState>, envelope: Value) -> Response {
    match command(State(state),HeaderMap::new(),None,Some(Json(envelope))).await {
        Ok(response) => response,
        Err(error) => error.into_response(),
    }
}
pub(crate) fn task_event_cursor(state: &AppState, task_id: &str, seq: u64, scope: &str) -> Result<String, String> {
    platform_events::signed(state, "task", task_id, seq, scope)
        .map_err(|error| error.1.0.to_string())
}
pub(crate) fn domain_event_cursor(state: &AppState, aggregate_type: &str,
    aggregate_id: &str, seq: u64, scope: &str) -> Result<String,String> {
    platform_events::signed_domain(state,aggregate_type,aggregate_id,seq,scope).map_err(|error| error.1.0.to_string())
}
pub(crate) fn competition_event_cursor(state: &AppState, aggregate_type: &str,
    aggregate_id: &str, seq: u64, scope: &str) -> Result<String, String> {
    platform_events::signed(state, aggregate_type, aggregate_id, seq, scope)
        .map_err(|error| error.1.0.to_string())
}

pub fn spawn_effect_loop(state:Arc<AppState>){spawn_effect_loop_for(state,None);}
pub(crate) fn spawn_effect_loop_for(state:Arc<AppState>,module:Option<crate::business::Module>){
    let saved=state.clone();
    let work=async move {
        let mut interval=tokio::time::interval(Duration::from_secs(5));
        loop {
            interval.tick().await;
            if state.admission.draining()||module.is_some_and(|module|crate::business::require(&state,module).is_err()){continue;}
            if let Err(error)=platform_effects::drain_once(&state,module).await{
                tracing::warn!(module=?module,code=%error.1.0["error"]["code"],"Effect outbox consumer failed");
            }
        }
    };
    if let Some(module)=module{crate::business::spawn(saved,module,"effects",work);}else{tokio::spawn(work);}
}

pub fn router() -> Router<Arc<AppState>> {
    STARTED.get_or_init(Instant::now);
    Router::new()
        .route("/api/operations/overview", get(overview))
        .route("/api/operations/outbox", get(outbox))
        .route("/api/operations/receipts/{id}", get(receipt_trace))
        .route("/api/operations/recovery-preview", get(recovery_preview))
        .route(
            "/api/operations/maintenance-preview",
            get(maintenance_preview),
        )
        .route("/api/operations/maintenance", post(maintenance))
        .route("/api/operations/diagnostic-bundle", get(diagnostic_bundle))
        .route("/api/domain-modules", get(modules))
        .route("/api/domain-modules/{id}", get(module))
        .route("/api/domain-modules/{id}/{action}", post(module_action))
        .route("/api/commands", post(command))
        .route("/api/queries", post(query))
        .merge(platform_events::router())
        .route("/api/receipts/{id}/effects", get(receipt_effects))
        .route("/api/effects", get(effects))
        .route("/api/effects/{id}", get(effect))
        .route("/api/effects/{id}/retry", post(retry_effect))
        .route("/api/effects/{id}/cancel", post(cancel_effect))
}

type Failure = (StatusCode, Json<Value>);
pub(crate) fn fail(status: StatusCode, code: &str, message: impl Into<String>) -> Failure {
    let category = match status {
        StatusCode::BAD_REQUEST | StatusCode::UNPROCESSABLE_ENTITY => "validation",
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => "permission",
        StatusCode::NOT_FOUND => "not_found",
        StatusCode::CONFLICT => "conflict",
        StatusCode::TOO_MANY_REQUESTS => "rate_limit",
        StatusCode::REQUEST_TIMEOUT | StatusCode::GATEWAY_TIMEOUT => "timeout",
        StatusCode::BAD_GATEWAY => "runtime",
        StatusCode::SERVICE_UNAVAILABLE => "state",
        _ => "internal",
    };
    (
        status,
        Json(json!({"error":{"schema_version":1,"code":code,"message":message.into(),"category":category,
            "recovery_hint":"","correlation_id":null,"retryable":matches!(status,
                StatusCode::TOO_MANY_REQUESTS | StatusCode::REQUEST_TIMEOUT | StatusCode::GATEWAY_TIMEOUT),"detail":{}}})),
    )
}
fn storage(_: impl std::fmt::Display) -> Failure {
    fail(
        StatusCode::INTERNAL_SERVER_ERROR,
        "storage.unavailable",
        "Storage error",
    )
}
fn require_subagent_caller(state: &AppState, headers: &HeaderMap, principal: &str, parent_id: &str) -> Result<(), Failure> {
    if !principal.starts_with("binding:") { return Ok(()); }
    let token = headers.get(header::AUTHORIZATION).and_then(|value|value.to_str().ok())
        .and_then(|value|value.strip_prefix("Bearer ")).unwrap_or("").trim();
    if state.capability_gateway.owner_for_token(token).as_deref() != Some(parent_id) {
        return Err(fail(StatusCode::FORBIDDEN,"conversation.subagent.caller_mismatch",
            "A subagent command or query must target its calling Thread"));
    }
    Ok(())
}
fn now() -> String {
    chrono::Utc::now().to_rfc3339()
}

/// Records what startup recovery did, for the recovery preview.
pub fn record_startup(state: &AppState, started_at: &str, settled: &Result<usize, String>) {
    let step = match settled {
        Ok(count) => {
            json!({"name":"engine_sessions","state":"completed","core":true,"detail":format!("Settled {count} engine sessions left open by an earlier run"),"impact":"","evidence":{"settled":count}})
        }
        Err(error) => {
            json!({"name":"engine_sessions","state":"failed","core":true,"detail":error,"impact":"Earlier engine sessions may still look live","evidence":{}})
        }
    };
    let ready = settled.is_ok();
    let record = json!({"started_at":started_at,"completed_at":now(),"ready":ready,"state":if ready {"ready"} else {"degraded"},"steps":[step]});
    if let Err(error) = state.store.put("system", "startup_recovery", &record) {
        tracing::error!(%error, "Could not record startup recovery");
    }
}

fn state_root(state: &AppState) -> Result<PathBuf, Failure> {
    state
        .store
        .get("system", "state_dir")
        .map_err(storage)?
        .and_then(|v| v.as_str().map(PathBuf::from))
        .ok_or_else(|| {
            fail(
                StatusCode::INTERNAL_SERVER_ERROR,
                "operations.state_root_unknown",
                "The service state directory is not recorded",
            )
        })
}

fn seconds_between(start: &Value, end: &Value) -> Option<f64> {
    let parse = |v: &Value| {
        v.as_str()
            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
    };
    Some((parse(end)? - parse(start)?).num_milliseconds() as f64 / 1000.0)
}
fn subtract_counts(total: &Value, excluded: &Value) -> Value {
    let mut result = serde_json::Map::new();
    for (key,value) in total.as_object().into_iter().flatten() {
        let count = value.as_i64().unwrap_or(0) - excluded[key].as_i64().unwrap_or(0);
        if count > 0 { result.insert(key.clone(),json!(count)); }
    }
    Value::Object(result)
}

fn overview_body(state: &AppState) -> Result<Value, Failure> {
    let root = state_root(state)?;
    let port = state
        .store
        .get("system", "port")
        .map_err(storage)?
        .and_then(|v| v.as_u64())
        .unwrap_or(8100);
    let metrics = state
        .store
        .operations_metrics(state.chat.epoch())
        .map_err(storage)?;
    let competition = competition_api::live_metrics(state).map_err(storage)?;
    let competition_receipts = outbox_view::competition_receipts(state)?;
    let effect_metrics = platform_effects::metrics(state)?;
    let outboxes = outbox_view::OutboxView::load(state)?;
    let platform_outbox = outboxes.summary("platform");
    let competition_outbox = outboxes.summary("competition");
    let src_outbox = outboxes.summary("src");
    let pending_total: i64 = ["accepted", "running", "waiting"]
        .iter()
        .filter_map(|s| metrics["receipts_by_state"][*s].as_i64())
        .sum();
    let pending = (pending_total-competition_receipts["pending"].as_i64().unwrap_or(0)).max(0);
    let platform_states = subtract_counts(&metrics["receipts_by_state"],&competition_receipts["by_state"]);
    let platform_pending_types = subtract_counts(&metrics["pending_receipts_by_type"],&competition_receipts["pending_by_type"]);
    let sessions = state
        .store
        .list(crate::agent_runtime::ledger::COLLECTION)
        .map_err(storage)?;
    let closed: Vec<f64> = sessions
        .iter()
        .filter(|s| s["status"] == "ended")
        .filter_map(|s| seconds_between(&s["started_at"], &s["ended_at"]))
        .collect();
    let instances = state
        .store
        .list(crate::runtime_api::INSTANCES)
        .map_err(storage)?
        .len();
    let plugins = crate::plugins::registry(state)
        .and_then(|r| r.records())
        .unwrap_or_default();
    let task_extensions = state.store.list("task_extensions").map_err(storage)?;
    let mut extension_health: Vec<Value> = task_extensions
        .iter()
        .map(|raw| {
            let item = raw.get("values").filter(|v| v.is_object()).unwrap_or(raw);
            let ext_id = item["extension_id"]
                .as_str()
                .or_else(|| item["id"].as_str())
                .unwrap_or("");
            let enabled = item["enabled"].as_bool().unwrap_or(false);
            let ext_state = item["state"]
                .as_str()
                .unwrap_or(if enabled { "ready" } else { "disabled" });
            json!({
                "extension_id": ext_id,
                "enabled": enabled,
                "state": ext_state,
                "health": item.get("health").cloned().unwrap_or(Value::Null),
                "last_error": item.get("last_error").cloned().unwrap_or(Value::Null),
            })
        })
        .collect();
    for p in &plugins {
        if let Some(pid) = p["id"].as_str() {
            if !extension_health.iter().any(|e| e["extension_id"] == pid) {
                extension_health.push(json!({
                    "extension_id": pid,
                    "enabled": p["enabled"],
                    "state": if p["enabled"] == false { "disabled" } else { "ready" },
                    "health": null,
                    "last_error": null,
                }));
            }
        }
    }
    let ext_installed = extension_health.len();
    let ext_enabled = extension_health.iter().filter(|e| e["enabled"] != false).count();
    let ext_degraded = extension_health
        .iter()
        .filter(|e| matches!(e["state"].as_str(), Some("degraded" | "unavailable")))
        .count();
    let ext_start_count: u64 = task_extensions
        .iter()
        .map(|raw| {
            let item = raw.get("values").filter(|v| v.is_object()).unwrap_or(raw);
            item["start_count"].as_u64().unwrap_or(0)
        })
        .sum();
    let frequent_extensions: Vec<String> = task_extensions
        .iter()
        .filter_map(|raw| {
            let item = raw.get("values").filter(|v| v.is_object()).unwrap_or(raw);
            let enabled = item["enabled"].as_bool().unwrap_or(false);
            let backoff = item["restart_backoff_seconds"].as_f64().unwrap_or(0.0);
            if enabled && backoff >= 8.0 {
                item["extension_id"]
                    .as_str()
                    .or_else(|| item["id"].as_str())
                    .map(str::to_owned)
            } else {
                None
            }
        })
        .collect();
    let recovery = state
        .store
        .get("system", "startup_recovery")
        .map_err(storage)?
        .unwrap_or(json!({"state":"unknown","ready":false,"steps":[]}));
    let ready = recovery["ready"] == true;
    let mut alerts = Vec::new();
    if pending > 0 {
        alerts.push(json!({"code":"platform.receipt.pending","severity":"warning","detail":format!("{pending} 个命令回执仍未完成"),"by_type":platform_pending_types}));
    }
    if competition_receipts["pending"].as_u64().unwrap_or(0) > 0 {
        alerts.push(json!({"code":"competition.receipt.pending","severity":"warning",
            "detail":format!("存在 {} 个未完成回执",competition_receipts["pending"])}));
    }
    for (domain,outbox) in [("platform",&platform_outbox),("competition",&competition_outbox),("src",&src_outbox)] {
        if outbox["oldest_pending_seconds"].as_f64().unwrap_or(0.0) >= 60.0 {
            alerts.push(json!({"code":format!("{domain}.outbox.delayed"),"severity":"warning",
                "detail":format!("最早未完成 outbox 已等待 {} 秒",outbox["oldest_pending_seconds"].as_f64().unwrap_or(0.0) as u64)}));
        }
    }
    if ext_degraded > 0 {
        alerts.push(json!({"code":"extension.health.degraded","severity":"warning",
            "detail":format!("{ext_degraded} 个扩展处于降级状态")}));
    }
    if !frequent_extensions.is_empty() {
        alerts.push(json!({"code":"extension.restart.frequent","severity":"warning",
            "detail":format!("频繁重启扩展：{}", frequent_extensions.join(", "))}));
    }
    let dead_letters = platform_outbox["by_state"]["dead_letter"].as_u64().unwrap_or(0)
        + competition_outbox["by_state"]["dead_letter"].as_u64().unwrap_or(0)
        + src_outbox["by_state"]["dead_letter"].as_u64().unwrap_or(0);
    if dead_letters > 0 {
        alerts.push(json!({"code":"outbox.dead_letter","severity":"critical",
            "detail":format!("存在 {dead_letters} 个 dead-letter 外部效果")}));
    }
    if effect_metrics["orphan_effects"].as_u64().unwrap_or(0) > 0 {
        alerts.push(json!({"code":"platform.effect.orphan","severity":"critical",
            "detail":format!("存在 {} 个孤立效果回执",effect_metrics["orphan_effects"])}));
    }
    if metrics["stream_gap_count"].as_i64().unwrap_or(0) > 0 {
        alerts.push(json!({"code":"platform.event.stream_gap","severity":"warning","detail":format!("{} 个会话事件流序号不连续", metrics["stream_gap_count"])}));
    }
    if !ready {
        alerts.push(json!({"code":"platform.recovery.incomplete","severity":"critical","detail":"启动恢复未完成，请查看恢复预览。"}));
    }
    Ok(json!({
        "generated_at": now(),
        "instance": {"id": hex(root.to_string_lossy().as_bytes())[..12], "pid": std::process::id(), "backend_url": format!("http://127.0.0.1:{port}"), "control_port": null},
        "health": {"status": if ready {"ready"} else {"degraded"}, "ready": true, "recovery": recovery, "runtime_instances": instances,
            "extensions": extension_health},
        "metrics": {
            "uptime_seconds": (STARTED.get_or_init(Instant::now).elapsed().as_secs_f64() * 1000.0).round() / 1000.0,
            "platform": {"event_watermark": metrics["event_watermark"], "pending_receipts": pending, "pending_receipts_by_type": platform_pending_types,
                "receipts_by_state": platform_states, "projection_lag_max": 0, "outbox": platform_outbox, "effects": effect_metrics["effects"], "effect_duration_avg_seconds": effect_metrics["effect_duration_avg_seconds"], "orphan_effects": effect_metrics["orphan_effects"]},
            "competition": {"event_watermark":competition["event_watermark"],"event_count":competition["event_count"],
                "pending_receipts":competition_receipts["pending"], "outbox": competition_outbox,
                "submissions":competition["submissions"], "sync_cursors":competition["sync_cursors"], "sync_errors":competition["sync_errors"],
                "connections":competition["connections"],"competitions":competition["competitions"],"browser_sessions":competition["browser_sessions"]},
            "src": {"outbox":src_outbox},
            "runtime": {"registered_instances": instances, "sessions_total": sessions.len(), "sessions_active": sessions.iter().filter(|s| s["status"] == "running").count(),
                "closed_duration_avg_seconds": if closed.is_empty() { 0.0 } else { closed.iter().sum::<f64>() / closed.len() as f64 }},
            "sse": {},
            "extensions": {"installed": ext_installed, "enabled": ext_enabled, "start_count": ext_start_count, "degraded": ext_degraded},
        },
        "alerts": alerts,
        "unavailable": {"sse": "此服务不统计 SSE 连接计数。"},
    }))
}

async fn overview(State(state): State<Arc<AppState>>) -> Result<Json<Value>, Failure> {
    Ok(Json(overview_body(&state)?))
}

#[derive(serde::Deserialize)]
struct OutboxQuery { limit: Option<usize> }
async fn outbox(State(state): State<Arc<AppState>>, Query(query): Query<OutboxQuery>) -> Result<Json<Value>, Failure> {
    let view = outbox_view::OutboxView::load(&state)?;
    Ok(Json(json!({"items":view.items(query.limit.unwrap_or(200))})))
}

async fn receipt_trace(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<Value>, Failure> {
    match state.store.command_receipt(&id).map_err(storage)? {
        Some(receipt) => {
            let mut effects = platform_effects::receipt_effects(&state, &id)?.0["effects"].clone();
            effects.as_array_mut().unwrap().extend(state.store.list("src_domain_effect_receipts")
                .map_err(storage)?.into_iter().filter(|row|row["command_id"] == id));
            let outbox = outbox_view::OutboxView::load(&state)?.for_command(&id);
            let competition = outbox.iter().any(|row|row["domain"] == "competition")
                || effects.as_array().is_some_and(|rows|rows.iter().any(|effect|effect["domain"] == "competition"));
            let src = outbox.iter().any(|row|row["domain"] == "src")
                || effects.as_array().is_some_and(|rows|rows.iter().any(|effect|effect["domain"] == "src"));
            Ok(Json(
                json!({"domain":if src {"src"} else if competition {"competition"} else {"platform"},"receipt":receipt,"effects":effects,"outbox":outbox}),
            ))
        }
        None => Err(fail(
            StatusCode::NOT_FOUND,
            "command.receipt_not_found",
            format!("no globally indexed receipt for command '{id}'"),
        )),
    }
}

fn recovery_body(state: &AppState) -> Result<Value, Failure> {
    let metrics = state
        .store
        .operations_metrics(state.chat.epoch())
        .map_err(storage)?;
    let competition = competition_api::live_metrics(state).map_err(storage)?;
    let competition_receipts = outbox_view::competition_receipts(state)?;
    let outboxes = outbox_view::OutboxView::load(state)?;
    let pending_total: i64 = ["accepted", "running", "waiting"]
        .iter()
        .filter_map(|s| metrics["receipts_by_state"][*s].as_i64())
        .sum();
    let pending = (pending_total-competition_receipts["pending"].as_i64().unwrap_or(0)).max(0);
    let platform_pending_types = subtract_counts(&metrics["pending_receipts_by_type"],&competition_receipts["pending_by_type"]);
    Ok(json!({
        "dry_run": true, "writes_performed": false,
        "platform": {"event_watermark": metrics["event_watermark"], "stream_gap_count": metrics["stream_gap_count"], "pending_receipts": pending,
            "pending_receipts_by_type": platform_pending_types, "pending_outbox": outboxes.recovery_pending("platform"), "projection_watermarks": {}},
        "competition": {"event_watermark": competition["event_watermark"], "pending_receipts": competition_receipts["pending"], "pending_outbox": outboxes.recovery_pending("competition")},
        "src": {"pending_outbox":outboxes.recovery_pending("src")},
        "last_startup_recovery": state.store.get("system", "startup_recovery").map_err(storage)?,
    }))
}

async fn recovery_preview(State(state): State<Arc<AppState>>) -> Result<Json<Value>, Failure> {
    Ok(Json(recovery_body(&state)?))
}

/// Stale `*.tmp` files under the state directory. Symlinks are never followed.
fn temporary_files(root: &FsPath) -> Vec<Value> {
    let now = SystemTime::now();
    let mut candidates: Vec<Value> = walkdir::WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_file() && entry.file_name().to_string_lossy().ends_with(".tmp"))
        .filter_map(|entry| {
            let age = now.duration_since(entry.metadata().ok()?.modified().ok()?).ok()?;
            let relative = entry.path().strip_prefix(root).ok()?.to_string_lossy().into_owned();
            (age >= TEMP_AGE).then(|| json!({"candidate_id": format!("temp:{relative}"), "kind": "temporary_file", "target": relative, "age_seconds": age.as_secs()}))
        })
        .collect();
    candidates.sort_by(|a, b| a["candidate_id"].as_str().cmp(&b["candidate_id"].as_str()));
    candidates
}

/// Unreferenced extension package directories under `extensions/installed` or `extensions/packages`.
fn orphan_extension_packages(state: &AppState, root: &FsPath) -> Result<Vec<Value>, Failure> {
    let mut referenced = std::collections::HashSet::new();
    for raw in state.store.list("task_extensions").map_err(storage)? {
        let item = raw.get("values").filter(|v| v.is_object()).unwrap_or(&raw);
        let mut add_path = |raw_path: &str| {
            if raw_path.trim().is_empty() {
                return;
            }
            let p = FsPath::new(raw_path);
            let resolved = if p.is_absolute() {
                p.to_path_buf()
            } else {
                root.join(p)
            };
            if let Ok(canon) = resolved.canonicalize() {
                referenced.insert(canon);
            }
            referenced.insert(resolved);
        };
        if let Some(dirs) = item["install_dirs"].as_object() {
            for val in dirs.values().filter_map(Value::as_str) {
                add_path(val);
            }
        }
        if let Some(val) = item["install_dir"].as_str() {
            add_path(val);
        }
    }
    let mut candidates = Vec::new();
    let mut seen_ids = std::collections::HashSet::new();
    for sub in ["extensions/installed", "extensions/packages"] {
        let install_root = root.join(sub);
        if !std::fs::symlink_metadata(&install_root)
            .is_ok_and(|m| m.file_type().is_dir() && !m.file_type().is_symlink())
        {
            continue;
        }
        let Ok(ext_entries) = std::fs::read_dir(&install_root) else {
            continue;
        };
        for ext_entry in ext_entries.filter_map(Result::ok) {
            if !ext_entry
                .file_type()
                .is_ok_and(|ft| ft.is_dir() && !ft.is_symlink())
            {
                continue;
            }
            let Ok(ver_entries) = std::fs::read_dir(ext_entry.path()) else {
                continue;
            };
            for ver_entry in ver_entries.filter_map(Result::ok) {
                if !ver_entry
                    .file_type()
                    .is_ok_and(|ft| ft.is_dir() && !ft.is_symlink())
                {
                    continue;
                }
                let path = ver_entry.path();
                let canon = path.canonicalize().unwrap_or_else(|_| path.clone());
                if referenced.contains(&path) || referenced.contains(&canon) {
                    continue;
                }
                let Ok(rel) = path.strip_prefix(root) else {
                    continue;
                };
                let relative = rel.to_string_lossy().replace('\\', "/");
                let candidate_id = format!("extension:{relative}");
                if seen_ids.insert(candidate_id.clone()) {
                    candidates.push(json!({
                        "candidate_id": candidate_id,
                        "kind": "orphan_extension_package",
                        "target": relative,
                    }));
                }
            }
        }
    }
    candidates.sort_by(|a, b| a["candidate_id"].as_str().cmp(&b["candidate_id"].as_str()));
    Ok(candidates)
}

fn maintenance_body(state: &AppState, root: &FsPath) -> Result<Value, Failure> {
    let mut candidates = temporary_files(root);
    candidates.extend(orphan_extension_packages(state, root)?);
    candidates.extend(competition_api::expired_browser_sessions(state).map_err(storage)?);
    Ok(json!({
        "dry_run": true,
        "writes_performed": false,
        "count": candidates.len(),
        "candidates": candidates,
        "unsupported_kinds": []
    }))
}

async fn maintenance_preview(State(state): State<Arc<AppState>>) -> Result<Json<Value>, Failure> {
    Ok(Json(maintenance_body(&state, &state_root(&state)?)?))
}

async fn maintenance(
    State(state): State<Arc<AppState>>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, Failure> {
    let invalid = || {
        fail(
            StatusCode::UNPROCESSABLE_ENTITY,
            "operations.maintenance.invalid",
            "Body must be {confirm: bool, candidate_ids: [string]}",
        )
    };
    let confirm = body
        .get("confirm")
        .and_then(Value::as_bool)
        .ok_or_else(invalid)?;
    let selected: Vec<String> = match body.get("candidate_ids") {
        None => Vec::new(),
        Some(Value::Array(ids)) if ids.len() <= 1000 => ids
            .iter()
            .map(|v| v.as_str().map(str::to_owned).ok_or_else(invalid))
            .collect::<Result<_, _>>()?,
        Some(_) => return Err(invalid()),
    };
    if !confirm {
        return Err(fail(
            StatusCode::BAD_REQUEST,
            "operations.maintenance.confirm_required",
            "维护清理需要 confirm=true",
        ));
    }
    let root = state_root(&state)?;
    let mut candidates = temporary_files(&root);
    candidates.extend(orphan_extension_packages(&state, &root)?);
    candidates.extend(competition_api::expired_browser_sessions(&state).map_err(storage)?);
    let known: Vec<&str> = candidates
        .iter()
        .filter_map(|c| c["candidate_id"].as_str())
        .collect();
    let stale: Vec<&String> = selected
        .iter()
        .filter(|id| !known.contains(&id.as_str()))
        .collect();
    if !stale.is_empty() {
        return Err(fail(
            StatusCode::CONFLICT,
            "operations.maintenance.stale_preview",
            format!("unknown or stale maintenance candidates: {stale:?}"),
        ));
    }
    let canonical_root = root.canonicalize().map_err(storage)?;
    let results: Vec<Value> = candidates
        .into_iter()
        .filter(|c| {
            selected.is_empty() || selected.iter().any(|id| c["candidate_id"] == id.as_str())
        })
        .map(|mut candidate| {
            if candidate["kind"] == "expired_browser_session" {
                candidate["removed"] = json!(candidate["target"].as_str().is_some_and(|id| {
                    competition_api::purge_expired_browser_session(&state, id).unwrap_or(false)
                }));
            } else if candidate["kind"] == "orphan_extension_package" {
                let path = root.join(candidate["target"].as_str().unwrap_or_default());
                let inside = path
                    .parent()
                    .and_then(|p| p.canonicalize().ok())
                    .is_some_and(|p| p.starts_with(&canonical_root));
                let is_dir = std::fs::symlink_metadata(&path)
                    .is_ok_and(|m| m.file_type().is_dir() && !m.file_type().is_symlink());
                candidate["removed"] =
                    json!(inside && is_dir && std::fs::remove_dir_all(&path).is_ok());
            } else {
                let path = root.join(candidate["target"].as_str().unwrap_or_default());
                let inside = path
                    .parent()
                    .and_then(|p| p.canonicalize().ok())
                    .is_some_and(|p| p.starts_with(&canonical_root));
                let regular =
                    std::fs::symlink_metadata(&path).is_ok_and(|m| m.file_type().is_file());
                candidate["removed"] =
                    json!(inside && regular && std::fs::remove_file(&path).is_ok());
            }
            candidate
        })
        .collect();
    let audit = root.join("operations/maintenance.jsonl");
    let line =
        json!({"at": now(), "actor": "local-user", "candidate_ids": selected, "results": results});
    append_private(&audit, &format!("{line}\n")).map_err(storage)?;
    Ok(Json(
        json!({"applied": results.iter().filter(|r| r["removed"] == true).count(), "results": results, "audit": "operations/maintenance.jsonl"}),
    ))
}

fn append_private(path: &FsPath, text: &str) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
    if let Some(parent) = path.parent() {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(parent)?;
    }
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(path)?
        .write_all(text.as_bytes())
}

/// Removes credentials, the home directory and other absolute paths from text.
fn redact(text: &str) -> String {
    static SECRET: OnceLock<regex::Regex> = OnceLock::new();
    static PATH: OnceLock<regex::Regex> = OnceLock::new();
    let secret = SECRET.get_or_init(|| {
        regex::Regex::new(r"(?i)(bearer|token|api[_-]?key|secret|password)(\s*[=:]\s*)\S+")
            .expect("secret pattern")
    });
    let path = PATH.get_or_init(|| {
        regex::Regex::new(r"(^|[^\w.~:/-])/(?:[\w.@+-]+/)+([\w.@+-]+)").expect("path pattern")
    });
    let mut text = secret.replace_all(text, "${1}${2}[REDACTED]").into_owned();
    if let Some(home) = std::env::var_os("HOME")
        .map(|h| h.to_string_lossy().into_owned())
        .filter(|h| h.len() > 1)
    {
        text = text.replace(&home, "~");
    }
    path.replace_all(&text, "${1}${2}").into_owned()
}

fn redact_value(value: &Value) -> Value {
    match value {
        Value::String(text) => Value::String(redact(text)),
        Value::Array(items) => Value::Array(items.iter().map(redact_value).collect()),
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(k, v)| (k.clone(), redact_value(v)))
                .collect(),
        ),
        other => other.clone(),
    }
}

async fn diagnostic_bundle(State(state): State<Arc<AppState>>) -> Result<Response, Failure> {
    use std::io::Write;
    let root = state_root(&state)?;
    let files = [
        ("overview.json", overview_body(&state)?),
        ("recovery-preview.json", recovery_body(&state)?),
        ("outbox.json", json!({"items":outbox_view::OutboxView::load(&state)?.items(1000)})),
        ("maintenance-preview.json", maintenance_body(&state, &root)?),
    ];
    let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let options = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);
    for (name, value) in files {
        zip.start_file(name, options).map_err(storage)?;
        zip.write_all(
            serde_json::to_string_pretty(&redact_value(&value))
                .map_err(storage)?
                .as_bytes(),
        )
        .map_err(storage)?;
    }
    let bytes = zip.finish().map_err(storage)?.into_inner();
    let mut response = bytes.into_response();
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/zip"),
    );
    headers.insert(
        header::CONTENT_DISPOSITION,
        HeaderValue::from_static("attachment; filename=muteki-diagnostic.zip"),
    );
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    Ok(response)
}

/// Built-in modules backed by live Rust routes.
fn descriptors() -> Vec<Value> {
    let module = |id: &str,
                  kind: &str,
                  task_kinds: Value,
                  executor: &str,
                  namespaces: Value,
                  routes: Value,
                  title: &str,
                  route: &str,
                  aggregate: &str| {
        let (description, icon) = match kind {
            "conversation" => ("Local conversations", "message-circle"),
            "single-security-task" => ("Security task", "flag"),
            "pentest" => ("Security assessment", "shield"),
            "competition" => ("比赛工作台", "trophy"),
            "src" => ("SRC 工作台", "target"),
            _ => ("", "layers"),
        };
        json!({"schema_version":1,"id":id,"version":"1","task_kinds":task_kinds,"required_capabilities":[],"default_executor":executor,"command_handlers":{},
            "event_namespaces":namespaces,"api_routes":routes,"workspace_kind":kind,
            "ui_contributions":{"title":title,"description":description,"icon":icon,"route":route,"create_entry":route,"aggregate_type":aggregate},
            "artifact_types":[],"graph_binding":null,"gate_binding":null})
    };
    vec![
        module(
            "builtin.conversation",
            "conversation",
            json!(["conversation", "coding"]),
            "external-agent.single",
            json!(["core."]),
            json!([
                "/api/projects",
                "/api/threads",
                "/api/threads/{thread_id}",
                "/api/threads/{thread_id}/events",
                "/api/threads/{thread_id}/commands",
                "/api/agent-runtimes"
            ]),
            "对话",
            "/chat",
            "thread",
        ),
        module(
            "builtin.single-security-task",
            "single-security-task",
            json!(["ctf"]),
            "swarm.coordinator",
            json!(["run.", "ctf."]),
            json!(["/api/runs"]),
            "CTF 工作台",
            "/ctf",
            "run",
        ),
        module(
            "builtin.pentest",
            "pentest",
            json!(["pentest"]),
            "swarm.coordinator",
            json!(["pentest."]),
            json!(["/api/runs/{run_id}/pentest-report"]),
            "渗透测试",
            "/pentest",
            "run",
        ),
        module(
            "builtin.competition",
            "competition",
            json!(["competition"]),
            "competition.scheduler",
            json!(["competition."]),
            json!(["/api/competitions", "/api/competitions/{competition_id}"]),
            "比赛",
            "/competitions",
            "competition",
        ),
        module(
            "builtin.src",
            "src",
            json!(["src.bounty"]),
            "swarm.coordinator",
            json!(["src."]),
            json!(["/api/src/tasks", "/api/src/tasks/{task_id}"]),
            "SRC 赏金",
            "/src",
            "task",
        ),
    ]
}

pub fn module_records(state: &AppState) -> rusqlite::Result<Vec<Value>> {
    let mut records:Vec<Value>=descriptors()
        .into_iter()
        .map(|descriptor| {
            let id = descriptor["id"].as_str().unwrap_or_default().to_owned();
            let runtime=crate::business::Module::from_descriptor(&id).map(|module|state.business.status(module));
            let (module_state, error) = if crate::business::Module::from_descriptor(&id).map(|module|crate::business::configured_enabled(state,module)).transpose()?.is_some_and(|enabled|!enabled) {
                ("disabled", Value::Null)
            } else {
                (match runtime.as_ref().and_then(|status|status["state"].as_str()){Some("ready")|None=>"ready",Some("failed")=>"unavailable",_=>"registered"}, runtime.as_ref().map(|status|status["error"].clone()).unwrap_or(Value::Null))
            };
            Ok(json!({"schema_version":1,"descriptor":descriptor,"state":module_state,"error":error,"components":[],"pending_components":[],"runtime":runtime,"origin":"builtin"}))
        })
        .collect::<rusqlite::Result<_>>()?;
    records.extend(crate::extensions::module_records(state).map_err(|_| rusqlite::Error::InvalidQuery)?);
    Ok(records)
}

async fn modules(State(state): State<Arc<AppState>>) -> Result<Json<Value>, Failure> {
    Ok(Json(json!(module_records(&state).map_err(storage)?)))
}

async fn module(State(state): State<Arc<AppState>>, Path(id): Path<String>) -> Response {
    match module_records(&state) {
        Ok(records) => match records
            .into_iter()
            .find(|r| r["descriptor"]["id"] == id.as_str())
        {
            Some(record) => Json(record).into_response(),
            None => (
                StatusCode::NOT_FOUND,
                Json(json!({"detail": format!("unknown module: {id}")})),
            )
                .into_response(),
        },
        Err(error) => storage(error).into_response(),
    }
}

fn hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn new_id(prefix: &str) -> String {
    format!("{prefix}-{}", uuid::Uuid::new_v4().simple())
}
pub(crate) fn platform_cursor_key(state: &AppState) -> Result<Vec<u8>, String> {
    use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
    const KEY: &str = "platform-event-cursor-key";
    if let Ok(value) = std::env::var("MUTEKI_CURSOR_SECRET") {
        if !value.trim().is_empty() {
            return Ok(Sha256::digest(value.trim().as_bytes()).to_vec());
        }
    }
    let read = || -> Result<Option<Vec<u8>>, String> {
        state.store.get("system", KEY).map_err(|error| error.to_string())?
            .map(|value| URL_SAFE_NO_PAD.decode(value.as_str().unwrap_or(""))
                .ok().filter(|bytes| bytes.len() == 32)
                .ok_or_else(|| "Event cursor key is unavailable".to_owned()))
            .transpose()
    };
    if let Some(key) = read()? { return Ok(key); }
    if let Some(root) = state.store.get("system", "history_root").map_err(|error| error.to_string())?
        .and_then(|value| value.as_str().map(str::to_owned))
    {
        let path = std::path::Path::new(&root).join("_platform/platform.db.cursor-key");
        if path.exists() {
            let encoded = std::fs::read_to_string(&path).map_err(|error| error.to_string())?;
            let encoded = encoded.trim();
            if encoded.len() != 64 || !encoded.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                return Err("Source event cursor key is invalid".into());
            }
            let key = (0..64).step_by(2)
                .map(|index| u8::from_str_radix(&encoded[index..index + 2], 16))
                .collect::<Result<Vec<_>, _>>()
                .map_err(|error| error.to_string())?;
            state.store.put_if_absent("system", KEY,
                &json!(URL_SAFE_NO_PAD.encode(key))).map_err(|error| error.to_string())?;
            return read()?.ok_or_else(|| "Event cursor key was not persisted".into());
        }
    }
    let mut random = [0u8; 32];
    getrandom::getrandom(&mut random).map_err(|error| error.to_string())?;
    state.store.put_if_absent("system", KEY,
        &json!(URL_SAFE_NO_PAD.encode(random))).map_err(|error| error.to_string())?;
    read()?.ok_or_else(|| "Event cursor key was not persisted".into())
}
fn transactional_competition_command(kind: &str) -> bool {
    matches!(kind,"connection.create" | "competition.connection.create"
        | "connection.unregister" | "competition.unregister"
        | "connection.test" | "competition.sync"
        | "connection.credential.set" | "connection.credential.revoke"
        | "connection.browser_session.import" | "connection.browser_session.renew"
        | "connection.browser_session.revoke"
        | "challenge.select" | "challenge.queue" | "competition.challenge.queue"
        | "challenge.skip" | "policy.update" | "competition.policy.update"
        | "scheduler.start" | "scheduler.pause" | "scheduler.resume"
        | "instance.ensure" | "instance.stop"
        | "run_binding.redirect" | "run_binding.pause" | "run_binding.resolve"
        | "competition.submission.submit" | "platform_submission.approve"
        | "platform_submission.retry" | "competition.submission.manual_override")
}
fn planned_competition_command(kind: &str) -> bool {
    matches!(kind, "competition.message" | "connection.update" | "competition.update")
        || competition_api::is_binding_command(kind)
        || transactional_competition_command(competition_api::canonical_command_type(kind))
}
pub(crate) fn command_fingerprint(envelope: &Value) -> String {
    let content = json!({"command_type":envelope["command_type"],
        "aggregate_type":envelope["aggregate_type"],"aggregate_id":envelope["aggregate_id"],
        "payload":envelope["payload"]});
    format!("{:x}", Sha256::digest(
        crate::runs::src_api::source_compact_json(&content).as_bytes()))
}
fn idempotency_index(key: &str) -> String {
    format!("{:x}", Sha256::digest(key.as_bytes()))
}
pub(crate) fn competition_command_identity(envelope: &Value) -> (String, String, String) {
    let request = command_fingerprint(envelope);
    let semantic = request.clone();
    let key_id = envelope["idempotency_key"]
        .as_str()
        .filter(|key| !key.is_empty())
        .map(idempotency_index)
        .unwrap_or_default();
    (request, semantic, key_id)
}
pub(crate) fn planned_receipt_status(receipt: &Value) -> StatusCode {
    match receipt["state"].as_str().unwrap_or("") {
        "completed" => StatusCode::OK,
        "accepted" | "running" | "waiting" => StatusCode::ACCEPTED,
        "conflict" => StatusCode::CONFLICT,
        "failed" => match receipt["error"]["category"].as_str().unwrap_or("") {
            "validation" => StatusCode::BAD_REQUEST,
            "not_found" => StatusCode::NOT_FOUND,
            "permission" => StatusCode::FORBIDDEN,
            "state" | "conflict" => StatusCode::CONFLICT,
            _ => StatusCode::BAD_GATEWAY,
        },
        _ => StatusCode::OK,
    }
}
fn planned_existing(state: &AppState, command_id: &str, digest: &str,
    semantic: &str, key_id: Option<&str>) -> Result<Option<Response>, Failure> {
    competition_api::settle_planned_result(state,command_id);
    if let Some(stored) = state.store.get(COMMANDS,command_id).map_err(storage)? {
        if stored["request_fingerprint"] != digest {
            return Err(fail(StatusCode::CONFLICT,"command.idempotency_conflict",
                "command_id was used with different content"));
        }
        let mut receipt = stored["response"]["receipt"].clone();
        receipt["deduplicated"] = json!(true);
        let status = planned_receipt_status(&receipt);
        return Ok(Some((status,Json(json!({"receipt":receipt}))).into_response()));
    }
    if let Some(key_id) = key_id
        && let Some(keyed) = state.store.get(IDEMPOTENCY_KEYS,key_id).map_err(storage)? {
        if keyed["semantic_fingerprint"] != semantic {
            return Err(fail(StatusCode::CONFLICT,"command.idempotency_conflict",
                "idempotency_key was used with different content"));
        }
        let prior_id = keyed["command_id"].as_str().unwrap_or("");
        competition_api::settle_planned_result(state,prior_id);
        let stored = state.store.get(COMMANDS,prior_id).map_err(storage)?
            .ok_or_else(||fail(StatusCode::CONFLICT,"command.outcome_unknown",
                "Prior command outcome must be inspected before retry"))?;
        let mut receipt = stored["response"]["receipt"].clone();
        receipt["deduplicated"] = json!(true);
        let status = planned_receipt_status(&receipt);
        return Ok(Some((status,Json(json!({"receipt":receipt}))).into_response()));
    }
    if state.store.get(COMMAND_CLAIMS,command_id).map_err(storage)?.is_some() {
        return Err(fail(StatusCode::CONFLICT,"command.outcome_unknown",
            "Prior command outcome must be inspected before retry"));
    }
    Ok(None)
}
fn aggregate_version(
    state: &AppState,
    aggregate_type: &str,
    aggregate_id: &str,
) -> Result<u64, Failure> {
    if aggregate_id.is_empty() {
        return Ok(0);
    }
    let key = format!("{aggregate_type}\0{aggregate_id}");
    let stored = state
        .store
        .get(AGGREGATE_VERSIONS, &key)
        .map_err(storage)?
        .and_then(|v| v["version"].as_u64())
        .unwrap_or(0);
    let conversation = match aggregate_type {
        "thread" => state
            .store
            .get(crate::chat::COLLECTION, aggregate_id)
            .map_err(storage)?
            .as_ref()
            .map(crate::chat::stream_head)
            .unwrap_or(0),
        "project" => u64::from(
            state
                .store
                .get("projects", aggregate_id)
                .map_err(storage)?
                .is_some(),
        ),
        "workspace" => u64::from(
            state
                .store
                .get("workspaces", aggregate_id)
                .map_err(storage)?
                .is_some(),
        ),
        _ => 0,
    };
    Ok(stored.max(conversation).max(platform_events::stream_version(
        state,
        aggregate_type,
        aggregate_id,
    )?).max(competition_api::event_stream_version(state, aggregate_type, aggregate_id).map_err(storage)?))
}
fn save_aggregate_version(
    state: &AppState,
    aggregate_type: &str,
    aggregate_id: &str,
    prior: u64,
) -> Result<(), Failure> {
    if aggregate_id.is_empty() {
        return Ok(());
    }
    let key = format!("{aggregate_type}\0{aggregate_id}");
    let current = aggregate_version(state, aggregate_type, aggregate_id)?;
    if current > prior { return Ok(()); }
    state.store.advance_version_with_record(AGGREGATE_VERSIONS, &key, aggregate_type,
        aggregate_id, current, &now(), None).map_err(storage)?;
    Ok(())
}
pub(crate) fn advance_direct_version(state: &AppState, aggregate_type: &str, aggregate_id: &str) -> Result<(), String> {
    let floor = aggregate_version(state, aggregate_type, aggregate_id).map_err(|error| error.1.0.to_string())?;
    let key = format!("{aggregate_type}\0{aggregate_id}");
    state.store.advance_version_with_record(AGGREGATE_VERSIONS, &key, aggregate_type,
        aggregate_id, floor, &now(), None).map_err(|error| error.to_string())?;
    Ok(())
}
pub(crate) fn current_aggregate_version(state: &AppState, aggregate_type: &str, aggregate_id: &str) -> Result<u64, String> {
    aggregate_version(state, aggregate_type, aggregate_id).map_err(|error| error.1.0.to_string())
}
pub(crate) fn persist_competition_event(state: &AppState, aggregate_type: &str, aggregate_id: &str,
    collection: &str, competition_id: &str, event: Value) -> Result<Value, String> {
    let floor = aggregate_version(state, aggregate_type, aggregate_id).map_err(|error| error.1.0.to_string())?;
    state.store.append_competition_event(AGGREGATE_VERSIONS, collection, competition_id,
        aggregate_type, aggregate_id, floor, event).map_err(|error| error.to_string())
}
pub(crate) fn persist_competition_plan(
    state: &AppState,
    command_id: &str,
    command: &Value,
    competition_id: &str,
    events: &[Value],
) -> Result<Option<Vec<Value>>, String> {
    let floors = events.iter().map(|event| {
        let aggregate_type = event["aggregate_type"].as_str().ok_or_else(||"planned event aggregate type missing".to_owned())?;
        let aggregate_id = event["aggregate_id"].as_str().ok_or_else(||"planned event aggregate id missing".to_owned())?;
        let floor = aggregate_version(state, aggregate_type, aggregate_id).map_err(|error|error.1.0.to_string())?;
        Ok((event.clone(),floor))
    }).collect::<Result<Vec<_>,String>>()?;
    state.store.put_many_if_with_competition_events(
        &[(COMMANDS,command_id,None)], &[(COMMANDS,command_id,command)],
        AGGREGATE_VERSIONS, "competition_events_live", competition_id, &floors,
    ).map_err(|error|error.to_string())
}
/// Runs `module.enable` / `module.disable`, storing a receipt `/api/receipts` serves.
fn run_module_command(state: &AppState, original: &Value) -> Result<(StatusCode, Value), Failure> {
    let mut envelope = original.clone();
    if envelope["command_id"].as_str().is_none_or(str::is_empty) {
        envelope["command_id"] = json!(new_id("cmd"));
    }
    let command_id = envelope["command_id"].as_str().unwrap();
    let fingerprint = command_fingerprint(&envelope);
    let key_id = envelope["idempotency_key"].as_str().filter(|key| !key.is_empty()).map(idempotency_index);
    let replay = |stored: &Value| {
        let mut receipt = stored["response"]["receipt"].clone();
        receipt["deduplicated"] = json!(true);
        let status = StatusCode::from_u16(stored["status"].as_u64().unwrap_or(200) as u16).unwrap_or(StatusCode::OK);
        (status, json!({"receipt":receipt}))
    };
    if let Some(stored) = state.store.get(COMMANDS, command_id).map_err(storage)? {
        if stored["request"] != envelope {
            return Err(fail(StatusCode::CONFLICT,"command.idempotency_conflict","Command identity was used with different content"));
        }
        return Ok(replay(&stored));
    }
    if let Some(key_id) = &key_id {
        if let Some(prior) = state.store.get(IDEMPOTENCY_KEYS, key_id).map_err(storage)? {
            if prior["semantic_fingerprint"] != fingerprint {
                return Err(fail(StatusCode::CONFLICT,"command.idempotency_conflict","Idempotency key was used with different content"));
            }
            let prior_id = prior["command_id"].as_str().unwrap_or("");
            let stored = state.store.get(COMMANDS, prior_id).map_err(storage)?.ok_or_else(|| storage("Module receipt missing"))?;
            return Ok(replay(&stored));
        }
    }
    let enabled = envelope["command_type"] == "module.enable";
    let module_id = envelope["aggregate_id"].as_str().unwrap_or_default();
    let version = aggregate_version(state,"module",module_id)?;
    if let Some(expected) = envelope.get("expected_version").filter(|value| !value.is_null()) {
        let expected = expected.as_u64().ok_or_else(|| fail(StatusCode::BAD_REQUEST,"platform.request.invalid","expected_version must be a nonnegative integer"))?;
        if expected != version {
            return Err(fail(StatusCode::CONFLICT,"command.version_conflict",format!("Expected version {expected}, current version {version}")));
        }
    }
    let records = module_records(state).map_err(storage)?;
    let record = records.iter().find(|record| record["descriptor"]["id"] == module_id)
        .ok_or_else(|| fail(StatusCode::NOT_FOUND,"module.not_found",format!("unknown module: {module_id}")))?;
    if record["origin"].as_str().is_some_and(|origin| origin.starts_with("extension:")) {
        return Err(fail(StatusCode::CONFLICT,"module.extension_management_required","Manage extension modules through the extension lifecycle API"));
    }
    let module=crate::business::Module::from_descriptor(module_id);
    let previous = state.store.get(MODULE_FLAGS,module_id).map_err(storage)?;
    let siblings=module.map(crate::business::descriptor_ids).unwrap_or(&[]).iter().filter(|id|**id!=module_id)
        .map(|id|Ok((*id,state.store.get(MODULE_FLAGS,id).map_err(storage)?,json!({"module_id":id,"enabled":enabled,"updated_at":now()}))))
        .collect::<Result<Vec<_>,Failure>>()?;
    let flag = json!({"module_id":module_id,"enabled":enabled,"updated_at":now()});
    let next_state=if !enabled{"disabled"}else if record["runtime"]["state"]=="ready"{"ready"}else{"registered"};
    let scope = envelope["_verified_scope"].as_str().unwrap_or("operator:local-user");
    let receipt = json!({"schema_version":1,"command_id":command_id,"receipt_id":new_id("rcpt"),"state":"completed","run_id":null,
        "aggregate":{"type":"module","id":module_id},"event_cursor":platform_events::signed_domain(state,"module",module_id,version+1,scope)?,
        "deduplicated":false,"submitted_at":now(),"completed_at":now(),"next":["receipt","events","wait"],
        "output":{"module_id":module_id,"state":next_state,"changed":record["state"] != next_state},"effect_ids":[],"error":null});
    let response = json!({"receipt":receipt});
    let stored = json!({"request":envelope,"request_fingerprint":fingerprint,"response":response,"status":200});
    let keyed = json!({"command_id":command_id,"semantic_fingerprint":fingerprint,"created_at":now()});
    let mut conditions = vec![(COMMANDS,command_id,None),(MODULE_FLAGS,module_id,previous.as_ref())];
    let mut writes = vec![(COMMANDS,command_id,&stored),(MODULE_FLAGS,module_id,&flag)];
    for(id,prior,flag)in &siblings{conditions.push((MODULE_FLAGS,*id,prior.as_ref()));writes.push((MODULE_FLAGS,*id,flag));}
    if let Some(key_id) = &key_id {
        conditions.push((IDEMPOTENCY_KEYS,key_id.as_str(),None));
        writes.push((IDEMPOTENCY_KEYS,key_id.as_str(),&keyed));
    }
    let event = domain_events::event(&envelope,"module",module_id,
        if enabled{"core.module.enabled"}else{"core.module.disabled"},"platform.registry",json!({"module_id":module_id,"enabled":enabled}));
    if domain_events::commit(state,&conditions,&writes,&[event])?.is_none() {
        return Err(fail(StatusCode::CONFLICT,"command.version_conflict","Module changed during command admission; read the current state and retry"));
    }
    if !enabled {if let Some(module)=module{state.business.disable(module);}}
    Ok((StatusCode::OK,response))
}

async fn module_action(
    State(state): State<Arc<AppState>>,
    Path((id, action)): Path<(String, String)>,
    body: Option<Json<Value>>,
) -> Result<Response, Failure> {
    if action=="retry" {
        let module=crate::business::Module::from_descriptor(&id).ok_or_else(||fail(StatusCode::NOT_FOUND,"module.not_found","Unknown built-in module"))?;
        crate::business::retry(state.clone(),module);
        return Ok((StatusCode::ACCEPTED,Json(json!({"module_id":id,"state":"recovery_requested"}))).into_response());
    }
    if !matches!(action.as_str(), "enable" | "disable") {
        return Ok((
            StatusCode::NOT_FOUND,
            Json(json!({"detail": "unknown module action"})),
        )
            .into_response());
    }
    let body = body.map(|Json(v)| v).unwrap_or_else(|| json!({}));
    let mut envelope = json!({"command_type": format!("module.{action}"), "aggregate_type": "module", "aggregate_id": id, "payload": {"module_id": id}});
    if let Some(command_id) = body
        .get("command_id")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    {
        envelope["command_id"] = json!(command_id);
    }
    let (status, response) = run_module_command(&state, &envelope)?;
    if action=="enable" {if let Some(module)=crate::business::Module::from_descriptor(&id){crate::business::retry(state.clone(),module);}}
    Ok((status, Json(response)).into_response())
}

const ENVELOPE_FIELDS: [&str; 9] = [
    "schema_version",
    "command_id",
    "command_type",
    "aggregate_type",
    "aggregate_id",
    "expected_version",
    "actor",
    "idempotency_key",
    "payload",
];

async fn normalize_extension_response(
    state: &AppState,
    envelope: &Value,
    response: Response,
) -> Result<Response, Failure> {
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .map_err(storage)?;
    let mut body: Value = serde_json::from_slice(&bytes).map_err(storage)?;
    if status.is_success() && !body["receipt"].is_object() {
        let command_id = envelope["command_id"].as_str().unwrap_or("");
        let prior = state.store.get(COMMANDS,command_id).map_err(storage)?;
        let accepted = prior.as_ref().map(|row|row["response"]["receipt"].clone()).unwrap_or(Value::Null);
        let receipt = json!({"schema_version":1,"command_id":command_id,"receipt_id":accepted["receipt_id"].as_str().map(str::to_owned).unwrap_or_else(||new_id("rcpt")),"state":"completed","run_id":null,
            "aggregate":{"type":"extension","id":envelope["aggregate_id"]},"event_cursor":null,"deduplicated":false,
            "submitted_at":now(),"completed_at":now(),"next":[],"output":{"result":body["result"]},"effect_ids":[],"error":null});
        body["receipt"] = receipt;
    }
    Ok((status, Json(body)).into_response())
}

fn persist_planned_terminal(state: &AppState, envelope: &Value, digest: &str,
    semantic: &str, key_id: Option<&str>, status: StatusCode, body: &Value) -> Result<Response, Failure> {
    let command_id = envelope["command_id"].as_str().unwrap_or("");
    let timestamp = now();
    let code = body["error"]["code"].as_str().unwrap_or("command.plan_failed");
    let category = if code == "command.version_conflict"
        || (matches!(envelope["command_type"].as_str(),Some("effect.retry" | "effect.cancel"))
            && matches!(code,"effect.retry_not_allowed" | "effect.irreversible")) {
        "conflict"
    } else {match status {
        StatusCode::BAD_REQUEST | StatusCode::UNPROCESSABLE_ENTITY => "validation",
        StatusCode::NOT_FOUND => "not_found",
        StatusCode::FORBIDDEN | StatusCode::UNAUTHORIZED => "permission",
        StatusCode::CONFLICT => "state",
        _ => "internal",
    }};
    let receipt = json!({"schema_version":1,"command_id":command_id,"receipt_id":new_id("rcpt"),
        "state":if category == "conflict" {"conflict"} else {"failed"},"run_id":null,
        "aggregate":{"type":envelope["aggregate_type"],"id":envelope["aggregate_id"]},
        "event_cursor":null,"deduplicated":false,"submitted_at":timestamp,"completed_at":timestamp,
        "next":["receipt","snapshot","events","wait"],"output":{},"effect_ids":[],
        "error":{"schema_version":1,"code":code,"message":body["error"]["message"],"category":category,
            "recovery_hint":body["error"]["recovery_hint"].as_str().unwrap_or(""),
            "correlation_id":body["error"]["correlation_id"].as_str().unwrap_or(command_id),
            "retryable":body["error"]["retryable"].as_bool().unwrap_or(false),
            "detail":body["error"]["detail"].as_object().cloned().unwrap_or_default()}});
    let mut output = body.clone();
    output["receipt"] = receipt.clone();
    let response_status = planned_receipt_status(&receipt);
    let mut stored_payload = envelope["payload"].clone();
    if envelope["command_type"].as_str().is_some_and(|kind| kind.starts_with("workbench.")) {
        crate::workbench_settings::redact_request(&mut stored_payload);
    }
    if envelope["command_type"] == "competition.submission.manual_override" {
        if let Some(payload) = stored_payload.as_object_mut() {
            payload.remove("answer");
            payload.remove("confirm_answer");
        }
    }
    let stored = json!({"request":{"command_type":envelope["command_type"],
        "aggregate_type":envelope["aggregate_type"],"aggregate_id":envelope["aggregate_id"],
        "payload":stored_payload,"expected_version":envelope["expected_version"]},
        "request_fingerprint":digest,"response":{"receipt":receipt},"status":response_status.as_u16()});
    let claim = json!({"command_id":command_id,"request_fingerprint":digest,
        "state":"failed","updated_at":timestamp});
    let keyed = key_id.map(|_|json!({"command_id":command_id,
        "semantic_fingerprint":semantic,"created_at":timestamp}));
    let mut conditions = vec![(COMMANDS,command_id,None),(COMMAND_CLAIMS,command_id,None)];
    let mut records = vec![(COMMANDS,command_id,&stored),(COMMAND_CLAIMS,command_id,&claim)];
    if let (Some(key_id),Some(keyed)) = (key_id,keyed.as_ref()) {
        conditions.push((IDEMPOTENCY_KEYS,key_id,None));
        records.push((IDEMPOTENCY_KEYS,key_id,keyed));
    }
    if state.store.put_many_if(&conditions,&records).map_err(storage)? {
        return Ok((response_status,Json(output)).into_response());
    }
    planned_existing(state,command_id,digest,semantic,key_id)?
        .ok_or_else(||fail(StatusCode::CONFLICT,"command.outcome_unknown",
            "Prior command outcome must be inspected before retry"))
}

async fn settle_command_response(
    state: &AppState,
    envelope: &Value,
    digest: &str,
    semantic: &str,
    key_id: Option<&str>,
    effect_id: Option<&str>,
    principal: &str,
    planned: bool,
    response: Response,
) -> Result<Response, Failure> {
    let mut status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .map_err(storage)?;
    let mut body: Value = serde_json::from_slice(&bytes).map_err(storage)?;
    let command_id = envelope["command_id"].as_str().unwrap_or("");
    let prior = state.store.get(COMMANDS,command_id).map_err(storage)?;
    let prior_receipt = prior.as_ref().map(|row| row["response"]["receipt"].clone()).unwrap_or(Value::Null);
    let supplied_receipt = body["receipt"].is_object();
    if !supplied_receipt {
        let success = status.is_success();
        let code = body["error"]["code"].as_str().unwrap_or("command.failed");
        let message = redact(
            body["error"]["message"]
                .as_str()
                .unwrap_or("Command failed"),
        );
        let category = body["error"]["category"].as_str().unwrap_or(match status {
            StatusCode::BAD_REQUEST | StatusCode::UNPROCESSABLE_ENTITY => "validation",
            StatusCode::NOT_FOUND => "not_found",
            StatusCode::FORBIDDEN | StatusCode::UNAUTHORIZED => "permission",
            StatusCode::CONFLICT => {
                if code == "command.version_conflict" {
                    "conflict"
                } else {
                    "state"
                }
            }
            _ => "internal",
        });
        let mut err_obj = json!({"schema_version":1,"code":code,"message":message,"category":category,
            "recovery_hint":body["error"]["recovery_hint"].as_str().unwrap_or(""),
            "correlation_id":body["error"]["correlation_id"].as_str().unwrap_or(command_id),
            "retryable":body["error"]["retryable"].as_bool().unwrap_or(false),"detail":{}});
        if body["error"]["detail"].is_object() {
            err_obj["detail"] = body["error"]["detail"].clone();
        }
        body["receipt"] = json!({"schema_version":1,"command_id":command_id,"receipt_id":prior_receipt["receipt_id"].as_str().map(str::to_owned).unwrap_or_else(||new_id("rcpt")),
            "state":if success {"completed"} else if status==StatusCode::CONFLICT {"conflict"} else {"failed"},"run_id":null,
            "aggregate":{"type":envelope["aggregate_type"],"id":envelope["aggregate_id"]},"event_cursor":null,
            "deduplicated":false,"submitted_at":now(),"completed_at":now(),"next":if success {json!([])} else {json!(["receipt"])},"output":{},
            "effect_ids":effect_id.map(|id|json!([id])).unwrap_or_else(||json!([])),
            "error":if success {Value::Null} else {err_obj}});
    } else if matches!(body["receipt"]["state"].as_str(), Some("failed" | "conflict")) && status.is_success() {
        status = planned_receipt_status(&body["receipt"]);
    }
    if let Some(receipt) = body["receipt"].as_object_mut() {
        receipt.entry("schema_version").or_insert(json!(1));
        receipt.entry("receipt_id").or_insert_with(|| prior_receipt.get("receipt_id").cloned().unwrap_or_else(||json!(new_id("rcpt"))));
        receipt.entry("run_id").or_insert(Value::Null);
        receipt.entry("deduplicated").or_insert(json!(false));
        receipt.entry("submitted_at").or_insert_with(|| json!(now()));
        receipt.entry("next").or_insert(json!(["receipt", "snapshot", "events", "wait"]));
        receipt.entry("effect_ids").or_insert(json!([]));
        receipt.entry("error").or_insert(Value::Null);
        if receipt.get("state") == Some(&json!("completed")) {
            receipt.entry("completed_at").or_insert_with(|| json!(now()));
        }
        if receipt.get("event_cursor").is_none_or(Value::is_null)
            && receipt["aggregate"]["type"] == "thread" && status.is_success()
        {
            if let Some(thread_id) = receipt["aggregate"]["id"].as_str() {
                if let Some(seq) = state.store.get(crate::chat::COLLECTION, thread_id).map_err(storage)?
                    .and_then(|doc| doc["event_watermark"].as_u64()) {
                    receipt.insert("event_cursor".to_owned(), json!(platform_events::signed(state, "thread", thread_id, seq, principal)?));
                }
            }
        }
    }
    if let Some(effect_id) = effect_id {
        if let Some(effect) = state.store.get("platform_effect_receipts", effect_id).map_err(storage)? {
            let state_name = effect["state"].as_str().unwrap_or("");
            body["receipt"]["state"] = json!(if state_name == "dead_letter" { "failed" } else { state_name });
            body["receipt"]["completed_at"] = if matches!(state_name, "completed" | "cancelled" | "dead_letter") {
                json!(now())
            } else { Value::Null };
            if effect["error"].is_object() {
                body["receipt"]["error"] = effect["error"].clone();
            }
        }
    }
    if planned {
        if supplied_receipt {status = planned_receipt_status(&body["receipt"]);}
        if body["receipt"]["deduplicated"] == true {
            return Ok((status,Json(body)).into_response());
        }
        if let Some(prior) = prior.as_ref() {
            if prior["request_fingerprint"] != digest {
                return Err(fail(StatusCode::CONFLICT,"command.idempotency_conflict",
                    "command_id was used with different content"));
            }
            if !status.is_success() {
                return planned_existing(state,command_id,digest,semantic,key_id)?
                    .ok_or_else(||storage("Planned command receipt disappeared"));
            }
        } else {
            if status.is_success() {
                return Err(storage("Planned command returned success without an accepted receipt"));
            }
            if matches!(body["error"]["code"].as_str(),
                Some("command.idempotency_conflict" | "command.outcome_unknown")) {
                return Ok((status,Json(body)).into_response());
            }
            return persist_planned_terminal(state,envelope,digest,semantic,key_id,status,&body);
        }
    }
    let mut stored=prior.clone().unwrap_or_else(||json!({
        "request":{"command_type":envelope["command_type"],"aggregate_id":envelope["aggregate_id"]},"response":{},"status":status.as_u16()}));
    if envelope["command_type"].as_str().is_some_and(|kind| kind.starts_with("run_binding.")
        || matches!(kind,"connection.test" | "competition.sync" | "challenge.select"
            | "challenge.queue" | "competition.challenge.queue" | "challenge.skip"
            | "policy.update" | "competition.policy.update" | "scheduler.start"
            | "scheduler.pause" | "scheduler.resume"))
        && prior.is_some()
    {
        loop {
            let local_result = envelope["command_type"].as_str().and_then(|kind|match kind {
                "challenge.select" | "challenge.queue" | "competition.challenge.queue" | "challenge.skip"
                    => Some("competition_challenge_effect_results"),
                "policy.update" | "competition.policy.update" | "scheduler.start"
                    | "scheduler.pause" | "scheduler.resume"
                    => Some("competition_control_effect_results"),
                _ => None,
            });
            if local_result.is_some()
                && stored["response"]["receipt"]["state"] == "accepted" {
                body["receipt"] = stored["response"]["receipt"].clone();
                status = StatusCode::ACCEPTED;
            }
            if (body["receipt"]["state"] == "accepted"
                && stored["response"]["receipt"]["state"] == "waiting")
                || (matches!(body["receipt"]["state"].as_str(),Some("accepted" | "waiting"))
                    && matches!(stored["response"]["receipt"]["state"].as_str(),
                        Some("running" | "completed" | "failed" | "conflict" | "dead_letter")))
            {
                body["receipt"] = stored["response"]["receipt"].clone();
            }
            let mut next = stored.clone();
            next["response"]["receipt"] = body["receipt"].clone();
            next["request_fingerprint"] = json!(digest);
            next["status"] = json!(status.as_u16());
            if state.store.put_many_if(&[(COMMANDS,command_id,Some(&stored))],
                &[(COMMANDS,command_id,&next)]).map_err(storage)? {
                break;
            }
            stored = state.store.get(COMMANDS,command_id).map_err(storage)?
                .ok_or_else(||storage("Competition command disappeared while settling"))?;
        }
        return Ok((status, Json(body)).into_response());
    }
    let lifecycle = envelope["command_type"].as_str().is_some_and(crate::extensions::supports_lifecycle_command)
        && envelope["command_type"] != "extension.propose_event";
    if lifecycle && stored["domain_settled"] == true {
        body["receipt"] = stored["response"]["receipt"].clone();
        return Ok((status,Json(body)).into_response());
    }
    if lifecycle {
        let kind = envelope["command_type"].as_str().unwrap();
        let aggregate_id = body["receipt"]["aggregate"]["id"].as_str().unwrap_or("").to_owned();
        let version = aggregate_version(state,"extension",&aggregate_id)?;
        body["receipt"]["event_cursor"] = json!(platform_events::signed_domain(state,"extension",&aggregate_id,version+1,principal)?);
        let outcome = if status.is_success() {"completed"} else {"failed"};
        let event = domain_events::event(envelope,"extension",&aggregate_id,
            &format!("core.{kind}.{outcome}"),"platform.extensions",if status.is_success(){
                body.get("result").cloned().unwrap_or_else(||body["receipt"]["output"].clone())
            }else{json!({"error":body["receipt"]["error"]})});
        stored["response"]["receipt"] = body["receipt"].clone();
        stored["request_fingerprint"] = json!(digest);
        stored["status"] = json!(status.as_u16());
        stored["domain_settled"] = json!(true);
        if domain_events::commit(state,&[(COMMANDS,command_id,prior.as_ref())],&[(COMMANDS,command_id,&stored)],&[event])?.is_none() {
            return Err(fail(StatusCode::CONFLICT,"command.version_conflict","Extension result remains journaled; retry receipt settlement"));
        }
        return Ok((status,Json(body)).into_response());
    }
    stored["response"]["receipt"] = body["receipt"].clone();
    stored["request_fingerprint"] = json!(digest);
    stored["status"] = json!(status.as_u16());
    state
        .store
        .put(COMMANDS, command_id, &stored)
        .map_err(storage)?;
    Ok((status, Json(body)).into_response())
}

fn persist_thread_version_conflict(
    state: &AppState,
    envelope: &Value,
    digest: &str,
    semantic: &str,
    key_id: Option<&str>,
    current: u64,
    expected: u64,
) -> Result<Response, Failure> {
    let command_id = envelope["command_id"].as_str().unwrap_or("");
    let timestamp = now();
    let agg_type = envelope["aggregate_type"]
        .as_str()
        .filter(|s| !s.is_empty())
        .unwrap_or("thread");
    let receipt = json!({"schema_version":1,"command_id":command_id,"receipt_id":new_id("rcpt"),
        "state":"conflict","run_id":null,"aggregate":{"type":agg_type,"id":envelope["aggregate_id"]},
        "event_cursor":null,"deduplicated":false,"submitted_at":timestamp,"completed_at":timestamp,
        "next":["receipt","snapshot"],"output":{},"effect_ids":[],
        "error":{"code":"command.version_conflict","message":format!("Expected version {expected}, current version {current}"),
            "category":"conflict","retryable":false}});
    let error = receipt["error"].clone();
    let stored = json!({"request":{"command_type":envelope["command_type"],"aggregate_id":envelope["aggregate_id"]},
        "request_fingerprint":digest,"response":{"receipt":receipt},"status":409});
    let claim = json!({"command_id":command_id,"request_fingerprint":digest,"state":"failed","updated_at":timestamp});
    let keyed = key_id.map(|_| json!({"command_id":command_id,"semantic_fingerprint":semantic,"created_at":timestamp}));
    let mut conditions = vec![(COMMANDS, command_id, None), (COMMAND_CLAIMS, command_id, None)];
    let mut records = vec![(COMMANDS, command_id, &stored), (COMMAND_CLAIMS, command_id, &claim)];
    if let (Some(key_id), Some(keyed)) = (key_id, keyed.as_ref()) {
        conditions.push((IDEMPOTENCY_KEYS, key_id, None));
        records.push((IDEMPOTENCY_KEYS, key_id, keyed));
    }
    if !state.store.put_many_if(&conditions, &records).map_err(storage)? {
        return Err(fail(StatusCode::CONFLICT,"command.outcome_unknown",
            "Prior command outcome must be inspected before retry"));
    }
    Ok((StatusCode::CONFLICT, Json(json!({"receipt":receipt,"error":error}))).into_response())
}

async fn command(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    gateway_candidate: Option<Extension<crate::access_api::GatewayCandidate>>,
    body: Option<Json<Value>>,
) -> Result<Response, Failure> {
    let invalid = |message: &str| {
        fail(
            StatusCode::BAD_REQUEST,
            "platform.request.invalid",
            message.to_owned(),
        )
    };
    let Some(Json(mut envelope)) = body else {
        return Err(invalid("Body must be a command envelope"));
    };
    let Some(map) = envelope.as_object() else {
        return Err(invalid("Body must be a command envelope"));
    };
    if let Some(field) = map.keys().find(|k| !ENVELOPE_FIELDS.contains(&k.as_str())) {
        return Err(invalid(&format!("unknown field: {field}")));
    }
    let command_type = map
        .get("command_type")
        .and_then(Value::as_str)
        .ok_or_else(|| invalid("command_type is required"))?
        .to_owned();
    crate::business::require_operation(&state,&command_type,&envelope).map_err(|e|e.failure())?;
    if run_commands::supports(&command_type) {
        return run_commands::handle(state, &headers, gateway_candidate.is_some(), envelope).await;
    }
    if command_type == "task.create" {
        return task_dispatch::create(state, &headers, gateway_candidate.is_some(), envelope).await;
    }
    if command_type == "src.task.create" {
        if envelope["aggregate_type"]
            .as_str()
            .is_some_and(|value| !value.is_empty() && value != "task")
        {
            return Err(invalid("SRC domain aggregate_type must be task"));
        }
        let requested_id = envelope["aggregate_id"].as_str().unwrap_or("").to_owned();
        envelope["aggregate_type"] = json!("task");
        envelope["aggregate_id"] = json!(requested_id);
        let principal = platform_auth::authorize(
            &state,
            &headers,
            gateway_candidate.is_some(),
            &envelope,
            "command",
            "src.task.create",
            "task",
            &requested_id,
        )?;
        if !principal.starts_with("operator:") && !principal.starts_with("system:") {
            return Err(fail(
                StatusCode::FORBIDDEN,
                "task_binding_invalid",
                "SRC Tasks are created by the operator",
            ));
        }
        return Ok(crate::runs::src_api::generic_task_create(state, &envelope, &principal).await);
    }
    if crate::runs::src_api::supports_domain_command(&command_type) {
        let agg_type = envelope["aggregate_type"]
            .as_str()
            .filter(|value| !value.is_empty())
            .unwrap_or("task");
        if !matches!(agg_type, "task" | "thread") {
            return Err(invalid("SRC domain aggregate_type must be task"));
        }
        let task_id = if agg_type == "thread" {
            let thread_id = envelope["aggregate_id"]
                .as_str()
                .filter(|value| !value.is_empty())
                .ok_or_else(|| invalid("SRC domain command needs a Task or Thread aggregate"))?;
            let bound = match crate::runs::src_api::resolve_authorized_src_caller(&state, thread_id) {
                Ok(Some(caller)) => caller,
                Ok(None) => {
                    return Err(fail(
                        StatusCode::FORBIDDEN,
                        "task_binding_invalid",
                        "Thread is not bound to exactly one SRC Task",
                    ));
                }
                Err(error) => {
                    return Err((
                        error.status,
                        Json(json!({"error":{"code":error.code,"message":error.message,"detail":error.detail}})),
                    ));
                }
            };
            let id = bound.task_id;
            if let Some(requested_task) = envelope["payload"]["task_id"]
                .as_str()
                .filter(|value| !value.is_empty())
            {
                if requested_task != id {
                    return Err(fail(
                        StatusCode::FORBIDDEN,
                        "task_binding_invalid",
                        "requested Task does not belong to the Thread",
                    ));
                }
            }
            id
        } else {
            envelope["aggregate_id"]
                .as_str()
                .filter(|value| !value.is_empty())
                .or_else(|| {
                    envelope["payload"]["task_id"]
                        .as_str()
                        .filter(|value| !value.is_empty())
                })
                .ok_or_else(|| invalid("SRC domain command needs a Task aggregate"))?
                .to_owned()
        };
        envelope["aggregate_type"] = json!("task");
        envelope["aggregate_id"] = json!(task_id);
        let principal = platform_auth::authorize(&state,&headers,gateway_candidate.is_some(),
            &envelope,"command",&command_type,"task",&task_id)?;
        if principal.starts_with("binding:") {
            let token = headers.get(header::AUTHORIZATION)
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.strip_prefix("Bearer "))
                .unwrap_or("").trim();
            let owner = state.capability_gateway.owner_for_token(token)
                .ok_or_else(|| fail(StatusCode::UNAUTHORIZED,"capability.token_invalid",
                    "Gateway token is unknown or retired"))?;
            let bound = match crate::runs::src_api::resolve_authorized_src_caller(&state, &owner) {
                Ok(Some(caller)) => caller,
                Ok(None) => {
                    return Err(fail(
                        StatusCode::FORBIDDEN,
                        "task_binding_invalid",
                        "Thread is not bound to exactly one SRC Task",
                    ));
                }
                Err(error) => {
                    return Err((
                        error.status,
                        Json(json!({"error":{"code":error.code,"message":error.message,"detail":error.detail}})),
                    ));
                }
            };
            if bound.task_id != task_id {
                return Err(fail(StatusCode::FORBIDDEN,"task_binding_invalid",
                    "Thread does not belong to this SRC Task"));
            }
            if bound.delegation.is_some() {
                crate::runs::src_api::execution_runtime::check_delegated_scope(
                    &state,
                    &bound,
                    &command_type,
                    None,
                    &envelope["payload"],
                    true,
                )
                .map_err(|error| {
                    (
                        error.status,
                        Json(json!({"error":{"code":error.code,"message":error.message,"detail":error.detail}})),
                    )
                })?;
                if command_type == "src.workbench.todo.save"
                    && let Some(bound_todo) = bound
                        .delegation
                        .as_ref()
                        .and_then(|d| d["todo_id"].as_str())
                        .map(str::trim)
                        .filter(|v| !v.is_empty())
                {
                    let has_todo = envelope["payload"]["todo_id"]
                        .as_str()
                        .or_else(|| envelope["payload"]["payload"]["todo_id"].as_str())
                        .is_some_and(|v| !v.trim().is_empty());
                    if !has_todo && let Some(obj) = envelope["payload"].as_object_mut() {
                        obj.insert("todo_id".to_owned(), json!(bound_todo));
                    }
                }
                if let Some(actor) = envelope["actor"].as_object_mut() {
                    actor.entry("thread_id").or_insert_with(|| json!(owner));
                    actor.insert("subthread_id".to_owned(), json!(owner));
                }
            }
        }
        let agent_allowed_command = crate::runs::src_api::agent_allowed_command(&command_type);
        if !agent_allowed_command && !principal.starts_with("operator:") && !principal.starts_with("system:") {
            return Err(fail(StatusCode::FORBIDDEN,"policy_denied",
                "SRC domain command is restricted for safety"));
        }
        return Ok(crate::runs::src_api::generic_domain_command(state,&envelope,&principal).await);
    }
    if competition_api::supports_command(&command_type) || command_type.starts_with("ext.") || crate::extensions::supports_lifecycle_command(&command_type) || agent_dispatch::command_supported(&command_type)
        || matches!(command_type.as_str(), "effect.retry" | "effect.cancel") {
        let expected = match envelope.get("expected_version") {
            None | Some(Value::Null) => None,
            Some(value) => Some(
                value
                    .as_u64()
                    .ok_or_else(|| invalid("expected_version must be a nonnegative integer"))?,
            ),
        };
        let idempotency_key = match envelope.get("idempotency_key") {
            None | Some(Value::Null) => None,
            Some(Value::String(value)) if value.is_empty() => None,
            Some(Value::String(value)) if !value.is_empty() => {
                Some(value.clone())
            }
            Some(_) => {
                return Err(invalid(
                    "idempotency_key must be a string",
                ));
            }
        };
        let (scope_type,scope_id) = if crate::extensions::supports_lifecycle_command(&command_type) {
            let target = envelope["payload"]["extension_id"].as_str().filter(|value| !value.is_empty())
                .or_else(|| envelope["aggregate_id"].as_str().filter(|value| !value.is_empty()))
                .or_else(|| (command_type == "extension.install").then_some("registry"))
                .ok_or_else(|| invalid("Extension lifecycle command requires extension_id"))?;
            ("extension",target.to_owned())
        } else if agent_dispatch::command_supported(&command_type) {
            agent_dispatch::command_scope(&command_type,&envelope)?
        } else if matches!(command_type.as_str(), "effect.retry" | "effect.cancel") {
            ("effect", envelope["payload"]["effect_id"].as_str()
                .or_else(||envelope["aggregate_id"].as_str()).unwrap_or("").to_owned())
        } else if competition_api::is_binding_command(&command_type) {
            let binding_id = envelope["payload"]["binding_id"].as_str().filter(|value| !value.is_empty())
                .or_else(|| if envelope["aggregate_type"] == "run_binding" {
                    envelope["aggregate_id"].as_str().filter(|value| !value.is_empty())
                } else { None });
            if let Some(binding_id) = binding_id {
                ("run_binding", binding_id.to_owned())
            } else {
                let competition_id = envelope["payload"]["competition_id"].as_str()
                    .filter(|value| !value.is_empty())
                    .or_else(|| if envelope["aggregate_type"] == "competition" {
                        envelope["aggregate_id"].as_str().filter(|value| !value.is_empty())
                    } else { None }).unwrap_or("");
                ("competition", competition_id.to_owned())
            }
        } else {
            let scope_type = if command_type.starts_with("connection.")
                || command_type == "competition.connection.create" { "connection" }
                else if command_type.starts_with("ext.") { "extension" }
                else { "competition" };
            let scope_id = if scope_type == "connection" {
                envelope["payload"]["connection_id"].as_str().or_else(||envelope["aggregate_id"].as_str())
            } else if scope_type == "competition" {
                envelope["payload"]["competition_id"].as_str().or_else(|| {
                    if envelope["aggregate_type"] == "competition" { envelope["aggregate_id"].as_str() } else { None }
                })
            } else { envelope["aggregate_id"].as_str() }.unwrap_or("").to_owned();
            (scope_type,scope_id)
        };
        if agent_dispatch::command_supported(&command_type) || crate::extensions::supports_lifecycle_command(&command_type) {
            envelope["aggregate_type"] = json!(scope_type);
            envelope["aggregate_id"] = json!(scope_id);
        }
        let principal = platform_auth::authorize(
            &state,
            &headers,
            gateway_candidate.is_some(),
            &envelope,
            "command",
            &command_type,
            scope_type,
            &scope_id,
        )?;
        if crate::chat_subagents::supports_command(&command_type) {
            require_subagent_caller(&state,&headers,&principal,&scope_id)?;
        }
        if command_type == "competition.submission.manual_override" && !principal.starts_with("operator:") {
            return Err(fail(StatusCode::FORBIDDEN,"competition.submission.manual_override_operator_required",
                "Manual submission override requires an operator"));
        }
        if envelope.get("payload").is_none() {
            envelope["payload"] = json!({});
        }
        if !envelope["payload"].is_object() {
            return Err(invalid("payload must be an object"));
        }
        if envelope
            .get("command_id")
            .is_some_and(|value| !value.is_string() && !value.is_null())
        {
            return Err(invalid("command_id must be a string"));
        }
        if envelope["command_id"].as_str().is_none_or(str::is_empty) {
            envelope["command_id"] = json!(new_id("cmd"));
        }
        let command_id = envelope["command_id"].as_str().unwrap();
        let module=crate::business::operation_module(&state,&command_type,&envelope).map_err(|e|e.failure())?;
        let _dispatch=crate::business::dispatch_lock(&state,module).lock().await;
        crate::business::require_operation(&state,&command_type,&envelope).map_err(|e|e.failure())?;
        let digest = command_fingerprint(&envelope);
        let semantic = digest.clone();
        let key_id = idempotency_key
            .as_ref()
            .map(|key| idempotency_index(key));
        let planned_effect = matches!(command_type.as_str(),"effect.retry" | "effect.cancel")
            && competition_api::is_submission_or_binding_effect(&state,&scope_id).map_err(storage)?;
        let planned = planned_competition_command(&command_type) || planned_effect;
        if planned {
            if let Some(prior) = planned_existing(&state,command_id,&digest,&semantic,key_id.as_deref())? {
                return Ok(prior);
            }
        } else {
        command_journal::recover(&state,command_id).await?;
        if let Some(key_id) = key_id.as_ref() {
            if let Some(prior) = state.store.get(IDEMPOTENCY_KEYS, key_id).map_err(storage)? {
                if prior["semantic_fingerprint"] != semantic {
                    return Err(fail(
                        StatusCode::CONFLICT,
                        "command.idempotency_conflict",
                        "idempotency_key was used with different content",
                    ));
                }
                let prior_id = prior["command_id"].as_str().unwrap_or("");
                command_journal::recover(&state,prior_id).await?;
                if prior_id != command_id {
                    let stored = state
                        .store
                        .get(COMMANDS, prior_id)
                        .map_err(storage)?
                        .ok_or_else(|| {
                            fail(
                                StatusCode::CONFLICT,
                                "command.outcome_unknown",
                                "Prior command outcome must be inspected before retry",
                            )
                        })?;
                    let mut receipt = stored["response"]["receipt"].clone();
                    receipt["deduplicated"] = json!(true);
                    let status =
                        StatusCode::from_u16(stored["status"].as_u64().unwrap_or(200) as u16)
                            .unwrap_or(StatusCode::OK);
                    return Ok((status, Json(json!({"receipt":receipt}))).into_response());
                }
            }
        }
        let existing = state.store.get(COMMANDS, command_id).map_err(storage)?;
        if let Some(existing) = existing.as_ref() {
            if existing["request_fingerprint"] != digest {
                return Err(fail(
                    StatusCode::CONFLICT,
                    "command.idempotency_conflict",
                    format!("command '{command_id}' was already used with different content"),
                ));
            }
            let mut receipt = existing["response"]["receipt"].clone();
            receipt["deduplicated"] = json!(true);
            let status = StatusCode::from_u16(existing["status"].as_u64().unwrap_or(200) as u16)
                .unwrap_or(StatusCode::OK);
            return Ok((status, Json(json!({"receipt":receipt}))).into_response());
        }
        if state
            .store
            .get(COMMAND_CLAIMS, command_id)
            .map_err(storage)?
            .is_some()
        {
            return Err(fail(
                StatusCode::CONFLICT,
                "command.outcome_unknown",
                "Prior command outcome must be inspected before retry",
            ));
        }
        }
        let aggregate_type = envelope["aggregate_type"]
            .as_str()
            .filter(|value| !value.is_empty())
            .unwrap_or(scope_type);
        let aggregate_id = envelope["aggregate_id"]
            .as_str()
            .filter(|value| !value.is_empty())
            .unwrap_or(&scope_id);
        let before = aggregate_version(&state, aggregate_type, aggregate_id)?;
        if !planned && expected.is_some_and(|version| version != before) {
            if agent_dispatch::command_supported(&command_type) {
                return persist_thread_version_conflict(&state,&envelope,&digest,&semantic,key_id.as_deref(),before,expected.unwrap());
            }
            return Err(fail(
                StatusCode::CONFLICT,
                "command.expected_version_conflict",
                format!("Expected version {:?}, current version {before}", expected),
            ));
        }
        let claim = json!({"command_id":command_id,"request_fingerprint":digest,"state":"accepted","created_at":now(),"event_context":{"actor_id":envelope["actor"]["id"],"causation_id":command_id,"correlation_id":envelope["payload"]["correlation_id"].as_str().filter(|v|!v.is_empty()).unwrap_or(command_id),"idempotency_key":envelope["idempotency_key"]}});
        let accepted = json!({"version_before":before,"request":{"command_type":command_type,"aggregate_type":aggregate_type,"aggregate_id":aggregate_id,
                "actor":envelope["actor"],"idempotency_key":envelope["idempotency_key"]},"principal":principal,
            "request_fingerprint":digest,"status":202,"response":{"receipt":{
                "schema_version":1,"command_id":command_id,"receipt_id":new_id("rcpt"),"state":"accepted",
                "aggregate":{"type":aggregate_type,"id":aggregate_id},"run_id":null,"event_cursor":null,
                "deduplicated":false,"submitted_at":now(),"next":["receipt","events","wait"],
                "output":{},"effect_ids":[],"error":null}}});
        let keyed = key_id.as_ref().map(|_| json!({"command_id":command_id,"semantic_fingerprint":semantic,"created_at":now()}));
        let prepared_effect = if planned { None } else { platform_effects::prepare(&envelope) };
        let mut conditions = vec![(COMMAND_CLAIMS, command_id, None)];
        let mut records = vec![(COMMAND_CLAIMS, command_id, &claim)];
        if !planned {
            conditions.push((COMMANDS,command_id,None));
            records.push((COMMANDS,command_id,&accepted));
        }
        if let (Some(key_id), Some(keyed)) = (key_id.as_deref(), keyed.as_ref()) {
            conditions.push((IDEMPOTENCY_KEYS, key_id, None));
            records.push((IDEMPOTENCY_KEYS, key_id, keyed));
        }
        if let Some(prepared) = &prepared_effect { records.extend(prepared.records()); }
        let admitted = if !planned && crate::extensions::supports_lifecycle_command(&command_type) && command_type != "extension.propose_event" {
            let event = domain_events::event(&envelope,aggregate_type,aggregate_id,
                &format!("core.{command_type}.accepted"),"platform.extensions",json!({"extension_id":scope_id,"command_type":command_type}));
            domain_events::commit(&state,&conditions,&records,&[event])?.is_some()
        } else if !planned && settings_dispatch::supports(&command_type) {
            let event = settings_dispatch::event(&envelope,aggregate_type,aggregate_id).unwrap();
            domain_events::commit(&state,&conditions,&records,&[event])?.is_some()
        } else if !planned && crate::runtime_api::requested_event(&envelope,aggregate_type,aggregate_id).is_some() {
            let event = crate::runtime_api::requested_event(&envelope,aggregate_type,aggregate_id).unwrap();
            domain_events::commit(&state,&conditions,&records,&[event])?.is_some()
        } else if !planned { state.store.put_many_if(&conditions,&records).map_err(storage)? } else { true };
        if !admitted {
            if let Some(prior) = state.store.get(COMMANDS, command_id).map_err(storage)? {
                if prior["request_fingerprint"] != digest {
                    return Err(fail(StatusCode::CONFLICT, "command.idempotency_conflict",
                        "command_id was already used with different content"));
                }
                let mut receipt = prior["response"]["receipt"].clone();
                receipt["deduplicated"] = json!(true);
                let status = StatusCode::from_u16(prior["status"].as_u64().unwrap_or(200) as u16)
                    .unwrap_or(StatusCode::OK);
                return Ok((status, Json(json!({"receipt":receipt}))).into_response());
            }
            if let Some(key_id) = key_id.as_deref() {
                if state.store.get(IDEMPOTENCY_KEYS, key_id).map_err(storage)?
                    .is_some_and(|prior| prior["semantic_fingerprint"] != semantic) {
                    return Err(fail(StatusCode::CONFLICT, "command.idempotency_conflict",
                        "idempotency_key was used with different content"));
                }
            }
            return Err(fail(StatusCode::CONFLICT, "command.outcome_unknown",
                "Prior command outcome must be inspected before retry"));
        }
        let effect = prepared_effect.as_ref().map(|prepared|prepared.effect_id.clone());
        let processing = effect.as_deref().map(|effect_id|platform_effects::mark_processing(&state,effect_id)).transpose();
        let dispatch_result: Result<Response,Failure> = async {
            processing?;
        let response = if command_type == "effect.retry" {
            if competition_api::is_lease_effect(&state, &scope_id).map_err(storage)? {
                competition_api::control_lease_effect(&state, &envelope).await
            } else if competition_api::is_connection_effect(&state,&scope_id).map_err(storage)? {
                competition_api::control_connection_effect(&state,&envelope).await
            } else if planned_effect {
                let mut delegated = envelope.clone();
                delegated["_verified_scope"] = json!(principal);
                delegated["_planned_admission"] = json!({"claim":claim,"key_id":key_id,
                    "keyed":keyed,"semantic_fingerprint":semantic,
                    "expected_version":expected});
                competition_api::control_submission_or_binding_effect(&state,&delegated).await
            } else {
                platform_effects::retry(&state, &scope_id).await?.into_response()
            }
        } else if command_type == "effect.cancel" {
            if competition_api::is_lease_effect(&state, &scope_id).map_err(storage)? {
                competition_api::control_lease_effect(&state, &envelope).await
            } else if competition_api::is_connection_effect(&state,&scope_id).map_err(storage)? {
                competition_api::control_connection_effect(&state,&envelope).await
            } else if planned_effect {
                let mut delegated = envelope.clone();
                delegated["_verified_scope"] = json!(principal);
                delegated["_planned_admission"] = json!({"claim":claim,"key_id":key_id,
                    "keyed":keyed,"semantic_fingerprint":semantic,
                    "expected_version":expected});
                competition_api::control_submission_or_binding_effect(&state,&delegated).await
            } else {
                platform_effects::cancel(&state, &scope_id, envelope["payload"]["confirm"] == true)?.into_response()
            }
        } else if competition_api::supports_command(&command_type) {
            let mut delegated = envelope.clone();
            delegated["_verified_scope"] = json!(principal);
            if planned {
                delegated["_planned_admission"] = json!({"claim":claim,"key_id":key_id,
                    "keyed":keyed,"semantic_fingerprint":semantic,
                    "expected_version":expected});
            }
            competition_api::dispatch_command_envelope(state.clone(), delegated).await
        } else if crate::extensions::supports_lifecycle_command(&command_type) {
            let mut delegated = envelope.clone();
            delegated["_verified_scope"] = json!(principal);
            let response = crate::extensions::dispatch_lifecycle_command(&state,&delegated).await;
            normalize_extension_response(&state,&envelope,response).await?
        } else if agent_dispatch::command_supported(&command_type) {
            let mut delegated = envelope.clone();
            delegated["_verified_scope"] = json!(principal);
            agent_dispatch::command(state.clone(), &delegated).await.unwrap_or_else(|error|error.into_response())
        } else {
            let response = crate::extensions::dispatch_command(&state, &envelope)
                .await
                .ok_or_else(|| {
                    fail(
                        StatusCode::NOT_IMPLEMENTED,
                        "capability.unavailable",
                        "Extension command is not registered",
                    )
                })?;
            normalize_extension_response(&state, &envelope, response).await?
        };
            Ok(response)
        }.await;
        let response = dispatch_result.unwrap_or_else(|error|error.into_response());
        let response = if !planned {
            command_journal::record(&state,&envelope,effect.as_deref(),&principal,before,response).await?
        } else { response };
        let response = if let Some(effect_id) = effect.as_ref() {
            platform_effects::finish_response(&state, effect_id, response).await?
        } else {
            response
        };
        if planned && state.store.get(COMMANDS,command_id).map_err(storage)?.is_none() {
            if let Some(prior) = planned_existing(&state,command_id,&digest,&semantic,key_id.as_deref())? {
                return Ok(prior);
            }
        }
        let response =
            settle_command_response(&state, &envelope, &digest, &semantic, key_id.as_deref(),
                effect.as_deref(), &principal, planned, response)
                .await?;
        if response.status().is_success() {
            if let Some(stored) = state.store.get(COMMANDS, command_id).map_err(storage)? {
                let actual_type = stored["response"]["receipt"]["aggregate"]["type"]
                    .as_str()
                    .unwrap_or(aggregate_type);
                let actual_id = stored["response"]["receipt"]["aggregate"]["id"]
                    .as_str()
                    .unwrap_or(aggregate_id);
                let prior = if actual_type == aggregate_type && actual_id == aggregate_id {
                    before
                } else {
                    0
                };
                save_aggregate_version(&state, actual_type, actual_id, prior)?;
            }
        }
        if !planned {
            let claim_state = if response.status() == StatusCode::ACCEPTED {"accepted"}
                else if response.status().is_success() {"completed"} else {"failed"};
            state.store.put(COMMAND_CLAIMS,command_id,&json!({"command_id":command_id,
                "request_fingerprint":digest,"state":claim_state,"updated_at":now()}))
                .map_err(storage)?;
            command_journal::settled(&state,command_id)?;
        }
        return Ok(response);
    }
    match command_type.as_str() {
        "module.enable" | "module.disable" => {
            if map.get("aggregate_type").and_then(Value::as_str) != Some("module")
                || map
                    .get("aggregate_id")
                    .and_then(Value::as_str)
                    .is_none_or(str::is_empty)
            {
                return Err(invalid(
                    "module commands need aggregate_type=module and aggregate_id",
                ));
            }
            let principal = platform_auth::authorize(
                &state,
                &headers,
                gateway_candidate.is_some(),
                &envelope,
                "command",
                &command_type,
                "module",
                map["aggregate_id"].as_str().unwrap_or(""),
            )?;
            let mut delegated = envelope.clone();
            delegated["_verified_scope"] = json!(principal);
            let (status, response) = run_module_command(&state, &delegated)?;
            if command_type=="module.enable" {if let Some(module)=crate::business::Module::from_descriptor(delegated["aggregate_id"].as_str().unwrap_or("")){crate::business::retry(state.clone(),module);}}
            Ok((status, Json(response)).into_response())
        }
        other if other.starts_with("conversation.") => Err(fail(
            StatusCode::NOT_IMPLEMENTED,
            "capability.unavailable",
            format!(
                "{other} 通过 /api/threads/{{id}}/commands 提交；此通用入口接受已迁移的模块和比赛命令。"
            ),
        )),
        other => Err(fail(
            StatusCode::NOT_IMPLEMENTED,
            "capability.unavailable",
            format!("{other} 属于尚未迁移的执行后端或平台命令。"),
        )),
    }
}

const QUERY_FIELDS: [&str; 6] = [
    "query_id",
    "query_type",
    "aggregate_type",
    "aggregate_id",
    "actor",
    "params",
];
async fn query(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    gateway_candidate: Option<Extension<crate::access_api::GatewayCandidate>>,
    body: Option<Json<Value>>,
) -> Result<Response, Failure> {
    let invalid = |message: &str| {
        fail(
            StatusCode::BAD_REQUEST,
            "platform.request.invalid",
            message.to_owned(),
        )
    };
    let Some(Json(mut envelope)) = body else {
        return Err(invalid("Body must be a query envelope"));
    };
    let Some(map) = envelope.as_object() else {
        return Err(invalid("Body must be a query envelope"));
    };
    if let Some(field) = map.keys().find(|key| !QUERY_FIELDS.contains(&key.as_str())) {
        return Err(invalid(&format!("unknown field: {field}")));
    }
    let kind = envelope["query_type"]
        .as_str()
        .ok_or_else(|| invalid("query_type is required"))?;
    let kind = kind.to_owned();
    crate::business::require_operation(&state,&kind,&envelope).map_err(|e|e.failure())?;
    if matches!(kind.as_str(), "task.get" | "task.list") {
        return task_dispatch::query(&state, &headers, gateway_candidate.is_some(), envelope);
    }
    if src_queries::supports(&kind) {
        return src_queries::query(&state, &headers, gateway_candidate.is_some(), envelope);
    }
    if envelope.get("params").is_none() { envelope["params"] = json!({}); }
    if !envelope["params"].is_object() { return Err(invalid("params must be an object")); }
    let (scope_type,scope_id) = if agent_dispatch::query_supported(&kind) {
        agent_dispatch::query_scope(&kind,&envelope)?
    } else {
        let scope_type = if kind.starts_with("connection.") { "connection" }
            else if kind.starts_with("extension.") { "extension" }
            else { "competition" };
        let scope_id = if matches!(kind.as_str(),"connection.list" | "competition.list" | "extension.registry" | "extension.list") {
            "".to_owned()
        } else {
            let field = if scope_type == "extension" { "extension_id" } else { "competition_id" };
            envelope["params"][field].as_str().or_else(||envelope["aggregate_id"].as_str()).unwrap_or("").to_owned()
        };
        (scope_type,scope_id)
    };
    if agent_dispatch::query_supported(&kind) {
        envelope["aggregate_type"] = json!(scope_type);
        envelope["aggregate_id"] = json!(scope_id);
    }
    let principal = platform_auth::authorize(
        &state,
        &headers,
        gateway_candidate.is_some(),
        &envelope,
        "query",
        &kind,
        scope_type,
        &scope_id,
    )?;
    if crate::chat_subagents::supports_query(&kind) {
        require_subagent_caller(&state,&headers,&principal,&scope_id)?;
    }
    if envelope
        .get("query_id")
        .is_some_and(|value| !value.is_string() && !value.is_null())
    {
        return Err(invalid("query_id must be a string"));
    }
    if envelope["query_id"].as_str().is_none_or(str::is_empty) {
        envelope["query_id"] = json!(new_id("qry"));
    }
    if competition_api::supports_query(&kind) {
        return Ok(competition_api::query_envelope(&state, &envelope));
    }
    if agent_dispatch::query_supported(&kind) {
        let result = agent_dispatch::query(&state,&envelope,&principal).await?;
        let cursor = result.get("next_cursor").cloned().unwrap_or(Value::Null);
        return Ok(Json(json!({"result":{"query_id":envelope["query_id"],"query_type":kind,"result":result,"cursor":cursor}})).into_response());
    }
    let extension_response = if let Some(response) = crate::extensions::dispatch_lifecycle_query(&state,&envelope).await {
        Some(response)
    } else { crate::extensions::dispatch_query(&state,&envelope) };
    if let Some(response) = extension_response {
        if !response.status().is_success() {
            return Ok(response);
        }
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .map_err(storage)?;
        let raw: Value = serde_json::from_slice(&bytes).map_err(storage)?;
        return Ok(Json(json!({"result":{"query_id":envelope["query_id"],"query_type":kind,"result":raw,"cursor":null}})).into_response());
    }
    Err(fail(
        StatusCode::NOT_IMPLEMENTED,
        "capability.unavailable",
        format!("{kind} query is not available"),
    ))
}

async fn receipt_effects(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<Value>, Failure> {
    let mut result = platform_effects::receipt_effects(&state, &id)?.0;
    result["effects"].as_array_mut().unwrap().extend(state.store.list("src_domain_effect_receipts")
        .map_err(storage)?.into_iter().filter(|row|row["command_id"] == id));
    Ok(Json(result))
}

#[derive(serde::Deserialize)]
struct EffectQuery {
    state: Option<String>,
    limit: Option<usize>,
}

async fn effects(
    State(state): State<Arc<AppState>>,
    Query(query): Query<EffectQuery>,
) -> Result<Json<Value>, Failure> {
    let mut result = platform_effects::effects(&state, query.state.as_deref(), 1000)?.0;
    let allowed: Vec<&str> = query.state.as_deref().unwrap_or("").split(',').map(str::trim)
        .filter(|state| !state.is_empty()).collect();
    result["effects"].as_array_mut().unwrap().extend(state.store.list("src_domain_effect_receipts")
        .map_err(storage)?.into_iter().filter(|row|allowed.is_empty() || allowed.iter().any(|state|row["state"] == *state)));
    let rows = result["effects"].as_array_mut().unwrap();
    rows.sort_by(|a,b|b["created_at"].as_str().cmp(&a["created_at"].as_str()));
    rows.truncate(query.limit.unwrap_or(200).clamp(1,1000));
    Ok(Json(result))
}

async fn effect(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<Value>, Failure> {
    if let Some(effect) = state.store.get("src_domain_effect_receipts", &id).map_err(storage)? {
        return Ok(Json(json!({"effect":effect})));
    }
    platform_effects::effect(&state, &id)
}

fn lease_effect_envelope(id: &str, action: &str, body: &Value) -> Value {
    let mut envelope = json!({"command_type":format!("effect.{action}"),
        "aggregate_type":"effect","aggregate_id":id,
        "actor":{"kind":"operator","id":"local-user"},
        "payload":{"effect_id":id,"confirm":body["confirm"] == true}});
    for key in ["command_id", "idempotency_key"] {
        if let Some(value) = body[key].as_str().filter(|value| !value.is_empty()) {
            envelope[key] = json!(value);
        }
    }
    envelope
}

async fn retry_effect(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: Option<Json<Value>>,
) -> Result<Response, Failure> {
    if competition_api::is_lease_effect(&state, &id).map_err(storage)? {
        let payload = body.map(|Json(value)| value).unwrap_or_else(|| json!({}));
        return command(State(state), headers, None,
            Some(Json(lease_effect_envelope(&id, "retry", &payload)))).await;
    }
    let _dispatch = COMMAND_DISPATCH_LOCK.get_or_init(|| tokio::sync::Mutex::new(())).lock().await;
    Ok(platform_effects::retry(&state, &id).await?.into_response())
}
async fn cancel_effect(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: Option<Json<Value>>,
) -> Result<Response, Failure> {
    if competition_api::is_lease_effect(&state, &id).map_err(storage)? {
        let payload = body.map(|Json(value)| value).unwrap_or_else(|| json!({}));
        return command(State(state), headers, None,
            Some(Json(lease_effect_envelope(&id, "cancel", &payload)))).await;
    }
    let _dispatch = COMMAND_DISPATCH_LOCK.get_or_init(|| tokio::sync::Mutex::new(())).lock().await;
    Ok(platform_effects::cancel(
        &state,
        &id,
        body.is_some_and(|Json(value)| value["confirm"] == true),
    )?.into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state() -> (tempfile::TempDir, Arc<AppState>) {
        let directory = tempfile::tempdir().unwrap();
        let store = crate::store::Store::open(directory.path().join("muteki.sqlite")).unwrap();
        store
            .put("system", "state_dir", &json!(directory.path()))
            .unwrap();
        (directory, Arc::new(AppState::new(store)))
    }

    #[test]
    fn redaction_hides_secrets_home_and_paths() {
        let home = std::env::var("HOME").unwrap();
        let text = format!("token=abc123 at {home}/work/x.log and /var/lib/muteki/state.db");
        let clean = redact(&text);
        assert!(
            !clean.contains("abc123") && !clean.contains(&home),
            "{clean}"
        );
        assert!(
            clean.contains("token=[REDACTED]")
                && clean.contains("state.db")
                && !clean.contains("/var/lib"),
            "{clean}"
        );
    }

    #[test]
    fn maintenance_lists_only_stale_temporary_files() {
        let (directory, _) = state();
        let stale = directory.path().join("nested/old.tmp");
        std::fs::create_dir_all(stale.parent().unwrap()).unwrap();
        std::fs::write(&stale, b"x").unwrap();
        std::fs::write(directory.path().join("fresh.tmp"), b"x").unwrap();
        let old = std::fs::File::options().write(true).open(&stale).unwrap();
        old.set_modified(SystemTime::now() - Duration::from_secs(7200))
            .unwrap();
        let ids: Vec<_> = temporary_files(directory.path())
            .into_iter()
            .map(|c| c["candidate_id"].clone())
            .collect();
        assert_eq!(ids, vec![json!("temp:nested/old.tmp")]);
    }

    #[tokio::test]
    async fn module_commands_persist_receipts_and_flags() {
        let (_directory, state) = state();
        let body = Some(Json(json!({"command_id": "cmd-a"})));
        let response = module_action(
            State(state.clone()),
            Path(("builtin.conversation".into(), "disable".into())),
            body,
        )
        .await
        .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(module_records(&state).unwrap()[0]["state"], "disabled");
        assert_eq!(
            state.store.command_receipt("cmd-a").unwrap().unwrap()["output"]["state"],
            "disabled"
        );
        let src = run_module_command(&state, &json!({"command_type":"module.disable","aggregate_type":"module","aggregate_id":"builtin.src","payload":{}})).unwrap();
        assert_eq!(src.1["receipt"]["output"]["changed"], true);
        assert_eq!(module_records(&state).unwrap()[4]["state"], "disabled");
        let unknown = run_module_command(&state, &json!({"command_type":"module.enable","aggregate_type":"module","aggregate_id":"nope","payload":{}})).unwrap_err();
        assert_eq!(
            (unknown.0, unknown.1.0["error"]["code"].clone()),
            (StatusCode::NOT_FOUND, json!("module.not_found"))
        );
        let conflict = run_module_command(&state, &json!({"command_id":"cmd-a","command_type":"module.enable","aggregate_type":"module","aggregate_id":"builtin.conversation","payload":{}})).unwrap_err();
        assert_eq!(conflict.0, StatusCode::CONFLICT);
    }

    #[tokio::test]
    async fn competition_command_envelope_routes_payload_and_queries_live_projection() {
        let (directory, state) = state();
        state.store.put("competitions","comp-test",&json!({"competition_id":"comp-test","connection_id":"pconn-test","scheduler_state":"stopped","archived":false})).unwrap();
        let envelope = json!({"command_id":"cmd-policy-test","command_type":"policy.update","aggregate_type":"competition","aggregate_id":"comp-test","payload":{"competition_id":"comp-test","automation_mode":"observe"}});
        let response = command(
            State(state.clone()),
            HeaderMap::new(),
            None,
            Some(Json(envelope.clone())),
        )
        .await
        .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            state
                .store
                .get("competition_policies_live", "comp-test")
                .unwrap()
                .unwrap()["automation_mode"],
            "observe"
        );
        let duplicate = command(
            State(state.clone()),
            HeaderMap::new(),
            None,
            Some(Json(envelope.clone())),
        )
        .await
        .unwrap();
        let bytes = axum::body::to_bytes(duplicate.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(
            serde_json::from_slice::<Value>(&bytes).unwrap()["receipt"]["deduplicated"],
            true
        );
        let mut changed = envelope;
        changed["payload"]["automation_mode"] = json!("autonomous");
        assert_eq!(
            command(
                State(state.clone()),
                HeaderMap::new(),
                None,
                Some(Json(changed))
            )
            .await
            .unwrap_err()
            .0,
            StatusCode::CONFLICT
        );
        let secret_command = json!({"command_id":"cmd-connection-test","command_type":"connection.create","aggregate_type":"connection","payload":{
            "platform_kind":"ctfd","base_url":"https://example.invalid","account_key":"fixture","credential":"sensitive-token-value"}});
        assert_eq!(
            command(
                State(state.clone()),
                HeaderMap::new(),
                None,
                Some(Json(secret_command))
            )
            .await
            .unwrap()
            .status(),
            StatusCode::OK
        );
        let stored = state
            .store
            .get(COMMANDS, "cmd-connection-test")
            .unwrap()
            .unwrap();
        assert!(!stored.to_string().contains("sensitive-token-value"));
        assert_eq!(stored["request_fingerprint"].as_str().unwrap().len(), 64);
        let key = directory.path().join("control/platform/idempotency.key");
        assert!(!key.exists());
        let projection = query(
            State(state),
            HeaderMap::new(),
            None,
            Some(Json(
                json!({"query_type":"competition.snapshot","aggregate_id":"comp-test"}),
            )),
        )
        .await
        .unwrap();
        let bytes = axum::body::to_bytes(projection.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(
            serde_json::from_slice::<Value>(&bytes).unwrap()["result"]["result"]["policy"]["automation_mode"],
            "observe"
        );
    }

    #[tokio::test]
    async fn agent_actor_requires_live_grant_binding_operation_and_scope() {
        let (_directory, state) = state();
        state.store.put("competitions","comp-test",&json!({"competition_id":"comp-test","connection_id":"pconn-test","scheduler_state":"stopped","archived":false})).unwrap();
        let token = state
            .capability_gateway
            .issue("thread-test", "test")
            .unwrap();
        let digest = state
            .capability_gateway
            .token_digest("thread-test")
            .unwrap();
        let binding = json!({"binding_id":"bind-test","binding_version":1,"thread_id":"thread-test","principal_id":"system",
            "allowed_commands":["policy.update","conversation.project.create"],"allowed_queries":["competition.snapshot","project.list"],
            "resource_scopes":["competition:comp-test","project:*"],"revoked_at":null});
        state
            .store
            .put(
                crate::capabilities::BINDINGS,
                "thread-test\0system",
                &json!({"versions":[binding]}),
            )
            .unwrap();
        crate::capabilities::issue_grant(&state, "thread-test", &json!({
            "agent_session_id":"test-session","instance_id":"test","adapter_id":"pi.rpc"
        }), &digest).unwrap();
        let actor = json!({"kind":"agent","id":"system","binding_id":"bind-test","binding_version":1,"thread_id":"thread-test"});
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let service_state = state.clone();
        let server = tokio::spawn(async move {
            axum::serve(listener, router().with_state(service_state))
                .await
                .unwrap()
        });
        let base = format!("http://{address}");
        let client = reqwest::Client::new();
        let snapshot=client.post(format!("{base}/api/queries")).bearer_auth(&token)
            .json(&json!({"query_type":"competition.snapshot","aggregate_id":"comp-test","actor":actor})).send().await.unwrap();
        assert_eq!(snapshot.status(), StatusCode::OK);
        let projects=client.post(format!("{base}/api/queries")).bearer_auth(&token)
            .json(&json!({"query_type":"project.list","actor":actor})).send().await.unwrap();
        assert_eq!(projects.status(), StatusCode::OK);
        let project_created=client.post(format!("{base}/api/commands")).bearer_auth(&token)
            .json(&json!({"command_type":"conversation.project.create","aggregate_type":"project",
                "actor":actor,"payload":{"name":"Bound Agent Project"}})).send().await.unwrap();
        assert_eq!(project_created.status(), StatusCode::OK);
        let applied=client.post(format!("{base}/api/commands")).bearer_auth(&token)
            .json(&json!({"command_type":"policy.update","aggregate_type":"competition","aggregate_id":"comp-test",
                "actor":actor,"payload":{"competition_id":"comp-test","automation_mode":"observe"}})).send().await.unwrap();
        assert_eq!(applied.status(), StatusCode::OK);
        let denied=client.post(format!("{base}/api/queries")).bearer_auth(&token)
            .json(&json!({"query_type":"competition.snapshot","aggregate_id":"comp-other","actor":actor})).send().await.unwrap();
        assert_eq!(denied.status(), StatusCode::FORBIDDEN);
        assert_eq!(
            denied.json::<Value>().await.unwrap()["error"]["code"],
            "capability.scope_denied"
        );
        let mut revoked = binding;
        revoked["revoked_at"] = json!(now());
        state
            .store
            .put(
                crate::capabilities::BINDINGS,
                "thread-test\0system",
                &json!({"versions":[revoked]}),
            )
            .unwrap();
        let denied=client.post(format!("{base}/api/queries")).bearer_auth(&token)
            .json(&json!({"query_type":"competition.snapshot","aggregate_id":"comp-test","actor":actor})).send().await.unwrap();
        assert_eq!(denied.status(), StatusCode::FORBIDDEN);
        assert_eq!(
            denied.json::<Value>().await.unwrap()["error"]["code"],
            "capability.binding.stale"
        );
        server.abort();
    }

    async fn exec_cmd(state: &Arc<AppState>, envelope: Value) -> (StatusCode, Value) {
        let resp = match command(State(state.clone()), HeaderMap::new(), None, Some(Json(envelope))).await {
            Ok(r) => r,
            Err(f) => f.into_response(),
        };
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        (status, serde_json::from_slice(&bytes).unwrap())
    }

    #[tokio::test]
    async fn competition_metadata_updates_admit_claim_and_key_with_the_result() {
        let (_directory,state) = state();
        state.store.put("platform_connections","conn-local",&json!({
            "connection_id":"conn-local","platform_kind":"ctfd","account_key":"before",
            "canonical_base_url":"http://example.test","historical":false,"archived":false
        })).unwrap();
        state.store.put("competitions","comp-local",&json!({
            "competition_id":"comp-local","title":"Before","historical":false,
            "archived":false
        })).unwrap();
        for (kind,aggregate,id,payload,key) in [
            ("connection.update","connection","conn-local",
                json!({"connection_id":"conn-local","account_key":"after"}),"connection-update-key"),
            ("competition.update","competition","comp-local",
                json!({"competition_id":"comp-local","title":"After"}),"competition-update-key"),
        ] {
            let envelope = json!({"command_id":format!("cmd-{kind}"),"idempotency_key":key,
                "command_type":kind,"aggregate_type":aggregate,"aggregate_id":id,
                "expected_version":0,"payload":payload});
            let (status,first) = exec_cmd(&state,envelope.clone()).await;
            assert_eq!(status,StatusCode::OK,"{kind}: {first}");
            assert_eq!(first["receipt"]["state"],"completed");
            let command_id = first["receipt"]["command_id"].as_str().unwrap();
            assert!(state.store.get(COMMANDS,command_id).unwrap().is_some());
            assert!(state.store.get(COMMAND_CLAIMS,command_id).unwrap().is_some());
            assert_eq!(state.store.list(IDEMPOTENCY_KEYS).unwrap().len(),
                if kind == "connection.update" {1} else {2});
            let mut replay = envelope;
            replay["command_id"] = json!(format!("replay-{kind}"));
            let (status,replayed) = exec_cmd(&state,replay).await;
            assert_eq!(status,StatusCode::OK,"{kind}: {replayed}");
            assert_eq!(replayed["receipt"]["deduplicated"],true);
            assert_eq!(replayed["receipt"]["receipt_id"],first["receipt"]["receipt_id"]);
        }
        assert_eq!(state.store.get("platform_connections","conn-local").unwrap().unwrap()["account_key"],"after");
        assert_eq!(state.store.get("competitions","comp-local").unwrap().unwrap()["title"],"After");
        assert_eq!(state.store.list("competition_events_live").unwrap().iter()
            .filter(|event|event["event_type"]=="competition.updated").count(),1);
    }

    async fn exec_qry(state: &Arc<AppState>, envelope: Value) -> (StatusCode, Value) {
        let resp = match query(State(state.clone()), HeaderMap::new(), None, Some(Json(envelope))).await {
            Ok(r) => r,
            Err(f) => f.into_response(),
        };
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        (status, serde_json::from_slice(&bytes).unwrap())
    }

    #[tokio::test]
    async fn unified_conversation_commands_queries_version_and_idempotency() {
        let (directory, state) = state();
        let repo = directory.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        for args in [
            &["init", "-b", "main"][..],
            &["config", "user.name", "Fixture"][..],
            &["config", "user.email", "fixture@example.invalid"][..],
            &["commit", "--allow-empty", "-m", "Initial"][..],
        ] {
            assert!(
                crate::host_process::std_command("git")
                    .arg("-C")
                    .arg(&repo)
                    .args(args)
                    .status()
                    .unwrap()
                    .success()
            );
        }

        let (status, proj) = exec_cmd(
            &state,
            json!({
                "command_id": "cmd-proj-1",
                "command_type": "conversation.project.create",
                "aggregate_type": "project",
                "payload": {"name": "Workspace Project", "root_path": repo}
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let project_id = proj["receipt"]["aggregate"]["id"].as_str().unwrap().to_owned();

        let (status, dir_list) = exec_qry(
            &state,
            json!({"query_type": "conversation.project.directory_list", "params": {}}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(dir_list["result"]["result"]["count"], 1);
        assert_eq!(dir_list["result"]["result"]["projects"][0]["project_id"], project_id);

        let (status, bound) = exec_cmd(
            &state,
            json!({
                "command_id": "cmd-ws-bind-1",
                "idempotency_key": "idem-ws-bind-1",
                "command_type": "conversation.workspace.bind",
                "aggregate_type": "workspace",
                "payload": {
                    "project_id": project_id,
                    "mode": "new_worktree",
                    "branch": "feat-unified",
                    "parent_root": repo
                }
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{bound}");
        let ws_id = bound["receipt"]["output"]["workspace_id"].as_str().unwrap().to_owned();

        let (status, bound_replay) = exec_cmd(
            &state,
            json!({
                "command_id": "cmd-ws-bind-2",
                "idempotency_key": "idem-ws-bind-1",
                "command_type": "conversation.workspace.bind",
                "aggregate_type": "workspace",
                "payload": {
                    "project_id": project_id,
                    "mode": "new_worktree",
                    "branch": "feat-unified",
                    "parent_root": repo
                }
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(bound_replay["receipt"]["deduplicated"], true);
        assert_eq!(bound_replay["receipt"]["output"]["workspace_id"], ws_id);

        let (status, deleted_wt) = exec_cmd(
            &state,
            json!({
                "command_id": "cmd-ws-del-1",
                "command_type": "conversation.workspace.delete_worktree",
                "aggregate_type": "workspace",
                "aggregate_id": ws_id,
                "payload": {"workspace_id": ws_id}
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{deleted_wt}");
        assert_eq!(deleted_wt["receipt"]["output"]["removed"], true);

        let (status, created_thread) = exec_cmd(
            &state,
            json!({
                "command_id": "cmd-thread-1",
                "command_type": "conversation.thread.create",
                "aggregate_type": "thread",
                "expected_version": 0,
                "payload": {"project_id": project_id, "title": "Unified Chat"}
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let thread_id = created_thread["receipt"]["aggregate"]["id"].as_str().unwrap().to_owned();

        let (status, thread_list) = exec_qry(
            &state,
            json!({
                "query_type": "conversation.thread.list",
                "params": {"project_id": project_id}
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(thread_list["result"]["result"]["count"], 1);
        assert_eq!(thread_list["result"]["result"]["threads"][0]["thread_id"], thread_id);
        assert_eq!(thread_list["result"]["result"]["threads"][0]["state"]["unread"], false);
        assert_eq!(thread_list["result"]["result"]["threads"][0]["state"]["queued_turn"], false);

        let stale_env = json!({
            "command_id": "cmd-stale-ver",
            "idempotency_key": "idem-stale-ver",
            "command_type": "conversation.plan.inject",
            "aggregate_type": "thread",
            "aggregate_id": thread_id,
            "expected_version": 99,
            "payload": {"tasks": [{"title": "Step 1"}]}
        });
        let (status, conflict) = exec_cmd(&state, stale_env.clone()).await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(conflict["receipt"]["state"], "conflict");
        assert_eq!(conflict["receipt"]["error"]["code"], "command.version_conflict");

        let (status, conflict_replay) = exec_cmd(&state, stale_env).await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(conflict_replay["receipt"]["deduplicated"], true);
        assert_eq!(conflict_replay["receipt"]["state"], "conflict");

        let b64 = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, b"hello artifact");
        let (status, attached) = exec_cmd(
            &state,
            json!({
                "command_id": "cmd-art-1",
                "command_type": "conversation.artifact.attach",
                "aggregate_type": "thread",
                "aggregate_id": thread_id,
                "payload": {
                    "name": "note.txt",
                    "content_base64": b64,
                    "media_type": "text/plain"
                }
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{attached}");
        assert_eq!(attached["receipt"]["output"]["name"], "note.txt");
        assert_eq!(attached["receipt"]["output"]["size"], 14);
        assert!(uuid::Uuid::parse_str(attached["receipt"]["output"]["artifact_id"].as_str().unwrap()).is_ok());

        let (status, mem_no_consent) = exec_cmd(
            &state,
            json!({
                "command_id": "cmd-mem-noconsent",
                "command_type": "conversation.memory.record",
                "aggregate_type": "thread",
                "aggregate_id": thread_id,
                "payload": {"content": "remember this", "consent": false}
            }),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(mem_no_consent["receipt"]["error"]["code"], "conversation.memory.consent_required");

        let (status, mem_rec) = exec_cmd(
            &state,
            json!({
                "command_id": "cmd-mem-1",
                "command_type": "conversation.memory.record",
                "aggregate_type": "thread",
                "aggregate_id": thread_id,
                "payload": {"content": "Prefer Rust tests", "kind": "preference", "consent": true}
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{mem_rec}");
        let memory_id = mem_rec["receipt"]["output"]["memory_id"].as_str().unwrap().to_owned();

        let (status, mem_list) = exec_qry(
            &state,
            json!({
                "query_type": "conversation.memory.list",
                "aggregate_type": "thread",
                "aggregate_id": thread_id,
                "params": {"query": "Rust"}
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(mem_list["result"]["result"]["memories"].as_array().unwrap().len(), 1);
        assert_eq!(mem_list["result"]["result"]["memories"][0]["memory_id"], memory_id);

        let (status, mem_del) = exec_cmd(
            &state,
            json!({
                "command_id": "cmd-mem-del-1",
                "command_type": "conversation.memory.delete",
                "aggregate_type": "thread",
                "aggregate_id": thread_id,
                "payload": {"memory_id": memory_id, "confirm": true, "reason": "cleanup"}
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{mem_del}");

        let (status, mem_after) = exec_qry(
            &state,
            json!({
                "query_type": "conversation.memory.list",
                "aggregate_type": "thread",
                "aggregate_id": thread_id,
                "params": {"include_deleted": true}
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(mem_after["result"]["result"]["memories"].as_array().unwrap().len(), 0);
        assert_eq!(mem_after["result"]["result"]["deleted_count"], 1);
        assert_eq!(mem_after["result"]["result"]["tombstones"][0]["memory_id"], memory_id);

        let (status, plan_inj) = exec_cmd(
            &state,
            json!({
                "command_id": "cmd-plan-inj-1",
                "command_type": "conversation.plan.inject",
                "aggregate_type": "thread",
                "aggregate_id": thread_id,
                "payload": {
                    "tasks": [
                        {"task_id": "t1", "title": "Inspect routes", "status": "completed"},
                        {"task_id": "t2", "title": "Wire handlers", "status": "in_progress"}
                    ]
                }
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{plan_inj}");

        let (status, plan_amend) = exec_cmd(
            &state,
            json!({
                "command_id": "cmd-plan-amend-1",
                "command_type": "conversation.plan.amend",
                "aggregate_type": "thread",
                "aggregate_id": thread_id,
                "payload": {"text": "Also add fault isolation test"}
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{plan_amend}");
        assert_eq!(
            plan_amend["receipt"]["output"]["pending_amendment"]["text"],
            "Also add fault isolation test"
        );
        assert_eq!(plan_amend["receipt"]["output"]["steered"], false);

        let (status, agents_inj) = exec_cmd(
            &state,
            json!({
                "command_id": "cmd-agents-inj-1",
                "command_type": "conversation.agents.inject",
                "aggregate_type": "thread",
                "aggregate_id": thread_id,
                "payload": {
                    "turn_id": "turn-c21",
                    "agents": [
                        {"agent_id": "ag-1", "title": "Explorer", "status": "running"}
                    ],
                    "tools": [
                        {"call_id": "call-1", "tool": "grep", "status": "completed", "output": "ok"}
                    ]
                }
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{agents_inj}");

        let (status, app_disabled) = exec_cmd(
            &state,
            json!({
                "command_id": "cmd-app-dis",
                "command_type": "conversation.approval.inject",
                "aggregate_type": "thread",
                "aggregate_id": thread_id,
                "payload": {"approval_id": "ap-1", "command": "ls"}
            }),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(app_disabled["receipt"]["error"]["code"], "conversation.approval.fixture_disabled");

        let (status, ui_disabled) = exec_cmd(
            &state,
            json!({
                "command_id": "cmd-ui-dis",
                "command_type": "conversation.user_input.inject",
                "aggregate_type": "thread",
                "aggregate_id": thread_id,
                "payload": {"request_id": "req-1"}
            }),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(ui_disabled["receipt"]["error"]["code"], "conversation.user_input.fixture_disabled");

        unsafe {
            std::env::set_var("MUTEKI_ALLOW_APPROVAL_FIXTURES", "1");
            std::env::set_var("MUTEKI_ALLOW_USER_INPUT_FIXTURES", "1");
        }
        let (status, app_inj) = exec_cmd(
            &state,
            json!({
                "command_id": "cmd-app-inj-1",
                "command_type": "conversation.approval.inject",
                "aggregate_type": "thread",
                "aggregate_id": thread_id,
                "payload": {"approval_id": "ap-1", "command": "git status"}
            }),
        )
        .await;
        let (status_ui, ui_inj) = exec_cmd(
            &state,
            json!({
                "command_id": "cmd-ui-inj-1",
                "command_type": "conversation.user_input.inject",
                "aggregate_type": "thread",
                "aggregate_id": thread_id,
                "payload": {"request_id": "req-fixture-1"}
            }),
        )
        .await;
        unsafe {
            std::env::remove_var("MUTEKI_ALLOW_APPROVAL_FIXTURES");
            std::env::remove_var("MUTEKI_ALLOW_USER_INPUT_FIXTURES");
        }
        assert_eq!(status, StatusCode::OK, "{app_inj}");
        assert_eq!(status_ui, StatusCode::OK, "{ui_inj}");
        let fixture_turn_id = ui_inj["receipt"]["output"]["turn_id"].as_str().unwrap().to_owned();
        assert_eq!(ui_inj["receipt"]["output"]["questions"].as_array().unwrap().len(), 4);

        let (status, turn_proc) = exec_qry(
            &state,
            json!({
                "query_type": "conversation.turn.process",
                "aggregate_type": "thread",
                "aggregate_id": thread_id,
                "params": {"turn_id": fixture_turn_id}
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{turn_proc}");
        assert_eq!(turn_proc["result"]["result"]["turn_id"], fixture_turn_id);
        assert!(turn_proc["result"]["result"]["count"].as_u64().unwrap() >= 2);
    }
}
