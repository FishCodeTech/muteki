//! Agent runtime settings: configured instances, discovered defaults, probes,
//! descriptors and native model catalogs.
//!
//! An instance is an adapter's launch configuration (`adapter_id:instance_id`):
//! binary, extra arguments and host-variable references. Credentials and models
//! are never part of it; a conversation names those, and they are resolved when
//! a session starts. Probes record what a concrete installation answered, so a
//! row is only shown runnable after its binary actually ran.
//!
//! Every state change answers with a stored command receipt that
//! `/api/receipts/{id}` serves.

use crate::{
    AppState,
    agent_runtime::{
        RuntimeError, binaries, catalog, credentials,
        descriptors::{self, AdapterDescriptor, EngineDescriptor},
        host_login, version_check,
    },
};
use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde_json::{Value, json};
use std::{collections::HashMap, sync::Arc, time::Duration};

pub const INSTANCES: &str = "agent_runtime_instances";
pub const HEALTH: &str = "agent_runtime_health";
/// Receipts of runtime commands, in the `{response:{receipt}}` shape the
/// receipt lookup reads.
pub const COMMANDS: &str = "runtime_commands";
pub const NATIVE_CATALOG_SOURCE: &str = "native_model_list";
pub const NATIVE_CATALOG_METADATA_VERSION: u64 = 2;
static PROFILE_WRITE_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
const PROFILE_IDEMPOTENCY: &str = "runtime_profile_idempotency";

const PROBE_TIMEOUT: Duration = Duration::from_secs(120);
const VERSION_TIMEOUT: Duration = Duration::from_secs(20);
const PROBE_CONCURRENCY: usize = 4;
const MAX_LAUNCH_ARGS: usize = 64;
const MAX_LAUNCH_ARG_LENGTH: usize = 4096;
const CATALOG_TTL_SECONDS: f64 = 3600.0;

type Reply = (StatusCode, Json<Value>);

fn now() -> String {
    chrono::Utc::now().to_rfc3339()
}

fn seconds() -> f64 {
    chrono::Utc::now().timestamp_millis() as f64 / 1000.0
}

fn storage(error: impl std::fmt::Display) -> Reply {
    tracing::error!(%error, "Runtime settings storage failed");
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({"error":{"code":"runtime.storage_error","message":"Runtime 设置无法读写本地存储","category":"internal"}})),
    )
}

fn fail(status: StatusCode, code: &str, message: impl Into<String>, category: &str) -> Reply {
    let message = message.into();
    (status, Json(json!({"error":{"code":code,"message":message,"category":category},"detail":message})))
}

fn text<'a>(value: &'a Value, key: &str) -> &'a str {
    value[key].as_str().map(str::trim).unwrap_or("")
}


/// The engine descriptor adapter an id names.
#[derive(Clone, Copy)]
enum Target {
    Native(&'static EngineDescriptor, &'static AdapterDescriptor),
}

fn target(adapter_id: &str) -> Option<(String, Target)> {
    let value = adapter_id.trim();
    let (engine, adapter) = descriptors::for_adapter(value)?;
    Some((adapter.adapter_id.clone(), Target::Native(engine, adapter)))
}

pub(crate) fn key_of(adapter_id: &str, instance_id: &str) -> String {
    format!("{adapter_id}:{instance_id}")
}

fn extension_entry(state: &AppState, adapter_id: &str) -> Option<Value> {
    crate::extensions::ready_capabilities(state,"external-agent-adapter").ok()
        .and_then(|rows|rows.into_iter().find(|row|row["id"] == adapter_id))
}

fn extension_adapter(state: &AppState, adapter_id: &str) -> bool {
    extension_entry(state,adapter_id).is_some()
}

fn extension_view(state: &AppState, adapter_id: &str) -> Value {
    let key = key_of(adapter_id,"default");
    let version = extension_entry(state,adapter_id).map(|entry|entry["extension_version"].clone()).unwrap_or(Value::Null);
    let cached = health(&state.store,&key).filter(|row|row["extension_version"] == version)
        .unwrap_or_else(||json!({"healthy":true,"source":"extension_registry",
        "detail":"Extension adapter process is ready","capabilities":{}}));
    json!({"key":key,"adapter_id":adapter_id,"instance_id":"default","engine":adapter_id,"origin":"extension",
        "label":adapter_id,"transport_kind":"structured","default_for_engine":true,
        "binary_path":"","endpoint":"","adapter_endpoint":"","transport":{},"env_refs":{},"launch_args":[],
        "discovered":true,"configured":false,"enabled":true,"updated_at":"",
        "auth":not_applicable(),"models":cached["capabilities"]["supported_models"].as_array().cloned().unwrap_or_default(),
        "default_model":"","access_modes":cached["capabilities"]["access_modes"].as_array().cloned().unwrap_or_default(),"health":cached})
}

pub fn instance(store: &crate::store::Store, adapter_id: &str, instance_id: &str) -> Option<Value> {
    let adapter = target(adapter_id).map(|(id, _)| id).unwrap_or_else(|| adapter_id.to_owned());
    store.get(INSTANCES, &key_of(&adapter, instance_id)).ok().flatten()
}

fn health(store: &crate::store::Store, key: &str) -> Option<Value> {
    store.get(HEALTH, key).ok().flatten()
}

fn is_cli(adapter: &AdapterDescriptor) -> bool {
    adapter.role == "cli" || adapter.adapter_id.starts_with("cli.")
}

fn transport_kind(adapter: &AdapterDescriptor) -> &'static str {
    if is_cli(adapter) { "cli" } else { "structured" }
}

/// Whether this build can run the adapter at all: a conversation transport,
/// or the CLI compatibility entry Worker settings list.
fn runnable(adapter: &AdapterDescriptor) -> bool {
    adapter.native.is_some() || is_cli(adapter)
}

fn config_schema(engine: &EngineDescriptor, adapter: &AdapterDescriptor) -> Value {
    let mut properties = json!({
        "instance_id": {"type":"string","title":"实例 ID","default":"default"},
        "label": {"type":"string","title":"显示名称"},
        "binary_path": {"type":"string","title":"可执行文件"},
        "enabled": {"type":"boolean","title":"启用","default":true},
    });
    if !is_cli(adapter) && adapter.accepts_launch_args {
        properties["launch_args"] = json!({"type":"array","title":"启动参数","items":{"type":"string"},"maxItems":MAX_LAUNCH_ARGS});
    }
    let transport = match adapter.adapter_id.as_str() {
        "pi.rpc" | "omp.rpc_v2" | "omp.rpc" => json!({"session_dir":{"type":"string","title":"会话目录"}}),
        "codex.app_server" => json!({"experimental_api":{"type":"boolean","default":false},"thread_section_name":{"type":"string","title":"Codex 自动归集分组","default":""}}),
        "claude.sdk" => json!({"permission_mode":{"type":"string","enum":["default","acceptEdits","plan","bypassPermissions","dontAsk","auto"]}}),
        "grok.acp" => json!({"mcp_ready_wait_seconds":{"type":"number","minimum":0,"maximum":30,"default":8}}),
        _ => json!({}),
    };
    if transport.as_object().is_some_and(|values|!values.is_empty()) {
        properties["transport"] = json!({"type":"object","title":"传输设置","properties":transport,"additionalProperties":false});
    }
    if matches!(adapter.adapter_id.as_str(),"kimi.local_server" | "opencode.server") {
        properties["adapter_endpoint"] = json!({"type":"string","title":"已有服务地址"});
    }
    json!({
        "$schema":"https://json-schema.org/draft/2020-12/schema",
        "title":format!("{} Runtime 配置", adapter.adapter_id),
        "type":"object","required":["instance_id"],"properties":properties,"additionalProperties":false,
        "x-muteki-adapter-id":adapter.adapter_id,"x-muteki-engine":engine.engine(),
        "x-muteki-transport":transport_kind(adapter),
    })
}

fn not_applicable() -> Value {
    json!({"status":"not_applicable","detail":"凭据由 Worker 或 Thread 的 credential_id 在启动时解析","login_command":"","account_id":""})
}

/// The host login state of `engine`, in the shape instance views carry.
pub async fn auth_view(engine: &EngineDescriptor, fresh: bool) -> Value {
    let command = engine.login.guidance_command.clone();
    let note = engine.login.guidance_note.clone();
    match host_login::detect(engine.engine(), fresh).await {
        host_login::DISABLED => json!({"status":"unavailable","code":"host_discovery_disabled",
            "detail":"此服务未提供宿主登录发现；请选择已登记凭据","login_command":"","account_id":""}),
        host_login::PRESENT => json!({"status":"ok","detail":"检测到宿主登录态","login_command":"","account_id":""}),
        host_login::ABSENT => json!({"status":"missing","detail":"未检测到宿主登录态","login_command":command,"note":note,"account_id":""}),
        _ => json!({"status":"unknown","detail":"认证状态检测失败","login_command":command,"note":note,"account_id":""}),
    }
}

fn detects_host(engine: &EngineDescriptor, adapter: &AdapterDescriptor) -> bool {
    is_cli(adapter) || engine.login.system_login_only
}

fn declared_capabilities(engine: &EngineDescriptor, adapter: &AdapterDescriptor) -> Value {
    let mut capabilities = engine.public()["adapters"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|row| row["adapter_id"] == adapter.adapter_id.as_str())
        .map(|row| row["capabilities"].clone())
        .unwrap_or_else(|| json!({}));
    capabilities["access_modes"] = access_modes(adapter);
    capabilities
}

fn access_modes(adapter: &AdapterDescriptor) -> Value {
    let Some(native) = adapter.native else { return json!([]) };
    let modes = crate::agent_runtime::access::offered(
        crate::agent_runtime::pool::provider_of(native),
        adapter.capabilities.approval,
        &adapter.capabilities.access_modes,
    );
    json!(modes)
}

fn finish_view(
    store: &crate::store::Store,
    engine: &EngineDescriptor,
    adapter: &AdapterDescriptor,
    mut view: Value,
) -> Value {
    let key = view["key"].as_str().unwrap_or("").to_owned();
    let mut health = health(store, &key);
    if let Some(ref mut h) = health {
        crate::agent_runtime::version_policy::annotate_health_snapshot(store, engine.engine(), h);
    }
    let cached_auth = health
        .as_ref()
        .filter(|_| detects_host(engine, adapter) && host_login::discovery_enabled())
        .map(|h| h["auth"].clone())
        .filter(Value::is_object);
    view["auth"] = cached_auth.unwrap_or_else(not_applicable);
    view["health"] = health.unwrap_or(Value::Null);
    view["access_modes"] = access_modes(adapter);
    view["transport_kind"] = json!(transport_kind(adapter));
    view["default_for_engine"] = json!(
        view["instance_id"] == "default" && engine.identity.default_adapter_id == adapter.adapter_id
    );
    view["config_schema"] = config_schema(engine, adapter);
    view
}

/// Engines with a saved conversation (structured) instance, and whether any
/// of those instances is enabled.
fn added_engines(saved: &[Value]) -> HashMap<String, bool> {
    let mut out = HashMap::new();
    for cfg in saved {
        if let Some((_, Target::Native(engine, adapter))) = target(text(cfg, "adapter_id")) {
            if !is_cli(adapter) {
                *out.entry(engine.engine().to_owned()).or_insert(false) |= cfg["enabled"] != false;
            }
        }
    }
    out
}

/// Whether conversations may use a discovered (unsaved) structured adapter.
/// Engines listed by default keep every discovered adapter. Elsewhere the
/// engine's own default adapter must be added explicitly, while the
/// access-mode variants follow once an enabled instance of the engine exists.
fn chat_admitted(engine: &EngineDescriptor, adapter: &AdapterDescriptor, added: &HashMap<String, bool>) -> bool {
    engine.identity.default_enabled
        || (engine.identity.default_adapter_id != adapter.adapter_id
            && added.get(engine.engine()).copied().unwrap_or(false))
}

/// Whether a conversation may start on an instance. Saved rows answer with
/// their own switch; an unsaved discovered default only belongs to chat once
/// its engine is listed by default or was added.
pub fn chat_startable(store: &crate::store::Store, adapter_id: &str, instance_id: &str) -> bool {
    let Some((id, Target::Native(engine, adapter))) = target(adapter_id) else { return true };
    if let Some(row) = store.get(INSTANCES, &key_of(&id, instance_id)).ok().flatten() {
        return row["enabled"] != false;
    }
    is_cli(adapter) || chat_admitted(engine, adapter, &added_engines(&store.list(INSTANCES).unwrap_or_default()))
}

const ADOPTION_MARKER: &str = "runtime_agents_explicit_v1";

/// Conversations used to accept every healthy discovered runtime. Engines now
/// have to be added, so once per installation save the structured runtimes
/// that were already healthy as added instances and keep them usable.
pub fn adopt_discovered_agents(store: &crate::store::Store) -> anyhow::Result<usize> {
    if store.get("system", ADOPTION_MARKER)?.is_some() {
        return Ok(0);
    }
    let added = added_engines(&store.list(INSTANCES)?);
    let healthy: Vec<_> = discovered_adapters("structured").into_iter()
        .filter(|(engine, adapter)| !engine.identity.default_enabled && engine.identity.support_status == "supported"
            && !added.contains_key(engine.engine())
            && health(store, &key_of(&adapter.adapter_id, "default")).is_some_and(|h| h["healthy"] == true))
        .collect();
    let mut adopted = Vec::new();
    for &(engine, adapter) in &healthy {
        let is_default = engine.identity.default_adapter_id == adapter.adapter_id;
        if !is_default && healthy.iter().any(|(other, a)| other.engine() == engine.engine() && a.adapter_id == engine.identity.default_adapter_id) {
            continue;
        }
        let key = key_of(&adapter.adapter_id, "default");
        let label = if is_default {
            engine.identity.display_name.clone()
        } else {
            format!("{} · {}", engine.identity.display_name, adapter.adapter_id)
        };
        let cfg = normalized(&json!({"enabled":true,"label":label}), None, engine, adapter, "default")
            .map_err(anyhow::Error::msg)?;
        adopted.push((key, cfg));
    }
    let marker = json!({"adopted": adopted.iter().map(|(key, _)| key.clone()).collect::<Vec<_>>(), "at": now()});
    let mut records: Vec<(&str, &str, &Value)> = adopted.iter().map(|(key, cfg)| (INSTANCES, key.as_str(), cfg)).collect();
    records.push(("system", ADOPTION_MARKER, &marker));
    store.put_many(&records)?;
    Ok(adopted.len())
}

fn discovered_view(store: &crate::store::Store, engine: &EngineDescriptor, adapter: &AdapterDescriptor, admitted: bool) -> Value {
    let key = key_of(&adapter.adapter_id, "default");
    let health = health(store, &key);
    let structured = !is_cli(adapter);
    let view = json!({
        "adapter_id":adapter.adapter_id,"instance_id":"default","key":key,"engine":engine.engine(),
        "label":if structured { format!("{}（结构化兼容）", engine.engine()) } else { format!("{}（CLI 默认）", engine.engine()) },
        "binary_path":health.as_ref().map(|h| h["binary_path"].clone()).unwrap_or(json!("")),
        "adapter_endpoint":"","endpoint":"","env_refs":{},
        "enabled":(!structured || admitted) && health.as_ref().is_some_and(|h| h["healthy"] == true),
        "transport":{},"launch_args":[],"discovered":true,"configured":false,
    });
    finish_view(store, engine, adapter, view)
}

fn configured_view(store: &crate::store::Store, engine: &EngineDescriptor, adapter: &AdapterDescriptor, cfg: &Value) -> Value {
    let mut view = cfg.clone();
    view["discovered"] = json!(false);
    view["configured"] = json!(true);
    finish_view(store, engine, adapter, view)
}

/// Adapters listed without a saved row: each engine's default conversation
/// adapter, runnable variants that honor an access mode, and the CLI entries.
fn discovered_adapters(transport: &str) -> Vec<(&'static EngineDescriptor, &'static AdapterDescriptor)> {
    let mut out = Vec::new();
    for engine in descriptors::all() {
        for adapter in &engine.adapters {
            let wanted = if is_cli(adapter) {
                transport != "structured" && engine.identity.cli_adapter_id == adapter.adapter_id
            } else {
                transport != "cli"
                    && (engine.identity.default_adapter_id == adapter.adapter_id
                        || (adapter.native.is_some() && !adapter.capabilities.access_modes.is_empty()))
            };
            if wanted {
                out.push((engine, adapter));
            }
        }
    }
    out
}

/// Every instance view: configured rows, then discovered defaults, then
/// extension adapters.
pub fn list_views(state: &AppState, adapter_filter: &str, include_discovered: bool, transport: &str) -> Result<Vec<Value>, Reply> {
    let filter = if adapter_filter.trim().is_empty() {
        None
    } else {
        Some(target(adapter_filter).map(|(id, _)| id).unwrap_or_else(|| adapter_filter.trim().to_owned()))
    };
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let saved = state.store.list(INSTANCES).map_err(storage)?;
    let added = added_engines(&saved);
    for cfg in saved {
        let adapter_id = text(&cfg, "adapter_id");
        let Some((_, Target::Native(engine, adapter))) = target(adapter_id) else {
            continue;
        };
        if filter.as_deref().is_some_and(|f| f != adapter.adapter_id)
            || (filter.is_none() && !transport.is_empty() && transport != transport_kind(adapter))
        {
            continue;
        }
        seen.insert(text(&cfg, "key").to_owned());
        out.push(configured_view(&state.store, engine, adapter, &cfg));
    }
    if include_discovered {
        let candidates: Vec<_> = match &filter {
            Some(id) => descriptors::for_adapter(id).filter(|(_, a)| &a.adapter_id == id).into_iter().collect(),
            None => discovered_adapters(transport),
        };
        for (engine, adapter) in candidates {
            if !seen.contains(&key_of(&adapter.adapter_id, "default")) {
                out.push(discovered_view(&state.store, engine, adapter, chat_admitted(engine, adapter, &added)));
            }
        }
    }
    if transport != "cli" {
        for entry in crate::extensions::ready_capabilities(state,"external-agent-adapter")
            .map_err(|_|fail(StatusCode::INTERNAL_SERVER_ERROR,"extension.registry_unavailable","Extension registry unavailable","internal"))? {
            let id = entry["id"].as_str().unwrap_or("");
            if !id.is_empty() && filter.as_deref().is_none_or(|wanted|wanted==id) {
                out.push(extension_view(state,id));
            }
        }
    }
    Ok(out)
}

fn view_of(state: &AppState, adapter_id: &str, instance_id: &str) -> Result<Option<Value>, Reply> {
    match target(adapter_id) {
        Some((id, Target::Native(engine, adapter))) => {
            if let Some(cfg) = state.store.get(INSTANCES, &key_of(&id, instance_id)).map_err(storage)? {
                return Ok(Some(configured_view(&state.store, engine, adapter, &cfg)));
            }
            if instance_id != "default" {
                return Ok(None);
            }
            let added = added_engines(&state.store.list(INSTANCES).map_err(storage)?);
            Ok(Some(discovered_view(&state.store, engine, adapter, chat_admitted(engine, adapter, &added))))
        }
        None if instance_id == "default" && extension_adapter(state,adapter_id) =>
            Ok(Some(extension_view(state,adapter_id))),
        _ => Ok(None),
    }
}

/// `adapter:instance`, or a bare instance id that names exactly one saved row.
fn parse_key(state: &AppState, key: &str) -> (String, String) {
    if let Some((adapter, instance)) = key.trim().rsplit_once(':') {
        let adapter = target(adapter).map(|(id, _)| id).unwrap_or_else(|| adapter.to_owned());
        return (adapter, if instance.is_empty() { "default".into() } else { instance.to_owned() });
    }
    let wanted = key.trim();
    let rows: Vec<Value> = state
        .store
        .list(INSTANCES)
        .unwrap_or_default()
        .into_iter()
        .filter(|row| row["instance_id"] == wanted)
        .collect();
    if rows.len() == 1 {
        return (text(&rows[0], "adapter_id").to_owned(), wanted.to_owned());
    }
    (String::new(), wanted.to_owned())
}


fn error_value(code: &str, message: impl Into<String>, category: &str, retryable: bool) -> Value {
    json!({"code":code,"message":message.into(),"category":category,"retryable":retryable})
}

fn runtime_error(error: &RuntimeError, category: &str) -> Value {
    error_value(error.code, &error.message, category, error.retryable)
}

struct Outcome {
    output: Value,
    error: Option<Value>,
    failed: bool,
}

impl Outcome {
    fn done(output: Value) -> Self {
        Self { output, error: None, failed: false }
    }

    fn failed(error: Value) -> Self {
        Self { output: json!({}), error: Some(error), failed: true }
    }
}

fn status_of(outcome: &Outcome) -> StatusCode {
    if !outcome.failed {
        return StatusCode::OK;
    }
    match outcome.error.as_ref().map(|e| text(e, "category")).unwrap_or("") {
        "not_found" => StatusCode::NOT_FOUND,
        "validation" => StatusCode::BAD_REQUEST,
        "state" | "conflict" => StatusCode::CONFLICT,
        _ => StatusCode::BAD_GATEWAY,
    }
}

fn command_id(body: &Value) -> String {
    match text(body, "command_id") {
        "" => format!("cmd_runtime_{}", uuid::Uuid::new_v4().simple()),
        id => id.to_owned(),
    }
}

fn prior(state: &AppState, id: &str) -> Option<Response> {
    let stored = state.store.get(COMMANDS, id).ok().flatten()?;
    let mut receipt = stored["response"]["receipt"].clone();
    receipt["deduplicated"] = json!(true);
    let status = StatusCode::from_u16(stored["status"].as_u64().unwrap_or(200) as u16).unwrap_or(StatusCode::OK);
    Some((status, Json(json!({"receipt":receipt}))).into_response())
}

fn record(state: &AppState, id: &str, command_type: &str, aggregate: &str, outcome: Outcome, extra: Option<(&str, Value)>) -> Response {
    record_mutation(state,id,command_type,aggregate,outcome,extra,&[],&[],&[])
}
fn record_mutation(
    state: &AppState, id: &str, command_type: &str, aggregate: &str, outcome: Outcome,
    extra: Option<(&str,Value)>, expected: &[(&str,&str,Option<&Value>)], records: &[(&str,&str,&Value)], deletes: &[(String,String)],
) -> Response {
    use crate::operations_api::{domain_event_cursor,current_aggregate_version,domain_events};
    let status = status_of(&outcome);
    let platform = match state.store.get("platform_commands",id) { Ok(value) => value, Err(error) => return storage(error).into_response() };
    let mut envelope = platform.as_ref().map(|row|row["request"].clone()).unwrap_or_else(||json!({}));
    envelope["command_id"] = json!(id);
    let principal = platform.as_ref().and_then(|row|row["principal"].as_str()).unwrap_or("operator:local-user");
    let version = match current_aggregate_version(state,"runtime",aggregate) { Ok(version) => version, Err(error) => return storage(error).into_response() };
    let cursor = match domain_event_cursor(state,"runtime",aggregate,version+1,principal) { Ok(cursor) => cursor, Err(error) => return storage(error).into_response() };
    let receipt = json!({"schema_version":1,"command_id":id,"receipt_id":id,"command_type":command_type,
        "state":if outcome.failed {"failed"}else{"completed"},"aggregate":{"type":"runtime","id":aggregate},
        "run_id":null,"event_cursor":cursor,"output":outcome.output,"error":outcome.error,
        "deduplicated":false,"completed_at":now()});
    let mut body = json!({"receipt":receipt});
    if let Some((key,value)) = extra { body[key] = value; }
    let stored = json!({"response":{"receipt":receipt},"status":status.as_u16()});
    let platform_next = platform.as_ref().map(|prior| {
        let mut next = prior.clone();
        next["response"]["receipt"] = receipt.clone();
        next["status"] = json!(status.as_u16());
        next
    });
    let event_type = if outcome.failed { format!("core.{command_type}_failed") } else {
        match command_type {
            "runtime.instance.upsert" => "core.runtime.instance.upserted".to_owned(),
            "runtime.instance.remove" => "core.runtime.instance.removed".to_owned(),
            "runtime.instance.probe" => "core.runtime.instance.probed".to_owned(),
            "runtime.instance.refresh" => "core.runtime.instance.refresh_completed".to_owned(),
            _ => format!("core.{}_completed",command_type.replace(".auth.",".auth_")),
        }
    };
    let event = domain_events::event(&envelope,"runtime",aggregate,&event_type,"platform.agent_runtime",
        if outcome.failed{json!({"error":receipt["error"]})}else{receipt["output"].clone()});
    let mut checks = expected.to_vec();
    let mut writes = records.to_vec();
    checks.push((COMMANDS,id,None));
    writes.push((COMMANDS,id,&stored));
    if let Some(next) = platform_next.as_ref() {
        checks.push(("platform_commands",id,platform.as_ref()));
        writes.push(("platform_commands",id,next));
    }
    match domain_events::commit_with_deletes(state,&checks,&writes,&[event],deletes) {
        Ok(Some(_)) => (status,Json(body)).into_response(),
        Ok(None) => prior(state,id).unwrap_or_else(|| fail(StatusCode::CONFLICT,"command.version_conflict","Runtime changed during command commit","conflict").into_response()),
        Err(error) => error.into_response(),
    }
}


pub(crate) async fn run_version(binary: &std::path::Path) -> Result<String, String> {
    let mut cmd = crate::host_process::binary_command(binary).map_err(|error| error.message)?;
    let child = cmd
        .arg("--version")
        .env("NO_COLOR", "1")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .output();
    let output = tokio::time::timeout(VERSION_TIMEOUT, child)
        .await
        .map_err(|_| format!("--version 超过 {} 秒", VERSION_TIMEOUT.as_secs()))?
        .map_err(|e| e.to_string())?;
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
    if !output.status.success() {
        return Err(format!("--version 以 {} 退出：{}", output.status, if stderr.is_empty() { &stdout } else { &stderr }));
    }
    let text = if stdout.is_empty() { stderr } else { stdout };
    Ok(text.lines().next().unwrap_or("").trim().to_owned())
}

async fn codex_plan_supported(binary: &std::path::Path) -> bool {
    let Ok(directory) = tempfile::tempdir() else { return false };
    let Ok(mut command) = crate::host_process::binary_command(binary) else { return false };
    command.args(["app-server", "generate-json-schema", "--out"])
        .arg(directory.path())
        .arg("--experimental")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);
    let Ok(Ok(output)) = tokio::time::timeout(Duration::from_secs(30), command.output()).await else {
        return false;
    };
    if !output.status.success() { return false; }
    let path = [directory.path().join("TurnStartParams.json"), directory.path().join("v2/TurnStartParams.json")]
        .into_iter().find(|path| path.is_file());
    let Some(path) = path else { return false };
    let Ok(bytes) = std::fs::read(path) else { return false };
    let Ok(schema) = serde_json::from_slice::<Value>(&bytes) else { return false };
    schema["properties"].get("collaborationMode").is_some()
        || schema["definitions"].get("CollaborationMode").is_some()
        || schema["$defs"].get("CollaborationMode").is_some()
}

fn failure_health(previous: Option<&Value>, detail: &str, code: &str, source: &str, auth: Value) -> Value {
    json!({
        "healthy":false,"detail":detail,"error_code":code,"binary_path":"","binary_source":"","binary_env":"",
        "runtime_version":"","probed_at":now(),"probe_cached":false,"capabilities":{},
        "capability_injection":{},"field_sources":{},"degradations":[],"auth":auth,"source":source,
        "version_check":previous.map(|p| p["version_check"].clone()).filter(Value::is_object).unwrap_or(json!({})),
    })
}

/// Probe one instance and return its health snapshot. A failure is a
/// snapshot too (`healthy: false` with the real cause), never an error.
async fn probe_health(
    state: &AppState,
    engine: &'static EngineDescriptor,
    adapter: &'static AdapterDescriptor,
    instance_id: &str,
    force_version: bool,
) -> Value {
    let key = key_of(&adapter.adapter_id, instance_id);
    let previous = health(&state.store, &key);
    let auth = if detects_host(engine, adapter) { auth_view(engine, true).await } else { not_applicable() };
    if !runnable(adapter) {
        let fallback = &engine.identity.default_adapter_id;
        return failure_health(
            previous.as_ref(),
            &format!("此版本没有 {} 的传输实现；请改用 {fallback}", adapter.adapter_id),
            "capability.unsupported",
            "probe_error",
            auth,
        );
    }
    let cfg = instance(&state.store, &adapter.adapter_id, instance_id).unwrap_or(Value::Null);
    let configured = text(&cfg, "binary_path");
    let explicit = (!configured.is_empty()).then(|| std::path::PathBuf::from(configured));
    let binary = match binaries::resolve(engine, explicit.as_deref()).await {
        Ok(binary) => binary,
        Err(error) => {
            if explicit.is_none() {
                let state_dir = crate::host_process::configured_state_dir(state);
                let _ = crate::host_process::refresh_shell_env(
                    state_dir.as_deref(),
                    Duration::from_secs(15),
                )
                .await;
                if let Ok(binary) = binaries::resolve(engine, None).await {
                    binary
                } else {
                    return failure_health(previous.as_ref(), &error.message, error.code, "probe_error", auth);
                }
            } else {
                return failure_health(previous.as_ref(), &error.message, error.code, "probe_error", auth);
            }
        }
    };
    let version = match run_version(&binary.path).await {
        Ok(version) => version,
        Err(detail) => {
            let detail = if explicit.is_some() {
                format!("实例配置的 binary 不可运行：{detail}")
            } else {
                format!("{} 不可运行：{detail}", binary.path.display())
            };
            let mut row = failure_health(previous.as_ref(), &detail, "runtime.binary_unrunnable", "probe_error", auth);
            row["binary_path"] = json!(binary.path.to_string_lossy());
            row["binary_source"] = json!(binary.source);
            return row;
        }
    };
    let lookups_off = match version_check::lookups_disabled(&state.store) {
        Ok(off) => off,
        Err(detail) => return failure_health(previous.as_ref(), &detail, "runtime.settings_unreadable", "probe_error", auth),
    };
    let mut version_check = if lookups_off {
        version_check::disabled(engine.engine(), &version)
    } else {
        version_check::cached(engine.engine(), &binary.path, &version, force_version).await
    };
    if version_check["status"] == "unknown"
        && let Some(previous) = previous.as_ref().map(|p| &p["version_check"])
        && previous["installed_version"] == version_check["installed_version"]
        && !text(previous, "latest_version").is_empty()
    {
        let attempted = version_check["checked_at"].clone();
        let (error, code) = (version_check["error"].clone(), version_check["error_code"].clone());
        version_check = previous.clone();
        version_check["stale"] = json!(true);
        version_check["attempted_at"] = attempted;
        version_check["detail"] = json!("版本源暂时不可用，沿用上次检查结果");
        version_check["error"] = error;
        version_check["error_code"] = code;
    }
    let mut capabilities = declared_capabilities(engine, adapter);
    capabilities["runtime_version"] = json!(version);
    let mut field_sources = json!({"runtime_version":"probe"});
    if adapter.adapter_id == "codex.app_server" {
        capabilities["plan_mode"] = json!(cfg["transport"]["experimental_api"] == true
            && codex_plan_supported(&binary.path).await);
        field_sources["plan_mode"] = json!("probe");
    }
    let mut degradations = Vec::new();
    if engine.login.system_login_only && auth["status"] == "missing" {
        degradations.push(json!(format!("未检测到宿主登录态；请先执行 {}", engine.login.guidance_command)));
    }
    let mut snapshot = json!({
        "healthy":true,
        "detail":format!("{} {}", engine.identity.display_name, version),
        "binary_path":binary.path.to_string_lossy(),"binary_source":binary.source,
        "binary_env":engine.binary.env_override,"runtime_version":version,
        "version_check":version_check,"probed_at":now(),"probe_cached":false,
        "capabilities":capabilities,"capability_injection":{},
        "field_sources":field_sources,"configuration_updated_at":cfg["updated_at"],
        "degradations":degradations,"auth":auth,
        "source":if is_cli(adapter) { "cli_driver" } else { "native_adapter" },
    });
    crate::agent_runtime::version_policy::annotate_health_snapshot(&state.store, engine.engine(), &mut snapshot);
    snapshot
}

async fn probe_and_save(
    state: &AppState,
    engine: &'static EngineDescriptor,
    adapter: &'static AdapterDescriptor,
    instance_id: &str,
    force_version: bool,
) -> Result<Value, Reply> {
    let key = key_of(&adapter.adapter_id, instance_id);
    let snapshot = match tokio::time::timeout(PROBE_TIMEOUT, probe_health(state, engine, adapter, instance_id, force_version)).await {
        Ok(snapshot) => snapshot,
        Err(_) => {
            let detail = format!("Runtime 探测超过 {} 秒", PROBE_TIMEOUT.as_secs());
            let mut row = failure_health(health(&state.store, &key).as_ref(), &detail, "runtime.probe_timeout", "probe_timeout", not_applicable());
            row["degradations"] = json!([detail]);
            row
        }
    };
    state.store.put(HEALTH, &key, &snapshot).map_err(storage)?;
    Ok(snapshot)
}

/// Probe every listed instance, four at a time; one failure never stops the rest.
pub async fn probe_all(state: &Arc<AppState>, adapter_filter: &str, transport: &str, force_version: bool) -> Result<Value, Reply> {
    if force_version {
        let state_dir = crate::host_process::configured_state_dir(state);
        let _ = crate::host_process::refresh_shell_env(
            state_dir.as_deref(),
            Duration::from_secs(15),
        )
        .await;
    }
    let views = list_views(state, adapter_filter, true, transport)?;
    let semaphore = Arc::new(tokio::sync::Semaphore::new(PROBE_CONCURRENCY));
    let mut tasks = Vec::new();
    for view in views {
        let Some((_, Target::Native(engine, adapter))) = target(text(&view, "adapter_id")) else {
            continue;
        };
        let instance_id = text(&view, "instance_id").to_owned();
        let (state, semaphore) = (state.clone(), semaphore.clone());
        tasks.push(tokio::spawn(async move {
            let _permit = semaphore.acquire_owned().await;
            let mut entry = json!({"key":key_of(&adapter.adapter_id, &instance_id),"adapter_id":adapter.adapter_id,
                "instance_id":instance_id,"engine":engine.engine()});
            match probe_and_save(&state, engine, adapter, &instance_id, force_version).await {
                Ok(health) => {
                    entry["ok"] = json!(true);
                    entry["healthy"] = health["healthy"].clone();
                    entry["detail"] = health["detail"].clone();
                    entry["probe_cached"] = json!(false);
                    entry["version_check"] = health["version_check"].clone();
                    if let Some(code) = health.get("error_code") {
                        entry["error_code"] = code.clone();
                    }
                }
                Err((_, Json(body))) => {
                    entry["ok"] = json!(false);
                    entry["healthy"] = json!(false);
                    entry["error_code"] = body["error"]["code"].clone();
                    entry["detail"] = body["error"]["message"].clone();
                }
            }
            entry
        }));
    }
    let mut results = Vec::new();
    for task in tasks {
        match task.await {
            Ok(entry) => results.push(entry),
            Err(error) => results.push(json!({"ok":false,"healthy":false,"detail":format!("probe task failed: {error}")})),
        }
    }
    let failed = results.iter().filter(|r| r["ok"] != true).count();
    Ok(json!({"probed_at":now(),"total":results.len(),"failed":failed,"results":results}))
}

/// Re-probe in the background on an interval (`MUTEKI_AGENT_REFRESH_INTERVAL`
/// seconds, default 300, `0` disables) so the persisted snapshot serves
/// requests while slow CLIs answer.
pub fn spawn_refresh_loop(state: Arc<AppState>) {
    let interval = std::env::var("MUTEKI_AGENT_REFRESH_INTERVAL")
        .ok()
        .and_then(|v| v.trim().parse::<f64>().ok())
        .unwrap_or(300.0)
        .max(0.0);
    if interval <= 0.0 {
        return;
    }
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs_f64(interval.min(5.0))).await;
        loop {
            if state.admission.draining(){tokio::time::sleep(std::time::Duration::from_millis(250)).await;continue;}
            if let Err((_, Json(body))) = probe_all(&state, "", "", false).await {
                tracing::error!(error = %body, "Background runtime refresh failed");
            }
            tokio::time::sleep(Duration::from_secs_f64(interval)).await;
        }
    });
}


pub(crate) fn catalog_key(credential_id: &str, runtime_key: &str) -> String {
    format!("native:{credential_id}|{runtime_key}")
}

pub fn is_native_runtime(runtime_key: &str) -> bool {
    let adapter = runtime_key.rsplit_once(':').map(|(a, _)| a).unwrap_or(runtime_key);
    matches!(target(adapter), Some((_, Target::Native(..))))
}

pub fn default_runtime_key(engine: &str) -> Option<String> {
    descriptors::find(engine).map(|e| key_of(&e.identity.default_adapter_id, "default"))
}

/// Read one credential's native catalog under one runtime and store it.
/// The response matches the HTTP catalog refresh.
pub async fn refresh_native_catalog(state: &AppState, credential_id: &str, runtime_key: &str) -> Result<Value, Reply> {
    let (adapter_id, instance_id) = parse_key(state, runtime_key);
    let Some((_, Target::Native(engine, adapter))) = target(&adapter_id) else {
        return Err(fail(StatusCode::NOT_FOUND, "runtime.instance.not_found", format!("unknown runtime instance: {runtime_key}"), "not_found"));
    };
    let runtime_key = key_of(&adapter.adapter_id, &instance_id);
    let credential = credentials::canonical(credential_id, engine.engine())
        .map_err(|e| fail(StatusCode::BAD_REQUEST, e.code, e.message, "validation"))?
        .id();
    let account_id = credential.strip_prefix("account:");
    let account_snapshot = account_id.map(|id|state.store.get("credential_accounts",id)).transpose().map_err(storage)?.flatten();
    let legacy_snapshot = account_id.map(|id|state.store.get("model_endpoints",id)).transpose().map_err(storage)?.flatten();
    let runtime_snapshot = state.store.get(INSTANCES,&runtime_key).map_err(storage)?;
    let key = catalog_key(&credential, &runtime_key);
    let old = state.store.get("model_catalogs", &key).map_err(storage)?;
    let reference_catalog = engine.public()["models"]["method"] == "reference_catalog";
    let result: Result<Vec<Value>, RuntimeError> = if reference_catalog {
        serde_json::from_str::<Value>(include_str!("runs/worker_models.manual.json"))
            .map(|reference| reference["models"][engine.engine()].as_array().cloned().unwrap_or_default())
            .map_err(|error| RuntimeError::new("model_catalog.invalid_reference", error.to_string()))
    } else { async {
        let native = adapter
            .native
            .or_else(|| (adapter.role == "cli").then(|| engine.default_adapter().native).flatten())
            .ok_or_else(|| {
                RuntimeError::unsupported(format!(
                    "{} declares no conversation transport in this build",
                    adapter.adapter_id
                ))
            })?;
        let resolved = credentials::resolve(&state.store, &crate::native_chat::state_dir(state), engine, &credential, None).await?;
        let cfg = runtime_snapshot.clone().unwrap_or(Value::Null);
        let instance_env = cfg["env"].as_object().into_iter().flatten()
            .filter_map(|(key, value)| value.as_str().map(|value| (key.clone(), value.to_owned()))).collect();
        let refs = credentials::env_refs_with_platform(state, &cfg["env_refs"])?;
        let env = crate::native_chat::merge_credential_env(resolved.env, instance_env, refs, credential.starts_with("account:"));
        let home = Some(text(&cfg, "home")).filter(|value| !value.is_empty()).map(std::path::PathBuf::from);
        let explicit = Some(text(&cfg, "binary_path")).filter(|p| !p.is_empty()).map(std::path::PathBuf::from);
        let binary = binaries::resolve(engine, explicit.as_deref()).await?;
        let cache = crate::native_chat::state_dir(state).join("model-manifests");
        catalog::list(engine, native, &binary.path, agent::LaunchEnv { env, home }, &cache).await
    }
    .await };
    let ok = result.is_ok();
    let (models, detail, code) = match result {
        Ok(models) => {
            let detail = if reference_catalog {
                format!("{} 没有稳定的模型清单命令；这里显示参考目录，真实可用性以手动连通测试为准", engine.identity.display_name)
            } else {
                format!("从 {} 读到 {} 个模型", engine.identity.display_name, models.len())
            };
            (models, detail, String::new())
        }
        Err(error) => (
            old.as_ref().and_then(|v| v["discovered_models"].as_array()).cloned().unwrap_or_default(),
            error.message.clone(),
            error.code.to_owned(),
        ),
    };
    let default_model = models
        .iter()
        .find(|m| m["is_default"] == true)
        .or_else(|| models.first())
        .map(|m| m["id"].clone())
        .unwrap_or(Value::Null);
    let previous = |field: &str| old.as_ref().map(|v| v[field].clone()).unwrap_or(Value::Null);
    let discovery_source = if reference_catalog {
        format!("{}_reference_catalog", engine.engine())
    } else {
        NATIVE_CATALOG_SOURCE.to_owned()
    };
    let catalog = json!({
        "credential_id":credential,"engine":engine.engine(),"environment":"local","runtime_instance":runtime_key,
        "configured_models":[],"discovered_models":models,
        "verified_models":old.as_ref().map(|v| v["verified_models"].clone()).filter(Value::is_array).unwrap_or(json!([])),
        "default_model":default_model,"source":NATIVE_CATALOG_SOURCE,"metadata_version":NATIVE_CATALOG_METADATA_VERSION,
        "discovery_source":discovery_source,
        "refresh_status":if ok { "fresh" } else if models.is_empty() { "failed" } else { "stale" },
        "refreshed_at":if ok { json!(seconds()) } else { previous("refreshed_at") },
        "expires_at":if ok { json!(seconds() + CATALOG_TTL_SECONDS) } else { previous("expires_at") },
        "last_error":if ok { "" } else { detail.as_str() },"error_code":code,
    });
    let mut catalog = catalog;
    catalog["configuration_revision"] = account_snapshot.as_ref().map(|row|row["revision"].clone()).unwrap_or(Value::Null);
    let mut conditions = vec![(INSTANCES,runtime_key.as_str(),runtime_snapshot.as_ref()),("model_catalogs",key.as_str(),old.as_ref())];
    if let Some(id) = account_id {
        conditions.push(("credential_accounts",id,account_snapshot.as_ref()));
        conditions.push(("model_endpoints",id,legacy_snapshot.as_ref()));
    }
    if !state.store.put_many_if(&conditions,&[("model_catalogs",key.as_str(),&catalog)]).map_err(storage)? {
        return Err(fail(StatusCode::CONFLICT,"credential.account.revision_conflict","Credential, Runtime or model evidence changed during catalog refresh","conflict"));
    }

    let ids: Vec<Value> = models.iter().map(|m| m["id"].clone()).collect();
    Ok(json!({"ok":ok,"detail":detail,"source":discovery_source,"models":ids,"credential_id":credential,
        "catalog":catalog,"error_code":code}))
}

/// Public catalog fields of every native catalog stored for `credential_id`,
/// keyed by runtime instance.
pub fn native_catalogs(store: &crate::store::Store, credential_id: &str) -> serde_json::Map<String, Value> {
    native_catalogs_for_environment(store, credential_id, "local")
}

pub fn native_catalogs_for_environment(store: &crate::store::Store, credential_id: &str, environment: &str) -> serde_json::Map<String, Value> {
    let mut out = serde_json::Map::new();
    for row in store.list("model_catalogs").unwrap_or_default() {
        if row["credential_id"] != credential_id || row["source"] != NATIVE_CATALOG_SOURCE
            || row["environment"].as_str().unwrap_or("local") != environment {
            continue;
        }
        if let Some(runtime) = row["runtime_instance"].as_str() {
            out.insert(runtime.to_owned(), row.clone());
        }
    }
    let existing: Vec<(String, Value)> = out.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
    for (runtime, row) in existing {
        let (adapter_id, instance_id) = runtime.rsplit_once(':').unwrap_or((&runtime, "default"));
        let Some((engine, _)) = descriptors::for_adapter(adapter_id) else {
            continue;
        };
        for sibling_adapter in [&engine.identity.default_adapter_id, &engine.identity.cli_adapter_id] {
            if sibling_adapter.is_empty() {
                continue;
            }
            let sibling_key = key_of(sibling_adapter, instance_id);
            out.entry(sibling_key.clone()).or_insert_with(|| {
                let mut cloned = row.clone();
                cloned["runtime_instance"] = json!(sibling_key);
                cloned
            });
        }
    }
    out
}


fn env_refs(value: &Value) -> Result<Value, String> {
    if value.is_null() || value == "" {
        return Ok(json!({}));
    }
    let Some(rows) = value.as_object() else {
        return Err("env_refs 必须是对象 {变量名: 引用}".into());
    };
    let mut out = serde_json::Map::new();
    for (name, raw) in rows {
        let name = name.trim();
        if name.is_empty() {
            continue;
        }
        let reference = raw.as_str().unwrap_or("").trim();
        let upper = name.chars().any(|c| c.is_ascii_alphabetic())
            && name.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_');
        if !upper {
            return Err(format!("env_refs 变量名须为大写环境变量名：{name}"));
        }
        if !reference.is_empty() && !reference.starts_with("env:") && !reference.starts_with("secret://") {
            return Err(format!("env_refs[{name}] 只接受 secret:// 或 env: 引用，不接受内联值"));
        }
        out.insert(name.to_owned(), json!(reference));
    }
    Ok(Value::Object(out))
}

fn launch_args(value: &Value, adapter: &AdapterDescriptor) -> Result<Value, String> {
    let empty = value.is_null() || value == "" || value.as_array().is_some_and(Vec::is_empty);
    if empty {
        return Ok(json!([]));
    }
    let Some(items) = value.as_array() else {
        return Err("launch_args 必须是字符串数组".into());
    };
    if is_cli(adapter) || !adapter.accepts_launch_args {
        return Err(format!("{} 不支持 launch_args", adapter.adapter_id));
    }
    if items.len() > MAX_LAUNCH_ARGS {
        return Err(format!("launch_args 最多 {MAX_LAUNCH_ARGS} 项，当前 {} 项", items.len()));
    }
    for (index, item) in items.iter().enumerate() {
        let Some(item) = item.as_str() else {
            return Err(format!("launch_args[{index}] 必须是字符串"));
        };
        if item.trim().is_empty() {
            return Err(format!("launch_args[{index}] 不能为空"));
        }
        if item.contains(['\0', '\n']) {
            return Err(format!("launch_args[{index}] 不能包含换行或 NUL"));
        }
        if item.len() > MAX_LAUNCH_ARG_LENGTH {
            return Err(format!("launch_args[{index}] 超过 {MAX_LAUNCH_ARG_LENGTH} 字符"));
        }
    }
    Ok(value.clone())
}

fn validate_transport(_engine: &EngineDescriptor, adapter: &AdapterDescriptor, transport: &Value, endpoint: &str) -> Result<(),String> {
    if !endpoint.is_empty() {
        if !matches!(adapter.adapter_id.as_str(),"kimi.local_server" | "opencode.server") { return Err(format!("{} 不支持已有服务地址",adapter.adapter_id)); }
        let url = url::Url::parse(endpoint).map_err(|_|"服务地址必须是 HTTP URL")?;
        if !matches!(url.scheme(),"http" | "https") || url.host_str().is_none() || !url.username().is_empty() || url.password().is_some() || url.query().is_some() || url.fragment().is_some() {
            return Err("服务地址必须是没有内嵌凭据、查询或 fragment 的 HTTP URL".into());
        }
    }
    if transport.is_null() { return Ok(()); }
    let values = transport.as_object().ok_or("transport 必须是对象")?;
    for (key,value) in values {
        let valid = match (adapter.adapter_id.as_str(),key.as_str()) {
            ("pi.rpc" | "omp.rpc_v2" | "omp.rpc","session_dir") => value.is_string(),
            ("codex.app_server","experimental_api") => value.is_boolean(),
            ("codex.app_server","thread_section_name") => value.is_string(),
            ("claude.sdk","permission_mode") => value.as_str().is_some_and(|mode|["default","acceptEdits","plan","bypassPermissions","dontAsk","auto"].contains(&mode)),
            ("grok.acp","mcp_ready_wait_seconds") => value.as_f64().is_some_and(|seconds|seconds.is_finite() && (0.0..=30.0).contains(&seconds)),
            _ => value.is_null() || value == "",
        };
        if !valid { return Err(format!("{} 的 transport.{key} 无效",adapter.adapter_id)); }
    }
    Ok(())
}

pub(crate) fn transport_args(adapter_id: &str, transport: &Value) -> Vec<String> {
    if matches!(adapter_id,"pi.rpc" | "omp.rpc_v2" | "omp.rpc") {
        if let Some(directory) = transport["session_dir"].as_str().map(str::trim).filter(|value|!value.is_empty()) {
            return vec!["--session-dir".into(),directory.into()];
        }
    }
    Vec::new()
}

pub(crate) fn transport_env(adapter_id: &str, transport: &Value, endpoint: &str) -> Vec<(String,String)> {
    let mut env = Vec::new();
    if matches!(adapter_id,"kimi.local_server" | "opencode.server") && !endpoint.is_empty() {
        env.push(("MUTEKI_ADAPTER_ENDPOINT".into(),endpoint.into()));
    }
    if adapter_id == "codex.app_server" {
        env.push(("MUTEKI_CODEX_THREAD_SECTION".into(), transport["thread_section_name"].as_str().unwrap_or("").trim().into()));
    }
    let field = match adapter_id {
        "codex.app_server" => Some(("experimental_api","MUTEKI_CODEX_EXPERIMENTAL_API")),
        "claude.sdk" => Some(("permission_mode","MUTEKI_CLAUDE_PERMISSION_MODE")),
        "grok.acp" => Some(("mcp_ready_wait_seconds","MUTEKI_GROK_MCP_READY_WAIT_SECONDS")),
        _ => None,
    };
    if let Some((field,key)) = field {
        if let Some(value) = transport.get(field).filter(|value|!value.is_null()) {
            env.push((key.into(),value.as_str().map(str::to_owned).unwrap_or_else(||value.to_string())));
        }
    }
    env
}

fn normalized(body: &Value, existing: Option<&Value>, engine: &EngineDescriptor, adapter: &AdapterDescriptor, instance_id: &str) -> Result<Value, String> {
    let base = existing.cloned().unwrap_or_else(|| json!({}));
    let pick = |name: &str| -> Value { body.get(name).cloned().unwrap_or_else(|| base[name].clone()) };
    let endpoint = body
        .get("adapter_endpoint")
        .or_else(|| body.get("endpoint"))
        .cloned()
        .unwrap_or_else(|| base["endpoint"].clone());
    let endpoint = endpoint.as_str().unwrap_or("").trim().to_owned();
    let transport = pick("transport");
    validate_transport(engine, adapter, &transport, &endpoint)?;
    let now = now();
    Ok(json!({
        "adapter_id":adapter.adapter_id,"instance_id":instance_id,"key":key_of(&adapter.adapter_id, instance_id),
        "engine":engine.engine(),
        "label":pick("label").as_str().map(str::trim).filter(|s| !s.is_empty()).unwrap_or(&key_of(&adapter.adapter_id, instance_id)),
        "binary_path":pick("binary_path").as_str().unwrap_or("").trim(),
        "adapter_endpoint":endpoint,"endpoint":endpoint,
        "env_refs":env_refs(&pick("env_refs"))?,
        "enabled":pick("enabled").as_bool().unwrap_or(true),
        "transport":transport,
        "launch_args":launch_args(&pick("launch_args"), adapter)?,
        "created_at":base["created_at"].as_str().filter(|s| !s.is_empty()).map(str::to_owned).unwrap_or_else(|| now.clone()),
        "updated_at":now,
    }))
}


pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/agent-runtimes", get(list).post(upsert))
        .route("/api/agent-runtimes/descriptors", get(descriptors_view))
        .route("/api/agent-runtimes/refresh", post(refresh))
        .route("/api/agent-runtimes/profiles", get(profiles).post(upsert_profile))
        .route("/api/agent-runtimes/profiles/{name}", axum::routing::delete(remove_profile))
        .route("/api/agent-runtimes/{key}", get(get_one).delete(remove))
        .route("/api/agent-runtimes/{key}/probe", post(probe))
        .route("/api/agent-runtimes/{key}/capability-refresh", post(capability_refresh))
        .route("/api/agent-runtimes/{key}/login", post(login))
        .route("/api/agent-runtimes/{key}/logout", post(logout))
        .route("/api/agent-runtimes/{key}/auth-refresh", post(auth_refresh))
}

type Params = Query<HashMap<String, String>>;

fn flag(query: &HashMap<String, String>, name: &str, default: bool) -> bool {
    query
        .get(name)
        .map(|v| !matches!(v.trim().to_ascii_lowercase().as_str(), "0" | "false" | "no" | "off"))
        .unwrap_or(default)
}

pub(crate) async fn list(State(state): State<Arc<AppState>>, Query(query): Params) -> Result<Json<Value>, Reply> {
    let transport = query.get("transport").map(|v| v.trim().to_ascii_lowercase()).unwrap_or_default();
    if !["", "cli", "structured"].contains(&transport.as_str()) {
        return Err(fail(StatusCode::BAD_REQUEST, "runtime.instance.invalid", format!("未知 Runtime transport：{transport}"), "validation"));
    }
    let adapter = query.get("adapter_id").cloned().unwrap_or_default();
    let instances = list_views(&state, &adapter, flag(&query, "discovered", true), &transport)?;
    let probed_at = instances
        .iter()
        .filter_map(|row| row["health"]["probed_at"].as_str())
        .max()
        .map(str::to_owned);
    Ok(Json(json!({"count":instances.len(),"instances":instances,"probed_at":probed_at,"transport_kind":transport})))
}

async fn get_one(State(state): State<Arc<AppState>>, Path(key): Path<String>) -> Result<Json<Value>, Reply> {
    let (adapter, instance_id) = parse_key(&state, &key);
    view_of(&state, &adapter, &instance_id)?
        .map(Json)
        .ok_or_else(|| fail(StatusCode::NOT_FOUND, "runtime.instance.not_found", format!("unknown runtime instance: {key}"), "not_found"))
}

fn capability_row(view: &Value, engine: &EngineDescriptor, adapter: &AdapterDescriptor) -> Value {
    let probed = view["health"]["capabilities"].as_object().filter(|c| !c.is_empty()).map(|c| json!(c));
    let mut capabilities = probed.clone().unwrap_or_else(|| declared_capabilities(engine, adapter));
    capabilities["plan_mode"] = json!(capabilities["plan_mode"] == true
        && crate::native_chat::confirmed_plan_adapter(&adapter.adapter_id)
        && (adapter.adapter_id != "codex.app_server" || (view["transport"]["experimental_api"] == true
            && view["updated_at"] == view["health"]["configuration_updated_at"])));
    let sources: serde_json::Map<String, Value> = capabilities
        .as_object()
        .into_iter()
        .flatten()
        .map(|(name, _)| {
            let probe = probed.is_some() && view["health"]["field_sources"][name] == "probe";
            (name.clone(), json!(if probe { "probe" } else { "static" }))
        })
        .collect();
    json!({"key":view["key"],"adapter_id":adapter.adapter_id,"engine":engine.engine(),"role":adapter.role,
        "configured":view["configured"],"probed_at":view["health"]["probed_at"].as_str().unwrap_or(""),
        "capabilities":capabilities,"capability_sources":sources,"native_rewind":adapter.native_rewind,
        "capability_gateway":adapter.capability_gateway,"access_mode_notes":adapter.access_mode_notes})
}

pub(crate) async fn descriptors_view(State(state): State<Arc<AppState>>, Query(query): Params) -> Result<Json<Value>, Reply> {
    let selected = query.get("engine").map(|v| v.trim().to_ascii_lowercase()).unwrap_or_default();
    if !selected.is_empty() && descriptors::find(&selected).is_none() {
        return Err(fail(StatusCode::NOT_FOUND, "provider.descriptor_unknown", format!("no provider descriptor for engine {selected}"), "not_found"));
    }
    let wanted = |engine: &str| selected.is_empty() || selected == engine;
    let mut out = Vec::new();
    for engine in descriptors::all().iter().filter(|e| wanted(e.engine())) {
        let mut public = engine.public().clone();
        for adapter in public["adapters"].as_array_mut().into_iter().flatten() {
            let id = adapter["adapter_id"].as_str().unwrap_or("").to_owned();
            if descriptors::for_adapter(&id).is_some_and(|(_, row)| row.native.is_some()) {
                adapter["capabilities"]["plan_mode"] = json!(adapter["capabilities"]["plan_mode"] == true
                    && crate::native_chat::confirmed_plan_adapter(&id));
            }
        }
        out.push(public);
    }
    let mut instances = Vec::new();
    for view in list_views(&state, "", true, "")? {
        match target(text(&view, "adapter_id")) {
            Some((_, Target::Native(engine, adapter))) if wanted(engine.engine()) => {
                instances.push(capability_row(&view, engine, adapter));
            }
            None if extension_adapter(&state, text(&view, "adapter_id")) => {
                instances.push(json!({"key":view["key"],"adapter_id":view["adapter_id"],"engine":view["engine"],
                    "instance_id":"default","role":"default","configured":view["configured"],
                    "probed_at":view["health"]["checked_at"],
                    "capabilities":view["health"]["capabilities"],"capability_sources":{},
                    "native_rewind":false,"capability_gateway":false,"access_mode_notes":{}}));
            }
            _ => {}
        }
    }
    Ok(Json(json!({"schema_version":descriptors::DESCRIPTOR_SCHEMA_VERSION,"descriptors":out,"count":instances.len(),"instances":instances})))
}

fn profile_name(row: &Value) -> &str {
    row["name"].as_str().or_else(|| row["id"].as_str()).unwrap_or("").trim()
}

fn profile_error(code: &str, message: impl Into<String>, category: &str) -> Outcome {
    Outcome::failed(error_value(code, message, category, false))
}

fn worker_config(state: &AppState) -> Result<Value, Reply> {
    crate::settings_metadata::configuration(state).map_err(|(_, Json(body))| {
        tracing::error!(error = %body, "Worker Profile settings failed");
        storage("Worker template settings unavailable")
    })
}

fn profiles_in(config: &Value) -> Vec<Value> {
    config["worker_profiles"].as_array().cloned().unwrap_or_default()
}

fn listed_profiles(state: &AppState) -> Result<Value, Reply> {
    let profiles = profiles_in(&worker_config(state)?);
    let cli = list_views(state, "", true, "cli")?;
    let mut out = Vec::with_capacity(profiles.len());
    for mut row in profiles {
        let engine = text(&row, "engine");
        let key = format!("cli.{engine}:default");
        let binding = cli.iter().find(|view| view["key"] == key);
        let legacy_ref = text(&row, "runtime_instance_ref");
        let reason = if binding.is_none() {
            format!("缺少 {key} CLI Worker")
        } else if !legacy_ref.is_empty() && legacy_ref != key {
            "旧 runtime_instance_ref 已忽略；做题模式固定使用同引擎 CLI".to_owned()
        } else {
            String::new()
        };
        let selected = binding.map(|view| text(view, "key")).unwrap_or("");
        row["runtime_binding"] = binding.map(|view| json!({
            "key": view["key"], "adapter_id": view["adapter_id"], "instance_id": view["instance_id"],
            "enabled": view["enabled"], "source": "worker_cli_default", "transport_kind": "cli",
            "health": view["health"], "auth": view["auth"],
            "capabilities": view["health"]["capabilities"],
            "capability_injection": view["health"]["capability_injection"],
        })).unwrap_or(Value::Null);
        row["runtime_binding_reason"] = json!(reason);
        row["runtime_resolution"] = json!({"requested":key,"selected":selected,
            "reason":reason,"fallback_order":["worker_cli_default"]});
        out.push(row);
    }
    Ok(json!({"profiles":out,"count":out.len()}))
}

pub(crate) async fn profiles(State(state): State<Arc<AppState>>) -> Result<Json<Value>, Reply> {
    listed_profiles(&state).map(Json)
}

fn profile_input(body: &Value) -> Result<Value, Reply> {
    let Some(_) = body.as_object() else {
        return Err(fail(StatusCode::BAD_REQUEST, "worker_profile.invalid", "Worker Profile 必须是对象", "validation"));
    };
    let name = profile_name(body);
    if name.is_empty() {
        return Err(fail(StatusCode::BAD_REQUEST, "worker_profile.invalid", "Worker Profile 需要有效 name 或 id", "validation"));
    }
    if body["enabled"] == false {
        return Err(fail(StatusCode::BAD_REQUEST, "worker_profile.invalid", "停用 Worker 请在身份配置中操作", "validation"));
    }
    let raw_engine = if !text(body, "transport").is_empty() { text(body, "transport") } else { text(body, "engine") };
    let engine = match raw_engine {
        "claude_code" => "claude", "codex_cli" => "codex", "cursor_agent" => "cursor",
        "oh_my_pi" | "oh-my-pi" | "ohmypi" => "omp", "kimi_code" | "kimi-code" => "kimi",
        "grok_build" => "grok", "opencode_cli" | "opencode-cli" => "opencode",
        "devin_cli" => "devin", "droid_cli" | "factory_droid" => "droid", other => other,
    };
    let Some(descriptor) = descriptors::find(engine) else {
        return Err(fail(StatusCode::BAD_REQUEST, "worker_profile.invalid", "Worker Profile 需要有效 engine/transport", "validation"));
    };
    if descriptor.identity.cli_adapter_id.is_empty() {
        return Err(fail(StatusCode::BAD_REQUEST, "worker_profile.chat_only", "该 Agent 仅支持聊天，不能用作 Worker", "validation"));
    }
    if descriptor.identity.support_status != "supported" {
        return Err(fail(StatusCode::CONFLICT, "engine.temporarily_unsupported", format!("{} 暂不支持", descriptor.identity.display_name), "state"));
    }
    let mut roles: Vec<Value> = body["roles"].as_array().into_iter().flatten()
        .filter_map(|value| value.as_str().map(str::trim).filter(|role| !role.is_empty() && *role != "race").map(|role| json!(role)))
        .collect();
    let mut implicit_roles = Vec::new();
    if roles.is_empty() {
        roles = ["bootstrap","explore","respond","review","verifier"].into_iter().map(|role| json!(role)).collect();
        implicit_roles = vec![json!("review"),json!("verifier")];
    } else if roles.iter().any(|role| ["bootstrap","explore","respond"].iter().any(|ordinary| role == ordinary)) {
        for fallback in ["review","verifier"] {
            if !roles.iter().any(|role| role == fallback) {
                roles.push(json!(fallback));
                implicit_roles.push(json!(fallback));
            }
        }
    }
    let mut profile = json!({
        "id":name,"name":name,"label":if text(body,"label").is_empty(){name}else{text(body,"label")},
        "engine":engine,"transport":raw_engine,"credential_mode":if text(body,"credential_mode").is_empty(){if text(body,"auth").is_empty(){"subscription"}else{text(body,"auth")}}else{text(body,"credential_mode")},
        "auth":if text(body,"credential_mode").is_empty(){if text(body,"auth").is_empty(){"subscription"}else{text(body,"auth")}}else{text(body,"credential_mode")},
        "credential_account":if !text(body,"credential_account").is_empty(){text(body,"credential_account")}else{text(body,"credential_account_ref")},
        "credential_id":text(body,"credential_id"),"credential_kind":text(body,"credential_kind"),
        "credential_seat_ref":text(body,"credential_seat_ref"),"provider_id":text(body,"provider_id"),
        "api_key_ref":text(body,"api_key_ref"),"base_url":text(body,"base_url"),"wire_api":text(body,"wire_api"),
        "roles":roles,"implicit_roles":implicit_roles,"enabled":true,
        "execution_mode":if text(body,"execution_mode").is_empty(){"step"}else{text(body,"execution_mode")},
        "model":text(body,"model"),"reasoning_effort":if text(body,"reasoning_effort").is_empty(){"default"}else{text(body,"reasoning_effort")},
        "runtime_instance_ref":"",
    });
    if !matches!(text(&profile,"execution_mode"), "step" | "campaign") {
        return Err(fail(StatusCode::BAD_REQUEST, "worker_profile.invalid", "execution_mode 必须是 step 或 campaign", "validation"));
    }
    if !matches!(text(&profile,"reasoning_effort"), "default" | "none" | "minimal" | "low" | "medium" | "high" | "xhigh" | "max") {
        profile["reasoning_effort"] = json!("default");
    }
    for (key, default) in [("max_running",1_u64),("max_review_running",0),("max_verifier_running",0),
        ("priority",100),("soft_reminder_seconds",600),("max_continuations",3)] {
        let value = body.get(key).and_then(|v|v.as_u64().or_else(||v.as_str().and_then(|s|s.trim().parse::<u64>().ok()))).unwrap_or(default);
        profile[key] = json!(value);
    }
    for key in ["context_window_tokens","max_output_tokens"] {
        if let Some(value) = body.get(key) {
            let Some(n) = value.as_u64().or_else(||value.as_str().and_then(|s|s.trim().parse::<u64>().ok())).filter(|n| *n > 0) else { return Err(fail(StatusCode::BAD_REQUEST,"worker_profile.invalid",format!("{key} 必须是正整数"),"validation")); };
            profile[key] = json!(n);
        }
    }
    profile["unlimited_workers"] = json!(body["unlimited_workers"].as_bool().unwrap_or(false));
    let credential_id = text(&profile,"credential_id").to_owned();
    if !credential_id.is_empty() {
        let canonical = credentials::canonical(&credential_id, engine)
            .map_err(|err| fail(StatusCode::BAD_REQUEST,"worker_profile.invalid",err.message,"validation"))?;
        profile["credential_id"] = json!(canonical.id());
        if text(&profile,"credential_account").is_empty() {
            if let credentials::CredentialRef::Account(account) = canonical {
                profile["credential_account"] = json!(account);
            }
        }
    } else if !text(&profile,"credential_account").is_empty() {
        profile["credential_id"] = json!(credentials::canonical(text(&profile,"credential_account"),engine)
            .map_err(|err| fail(StatusCode::BAD_REQUEST,"worker_profile.invalid",err.message,"validation"))?.id());
    } else if text(&profile,"credential_kind") == "system_inherit" {
        profile["credential_id"] = json!(format!("system:{engine}"));
    } else {
        let account = format!("{engine}-main");
        profile["credential_account"] = json!(account);
        profile["credential_id"] = json!(format!("account:{account}"));
    }
    Ok(profile)
}

fn profile_record(state: &AppState, id: &str, kind: &str, name: &str, request: &Value, outcome: Outcome) -> Response {
    let status = status_of(&outcome);
    let receipt = json!({"command_id":id,"receipt_id":id,"command_type":kind,
        "state":if outcome.failed{"failed"}else{"completed"},"aggregate":{"type":"profile","id":name},
        "run_id":null,"event_cursor":null,"output":outcome.output,"error":outcome.error,
        "deduplicated":false,"completed_at":now()});
    if let Err(error) = state.store.put(COMMANDS,id,&json!({"request":request,"response":{"receipt":receipt},"status":status.as_u16()})) {
        return storage(error).into_response();
    }
    (status,Json(json!({"receipt":receipt}))).into_response()
}

fn profile_replay(state: &AppState, id: &str, request: &Value) -> Result<Option<Response>, Reply> {
    let Some(saved) = state.store.get(COMMANDS,id).map_err(storage)? else { return Ok(None); };
    if saved["request"] != *request {
        return Err(fail(StatusCode::CONFLICT,"command.idempotency_conflict","command_id 已用于不同请求","conflict"));
    }
    Ok(prior(state,id))
}

fn profile_command_id(state: &AppState, requested: &str, key: &str, request: &Value) -> Result<String, Reply> {
    if key.is_empty() {
        return Ok(if requested.is_empty(){command_id(&Value::Null)}else{requested.to_owned()});
    }
    let saved = state.store.get(PROFILE_IDEMPOTENCY,key).map_err(storage)?;
    if let Some(saved) = saved {
        if saved["request"] != *request {
            return Err(fail(StatusCode::CONFLICT,"command.idempotency_conflict","idempotency_key 已用于不同请求","conflict"));
        }
        let id = text(&saved,"command_id");
        return Ok(id.to_owned());
    }
    let id = if requested.is_empty(){command_id(&Value::Null)}else{requested.to_owned()};
    Ok(id)
}

fn claim_profile_key(state: &AppState, key: &str, id: &str, request: &Value) -> Result<(), Reply> {
    if !key.is_empty() {
        state.store.put_if_absent(PROFILE_IDEMPOTENCY,key,&json!({"command_id":id,"request":request})).map_err(storage)?;
    }
    Ok(())
}

pub(crate) async fn upsert_profile(State(state): State<Arc<AppState>>, body: Option<Json<Value>>) -> Response {
    let Some(Json(body)) = body else { return fail(StatusCode::BAD_REQUEST,"worker_profile.invalid","Worker Profile 必须是对象","validation").into_response(); };
    let mut profile = match profile_input(&body) { Ok(profile) => profile, Err(error) => return error.into_response() };
    let name = profile_name(&profile).to_owned();
    let expected = match body.get("expected_revision") {
        Some(Value::String(value)) => Some(value.as_str()), None | Some(Value::Null) => None,
        _ => return fail(StatusCode::BAD_REQUEST,"worker_profile.invalid","expected_revision 必须是字符串","validation").into_response(),
    };
    let request = json!({"command_type":"worker_profile.upsert","profile":profile,"expected_revision":expected});
    let key = match body.get("idempotency_key") {
        Some(Value::String(value)) => value.as_str(), None | Some(Value::Null) => "",
        _ => return fail(StatusCode::BAD_REQUEST,"worker_profile.invalid","idempotency_key 必须是字符串","validation").into_response(),
    };
    let _guard = PROFILE_WRITE_LOCK.lock().await;
    let id = match profile_command_id(&state,text(&body,"command_id"),key,&request) {
        Ok(value) => value, Err(error) => return error.into_response(),
    };
    match profile_replay(&state,&id,&request) { Ok(Some(response)) => return response, Err(error) => return error.into_response(), Ok(None) => {} }
    if let Err(error) = claim_profile_key(&state,key,&id,&request) { return error.into_response(); }
    let mut config = match worker_config(&state) { Ok(config) => config, Err(error) => return error.into_response() };
    let mut rows = profiles_in(&config);
    let current = rows.iter().find(|row| profile_name(row) == name);
    if expected.is_some_and(|value| value != current.map(|row| text(row,"revision")).unwrap_or("")) {
        return profile_record(&state,&id,"worker_profile.upsert",&name,&request,
            profile_error("worker_profile.revision_conflict","Worker Profile 已变化，请刷新后重试","conflict"));
    }
    profile["revision"] = json!(uuid::Uuid::new_v4().to_string());
    profile["updated_at"] = json!(now());
    rows.retain(|row| profile_name(row) != name);
    rows.push(profile.clone());
    config["worker_profiles"] = json!(rows);
    if let Err(error) = state.store.put("settings","worker_templates",&config) { return storage(error).into_response(); }
    profile_record(&state,&id,"worker_profile.upsert",&name,&request,Outcome::done(json!({"profile":profile})))
}

pub(crate) async fn remove_profile(State(state): State<Arc<AppState>>, Path(name): Path<String>, Query(query): Params) -> Response {
    let name = name.trim();
    if name.is_empty() { return fail(StatusCode::BAD_REQUEST,"worker_profile.invalid","Worker Profile name 不能为空","validation").into_response(); }
    let expected = query.get("expected_revision").map(String::as_str);
    let request = json!({"command_type":"worker_profile.remove","name":name,"expected_revision":expected});
    let key = query.get("idempotency_key").map(String::as_str).unwrap_or("");
    let _guard = PROFILE_WRITE_LOCK.lock().await;
    let id = match profile_command_id(&state,query.get("command_id").map(String::as_str).unwrap_or(""),key,&request) {
        Ok(value) => value, Err(error) => return error.into_response(),
    };
    match profile_replay(&state,&id,&request) { Ok(Some(response)) => return response, Err(error) => return error.into_response(), Ok(None) => {} }
    if let Err(error) = claim_profile_key(&state,key,&id,&request) { return error.into_response(); }
    let mut config = match worker_config(&state) { Ok(config) => config, Err(error) => return error.into_response() };
    let mut rows = profiles_in(&config);
    let current = rows.iter().find(|row| profile_name(row) == name);
    let Some(current) = current else { return profile_record(&state,&id,"worker_profile.remove",name,&request,
        profile_error("worker_profile.not_found",format!("unknown worker profile: {name}"),"not_found")); };
    if expected.is_some_and(|value| value != text(current,"revision")) {
        return profile_record(&state,&id,"worker_profile.remove",name,&request,
            profile_error("worker_profile.revision_conflict","Worker Profile 已变化，请刷新后重试","conflict"));
    }
    if rows.len() == 1 {
        return profile_record(&state,&id,"worker_profile.remove",name,&request,
            profile_error("worker_profile.invalid","不能删除最后一个 Worker Profile","validation"));
    }
    rows.retain(|row| profile_name(row) != name);
    config["worker_profiles"] = json!(rows);
    if let Err(error) = state.store.put("settings","worker_templates",&config) { return storage(error).into_response(); }
    profile_record(&state,&id,"worker_profile.remove",name,&request,Outcome::done(json!({"name":name})))
}

fn body_of(body: Option<Json<Value>>) -> Value {
    body.map(|Json(v)| v).filter(Value::is_object).unwrap_or_else(|| json!({}))
}

async fn managed_runtime(state: Arc<AppState>, kind: &str, key: &str, body: Value) -> Response {
    if !body.is_object() { return fail(StatusCode::BAD_REQUEST,"runtime.instance.invalid","body must be an object","validation").into_response(); }
    let mut payload = body.clone();
    for field in ["command_id","idempotency_key","expected_version","actor"] { payload.as_object_mut().unwrap().remove(field); }
    let mut envelope = json!({"command_type":kind,"aggregate_type":"runtime","aggregate_id":key,"payload":payload});
    for field in ["command_id","idempotency_key","expected_version","actor"] {
        if let Some(value) = body.get(field) { envelope[field] = value.clone(); }
    }
    crate::operations_api::dispatch_local_command(state,envelope).await
}
async fn upsert(State(state): State<Arc<AppState>>, body: Option<Json<Value>>) -> Response {
    let body = body_of(body);
    let key = key_of(text(&body,"adapter_id"),if text(&body,"instance_id").is_empty(){"default"}else{text(&body,"instance_id")});
    managed_runtime(state,"runtime.instance.upsert",&key,body).await
}
async fn refresh(State(state): State<Arc<AppState>>, body: Option<Json<Value>>) -> Response {
    let body = body_of(body);
    let key = [text(&body,"adapter_id"),text(&body,"transport_kind"),text(&body,"transport")].into_iter().find(|value|!value.is_empty()).unwrap_or("all").to_owned();
    managed_runtime(state,"runtime.instance.refresh",&key,body).await
}

pub(crate) fn requested_event(envelope: &Value, kind: &str, id: &str) -> Option<Value> {
    let command = text(envelope,"command_type");
    let suffix = match command {
        "runtime.instance.probe" | "runtime.instance.capability.refresh" => "probe_requested",
        "runtime.instance.refresh" => "refresh_requested",
        "runtime.instance.login" => "login_requested",
        "runtime.instance.logout" => "logout_requested",
        "runtime.instance.auth.refresh" => "auth_refresh_requested",
        _ => return None,
    };
    Some(crate::operations_api::domain_events::event(envelope,kind,id,&format!("core.runtime.instance.{suffix}"),
        "platform.agent_runtime",json!({"runtime_key":id,"command_type":command})))
}

async fn upsert_impl(State(state): State<Arc<AppState>>, body: Option<Json<Value>>) -> Response {
    let body = body_of(body);
    let id = command_id(&body);
    if let Some(prior) = prior(&state, &id) {
        return prior;
    }
    let instance_id = match text(&body, "instance_id") {
        "" => "default".to_owned(),
        value => value.to_owned(),
    };
    let requested = text(&body, "adapter_id");
    let aggregate = key_of(requested, &instance_id);
    let invalid = |message: String| Outcome::failed(error_value("runtime.instance.invalid", message, "validation", false));
    if instance_id.contains([':', '/', ' ', '\t', '\n']) {
        return record(&state, &id, "runtime.instance.upsert", &aggregate, invalid(format!("非法 instance_id：{instance_id}")), None);
    }
    let Some((adapter_id, resolved)) = target(requested) else {
        return record(&state, &id, "runtime.instance.upsert", &aggregate,
            invalid(format!("未知 Runtime adapter：{requested}，需要结构化 Adapter ID 或 cli.<engine> compatibility transport")), None);
    };
    let key = key_of(&adapter_id, &instance_id);
    let existing = match state.store.get(INSTANCES, &key) {
        Ok(existing) => existing,
        Err(error) => return storage(error).into_response(),
    };
    if body.get("expected_revision").is_some_and(|v| !v.is_null()) {
        let expected = text(&body, "expected_revision");
        let current = existing.as_ref().map(|row| text(row, "updated_at")).unwrap_or("");
        if expected != current {
            return record(&state, &id, "runtime.instance.upsert", &key, Outcome::failed(error_value(
                "runtime.instance.revision_conflict", "运行配置已被其它视图修改，请刷新后重试。", "conflict", false)), None);
        }
    }
    let cfg = match resolved {
        Target::Native(engine, adapter) => {
            if engine.identity.support_status != "supported" {
                return record(&state, &id, "runtime.instance.upsert", &key, Outcome::failed(error_value(
                    "engine.temporarily_unsupported", format!("{} 暂不支持", engine.identity.display_name), "state", false)), None);
            }
            match normalized(&body, existing.as_ref(), engine, adapter, &instance_id) {
                Ok(cfg) => cfg,
                Err(message) => return record(&state, &id, "runtime.instance.upsert", &key, invalid(message), None),
            }
        }
    };
    record_mutation(&state,&id,"runtime.instance.upsert",&key,Outcome::done(json!({"instance":cfg})),None,
        &[(INSTANCES,key.as_str(),existing.as_ref())],&[(INSTANCES,key.as_str(),&cfg)],&[])
}

async fn remove(State(state): State<Arc<AppState>>, Path(key): Path<String>) -> Response {
    let id = command_id(&json!({}));
    managed_runtime(state,"runtime.instance.remove",&key,json!({"command_id":id})).await
}

async fn remove_with_id(state: Arc<AppState>, key: String, id: String) -> Response {
    if let Some(prior) = prior(&state,&id) { return prior; }
    let (adapter, instance_id) = parse_key(&state, &key);
    let key = key_of(&adapter, &instance_id);
    let existing = match state.store.get(INSTANCES,&key) {
        Ok(Some(row)) => row,
        Ok(None) => return record(&state,&id,"runtime.instance.remove",&key,Outcome::failed(error_value(
            "runtime.instance.not_found",format!("unknown runtime instance: {key}"),"not_found",false)),None),
        Err(error) => return storage(error).into_response(),
    };
    record_mutation(&state,&id,"runtime.instance.remove",&key,Outcome::done(json!({"key":key})),None,
        &[(INSTANCES,key.as_str(),Some(&existing))],&[],&[(INSTANCES.to_owned(),key.clone()),(HEALTH.to_owned(),key.clone())])
}

pub(crate) fn supports_command(kind: &str) -> bool {
    matches!(kind,"runtime.instance.upsert" | "runtime.instance.remove" | "runtime.instance.probe" |
        "runtime.instance.refresh" | "runtime.instance.login" | "runtime.instance.logout" |
        "runtime.instance.auth.refresh" | "runtime.instance.capability.refresh")
}

pub(crate) async fn dispatch_command_envelope(state: Arc<AppState>, envelope: &Value) -> Response {
    let kind = text(envelope,"command_type");
    let mut payload = envelope["payload"].clone();
    if !payload.is_object() { payload = json!({}); }
    payload["command_id"] = envelope["command_id"].clone();
    let target = text(envelope,"aggregate_id");
    let key = if !target.is_empty() { target.to_owned() } else {
        key_of(text(&payload,"adapter_id"),
            if text(&payload,"instance_id").is_empty(){"default"}else{text(&payload,"instance_id")})
    };
    match kind {
        "runtime.instance.upsert" => {
            if text(&payload,"adapter_id").is_empty() {
                let (adapter, instance_id) = parse_key(&state,&key);
                payload["adapter_id"] = json!(adapter);
                payload["instance_id"] = json!(instance_id);
            }
            upsert_impl(State(state),Some(Json(payload))).await
        }
        "runtime.instance.remove" => remove_with_id(state,key,command_id(&payload)).await,
        "runtime.instance.probe" => run_probe(state,key,payload,"runtime.instance.probe").await,
        "runtime.instance.capability.refresh" => run_probe(state,key,payload,"runtime.instance.capability.refresh").await,
        "runtime.instance.refresh" => refresh_impl(State(state),Some(Json(payload))).await,
        "runtime.instance.login" => auth_command(state,key,payload,"login").await,
        "runtime.instance.logout" => auth_command(state,key,payload,"logout").await,
        "runtime.instance.auth.refresh" => auth_command(state,key,payload,"auth.refresh").await,
        _ => fail(StatusCode::NOT_IMPLEMENTED,"capability.unavailable","Runtime command unavailable","unsupported").into_response(),
    }
}

/// The instance a command addresses, or the failed outcome that says why not.
fn addressed(state: &AppState, key: &str) -> Result<(String, String, Option<Target>), Outcome> {
    let (adapter, instance_id) = parse_key(state, key);
    if instance_id == "default" && extension_adapter(state,&adapter) {
        return Ok((adapter,instance_id,None));
    }
    let Some((adapter_id, resolved)) = target(&adapter) else {
        return Err(Outcome::failed(error_value("runtime.instance.invalid", format!("未知 Runtime adapter：{adapter}"), "validation", false)));
    };
    match view_of(state, &adapter_id, &instance_id) {
        Ok(Some(_)) => Ok((adapter_id, instance_id, Some(resolved))),
        Ok(None) => Err(Outcome::failed(error_value("runtime.instance.not_found", format!("unknown runtime instance: {key}"), "not_found", false))),
        Err(_) => Err(Outcome::failed(error_value("runtime.storage_error", "Runtime 设置无法读写本地存储", "internal", true))),
    }
}

async fn probe(State(state): State<Arc<AppState>>, Path(key): Path<String>, body: Option<Json<Value>>) -> Response {
    managed_runtime(state,"runtime.instance.probe",&key,body_of(body)).await
}

async fn capability_refresh(State(state): State<Arc<AppState>>, Path(key): Path<String>) -> Response {
    managed_runtime(state,"runtime.instance.capability.refresh",&key,json!({})).await
}

async fn run_probe(state: Arc<AppState>, key: String, body: Value, command_type: &str) -> Response {
    let id = command_id(&body);
    if let Some(prior) = prior(&state, &id) {
        return prior;
    }
    let (adapter_id, instance_id, resolved) = match addressed(&state, &key) {
        Ok(found) => found,
        Err(outcome) => return record(&state, &id, command_type, &key, outcome, None),
    };
    let key = key_of(&adapter_id, &instance_id);
    if resolved.is_none() && extension_adapter(&state,&adapter_id) {
        let request = json!({"schema_version":1,"runtime_instance_id":instance_id,
            "include_models":true,"options":{}});
        let response = crate::extensions::invoke_capability(&state,"external-agent-adapter",
            &adapter_id,"probe",json!({"request":request})).await;
        let result = match response {
            Ok(value) => value,
            Err(error) => return record(&state,&id,command_type,&key,
                Outcome::failed(error_value("runtime.instance.probe_failed",error,"runtime",true)),None),
        };
        if result["error"].is_object() {
            return record(&state,&id,command_type,&key,
                Outcome::failed(error_value("runtime.instance.probe_failed",
                    text(&result["error"],"message"),"runtime",true)),None);
        }
        let capabilities = result.get("capabilities").unwrap_or(&result);
        if !capabilities.is_object() {
            return record(&state,&id,command_type,&key,
                Outcome::failed(error_value("runtime.instance.probe_failed","Extension probe response must contain capabilities","runtime",false)),None);
        }
        let version = extension_entry(&state,&adapter_id).map(|entry|entry["extension_version"].clone()).unwrap_or(Value::Null);
        let health = json!({"healthy":true,"source":"extension_probe","detail":"Extension adapter probe completed",
            "extension_version":version,
            "capabilities":capabilities,"auth":not_applicable(),"checked_at":chrono::Utc::now().to_rfc3339()});
        if let Err(error) = state.store.put(HEALTH,&key,&health) { return storage(error).into_response(); }
        let view = view_of(&state,&adapter_id,&instance_id).ok().flatten();
        return record(&state,&id,command_type,&key,Outcome::done(json!({"key":key,"health":health})),
            view.map(|view|("instance",view)));
    }
    let Some(Target::Native(engine, adapter)) = resolved else {
        return record(&state, &id, command_type, &key, Outcome::failed(error_value(
            "runtime.instance.invalid", format!("未知 Runtime adapter：{adapter_id}"), "validation", false)), None);
    };
    let environment = body.get("environment").cloned().unwrap_or(json!("local"));
    let credential = body.get("credential_id").cloned().unwrap_or(json!(""));
    if environment != "local" || !credential.is_string() {
        return record(&state, &id, command_type, &key, Outcome::failed(error_value("runtime.instance.probe_scope_invalid",
            "credential_id must be a string and environment must be local", "validation", false)), None);
    }
    let credential = credential.as_str().unwrap_or("").trim().to_owned();
    if !credential.is_empty() {
        if let Err(error) = credentials::canonical(&credential, engine.engine()) {
            return record(&state, &id, command_type, &key, Outcome::failed(runtime_error(&error, "validation")), None);
        }
        if !adapter.scoped_model_catalog_probe {
            let supported: Vec<&str> = descriptors::all()
                .iter()
                .flat_map(|e| e.adapters.iter())
                .filter(|a| a.scoped_model_catalog_probe)
                .map(|a| a.adapter_id.as_str())
                .collect();
            return record(&state, &id, command_type, &key, Outcome::failed(error_value("runtime.instance.probe_scope_invalid",
                format!("Scoped model catalog probing is supported by {}", supported.join(", ")), "validation", false)), None);
        }
    }
    let health = match probe_and_save(&state, engine, adapter, &instance_id, true).await {
        Ok(health) => health,
        Err(reply) => return reply.into_response(),
    };
    let mut output = json!({"key":key,"health":health});
    let mut outcome = None;
    if !credential.is_empty() {
        match crate::model_settings_api::refresh_models(State(state.clone()), Path(credential.clone()),
            Some(Json(json!({"runtime_instance":key})))).await {
            Ok(Json(result)) => {
                output["model_catalog"] = result["catalog"].clone();
                if result["ok"] != true {
                    outcome = Some(Outcome::failed(error_value(
                        result["error_code"].as_str().filter(|c| !c.is_empty()).unwrap_or("model_catalog.request_failed"),
                        text(&result, "detail"), "runtime", true)));
                }
            }
            Err(reply) => return reply.into_response(),
        }
    }
    let view = view_of(&state, &adapter_id, &instance_id).ok().flatten();
    let outcome = outcome.unwrap_or_else(|| Outcome::done(output));
    record(&state, &id, command_type, &key, outcome, view.map(|v| ("instance", v)))
}

async fn refresh_impl(State(state): State<Arc<AppState>>, body: Option<Json<Value>>) -> Response {
    let body = body_of(body);
    let id = command_id(&body);
    if let Some(prior) = prior(&state, &id) {
        return prior;
    }
    let adapter = text(&body, "adapter_id").to_owned();
    let transport = body
        .get("transport_kind")
        .or_else(|| body.get("transport"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    let scope = if !adapter.is_empty() { adapter.clone() } else if !transport.is_empty() { transport.clone() } else { "all".into() };
    if !adapter.is_empty() && !matches!(target(&adapter), Some((_, Target::Native(..)))) {
        return record(&state, &id, "runtime.instance.refresh", &scope, Outcome::failed(error_value(
            "runtime.instance.invalid", format!("未知 Runtime adapter：{adapter}"), "validation", false)), None);
    }
    if !["", "cli", "structured"].contains(&transport.as_str()) {
        return record(&state, &id, "runtime.instance.refresh", &scope, Outcome::failed(error_value(
            "runtime.instance.invalid", format!("未知 Runtime transport：{transport}"), "validation", false)), None);
    }
    let summary = match probe_all(&state, &adapter, &transport, true).await {
        Ok(summary) => summary,
        Err(reply) => return reply.into_response(),
    };
    let total = summary["total"].as_u64().unwrap_or(0);
    let failed = summary["failed"].as_u64().unwrap_or(0);
    let mut outcome = Outcome::done(summary);
    if total > 0 && failed == total {
        outcome.error = Some(error_value("runtime.instance.refresh_failed", "全部实例 probe 失败", "internal", true));
    }
    record(&state, &id, "runtime.instance.refresh", &scope, outcome, None)
}

async fn auth_command(state: Arc<AppState>, key: String, body: Value, action: &str) -> Response {
    let command_type = format!("runtime.instance.{action}");
    let id = command_id(&body);
    if let Some(prior) = prior(&state, &id) {
        return prior;
    }
    let (adapter_id, instance_id, resolved) = match addressed(&state, &key) {
        Ok(found) => found,
        Err(outcome) => return record(&state, &id, &command_type, &key, outcome, None),
    };
    let key = key_of(&adapter_id, &instance_id);
    if action == "login" && !text(&body, "credential_ref").is_empty() {
        return record(&state, &id, &command_type, &key, Outcome::failed(error_value("runtime.instance.credential_moved",
            "Runtime 不再保存凭据；请在统一凭据中心登记账号，并让 Worker 或 Thread 保存 credential_id", "validation", false)), None);
    }
    let Some(Target::Native(engine, _)) = resolved else {
        return record(&state, &id, &command_type, &key,
            Outcome::done(json!({"instance":key,"auth":not_applicable(),"credential_detached":action == "logout"})), None);
    };
    if action == "auth.refresh" {
        let state_dir = crate::host_process::configured_state_dir(&state);
        let _ = crate::host_process::refresh_shell_env(
            state_dir.as_deref(),
            Duration::from_secs(15),
        )
        .await;
        host_login::invalidate();
    }
    let auth = auth_view(engine, true).await;
    if let Ok(Some(mut health)) = state.store.get(HEALTH, &key) {
        health["auth"] = auth.clone();
        if let Err(error) = state.store.put(HEALTH, &key, &health) {
            return storage(error).into_response();
        }
    }
    let output = json!({"instance":key,"auth":auth,"credential_detached":action == "logout"});
    if action == "login" && auth["status"] != "ok" {
        let mut error = error_value("runtime.instance.login_interaction_required",
            text(&auth, "detail").to_owned(), "state", false);
        error["recovery_hint"] = json!(match text(&auth, "login_command") { "" => "complete Runtime login", c => c });
        error["detail"] = json!({"auth":auth});
        return record(&state, &id, &command_type, &key, Outcome { output, error: Some(error), failed: true }, None);
    }
    record(&state, &id, &command_type, &key, Outcome::done(output), None)
}

async fn login(State(state): State<Arc<AppState>>, Path(key): Path<String>, body: Option<Json<Value>>) -> Response {
    managed_runtime(state,"runtime.instance.login",&key,body_of(body)).await
}

async fn logout(State(state): State<Arc<AppState>>, Path(key): Path<String>) -> Response {
    managed_runtime(state,"runtime.instance.logout",&key,json!({})).await
}

async fn auth_refresh(State(state): State<Arc<AppState>>, Path(key): Path<String>) -> Response {
    managed_runtime(state,"runtime.instance.auth.refresh",&key,json!({})).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Store;
    use axum::body::Body;
    use tower::ServiceExt;

    fn app() -> (Router, Arc<AppState>) {
        let state = Arc::new(AppState::new(Store::open(":memory:").unwrap()));
        (router().with_state(state.clone()), state)
    }

    async fn call(app: &Router, method: &str, uri: &str, body: Option<Value>) -> (StatusCode, Value) {
        let mut request = axum::http::Request::builder().method(method).uri(uri);
        let body = match body {
            Some(value) => {
                request = request.header("content-type", "application/json");
                Body::from(value.to_string())
            }
            None => Body::empty(),
        };
        let response = app.clone().oneshot(request.body(body).unwrap()).await.unwrap();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
        (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
    }

    #[tokio::test]
    async fn worker_profiles_persist_conflict_and_replay() {
        let (app, state) = app();
        let first = json!({"name":"pi-one","engine":"pi","credential_kind":"system_inherit",
            "expected_revision":"","command_id":"profile-create","idempotency_key":"profile-key"});
        let (status, created) = call(&app,"POST","/api/agent-runtimes/profiles",Some(first.clone())).await;
        assert_eq!(status,StatusCode::OK);
        assert_eq!(created["receipt"]["aggregate"]["type"],"profile");
        assert_eq!(created["receipt"]["output"]["profile"]["credential_id"],"system:pi");
        let revision = created["receipt"]["output"]["profile"]["revision"].as_str().unwrap();
        assert_eq!(crate::settings_metadata::configuration(&state).unwrap()["worker_profiles"][0]["id"],"pi-one");
        let mut repeated = first.clone();
        repeated["command_id"] = json!("another-command");
        let (status, replay) = call(&app,"POST","/api/agent-runtimes/profiles",Some(repeated)).await;
        assert_eq!(status,StatusCode::OK);
        assert_eq!(replay["receipt"]["deduplicated"],true);
        let (status, list) = call(&app,"GET","/api/agent-runtimes/profiles",None).await;
        assert_eq!(status,StatusCode::OK);
        assert_eq!(list["count"],1);
        assert_eq!(list["profiles"][0]["runtime_resolution"]["requested"],"cli.pi:default");
        let stale = json!({"name":"pi-one","engine":"pi","expected_revision":"stale","command_id":"stale-profile"});
        let (status, conflict) = call(&app,"POST","/api/agent-runtimes/profiles",Some(stale)).await;
        assert_eq!(status,StatusCode::CONFLICT);
        assert_eq!(conflict["receipt"]["error"]["code"],"worker_profile.revision_conflict");
        let (status, _) = call(&app,"POST","/api/agent-runtimes/profiles",Some(json!({"name":"pi-two","engine":"pi"}))).await;
        assert_eq!(status,StatusCode::OK);
        let (status, removed) = call(&app,"DELETE",&format!("/api/agent-runtimes/profiles/pi-one?expected_revision={revision}"),None).await;
        assert_eq!(status,StatusCode::OK);
        assert_eq!(removed["receipt"]["aggregate"]["type"],"profile");
        assert_eq!(crate::settings_metadata::configuration(&state).unwrap()["worker_profiles"].as_array().unwrap().len(),1);
        let (status, last) = call(&app,"DELETE","/api/agent-runtimes/profiles/pi-two",None).await;
        assert_eq!(status,StatusCode::BAD_REQUEST);
        assert_eq!(last["receipt"]["error"]["code"],"worker_profile.invalid");
    }

    #[tokio::test]
    async fn discovered_rows_cover_every_engine_and_stay_disabled_until_probed() {
        let (app, _) = app();
        let (status, body) = call(&app, "GET", "/api/agent-runtimes?transport=structured", None).await;
        assert_eq!(status, StatusCode::OK);
        let rows = body["instances"].as_array().unwrap();
        let codex = rows.iter().find(|r| r["key"] == "codex.app_server:default").unwrap();
        assert_eq!(codex["discovered"], true);
        assert_eq!(codex["enabled"], false);
        assert_eq!(codex["default_for_engine"], true);
        assert_eq!(codex["auth"]["status"], "not_applicable");
        assert!(rows.iter().all(|r| r["transport_kind"] == "structured"));
        assert!(rows.iter().any(|r| r["key"] == "droid.rpc:default" && r["default_for_engine"] == true));
        let cursor = rows.iter().find(|r| r["key"] == "cursor.sdk:default").unwrap();
        assert_eq!(cursor["access_modes"], json!(["full-access"]));
        let droid = rows.iter().find(|r| r["key"] == "droid.rpc:default").unwrap();
        assert_eq!(droid["access_modes"], json!(["supervised", "auto-accept-edits", "auto", "full-access"]));
        for adapter in ["droid.rpc", "kimi.local_server", "omp.acp"] {
            let row = rows.iter().find(|r| r["key"] == format!("{adapter}:default")).unwrap();
            for mode in row["access_modes"].as_array().unwrap() {
                crate::agent_runtime::pool::admit(adapter, mode.as_str().unwrap()).unwrap();
            }
        }
        assert!(crate::agent_runtime::pool::admit("omp.rpc_v2", "supervised").is_err());
        assert_eq!(declared_capabilities(descriptors::find("cursor").unwrap(), descriptors::find("cursor").unwrap().adapter("cursor.sdk").unwrap())["access_modes"], json!(["full-access"]));
        let (_, cli) = call(&app, "GET", "/api/agent-runtimes?transport=cli", None).await;
        assert!(cli["instances"].as_array().unwrap().iter().all(|r| r["transport_kind"] == "cli"));
        let (status, _) = call(&app, "GET", "/api/agent-runtimes?transport=bogus", None).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn upsert_validates_revisions_references_and_unwired_settings() {
        let (app, state) = app();
        let (status, body) = call(&app, "POST", "/api/agent-runtimes", Some(json!({
            "adapter_id":"codex","instance_id":"default","expected_revision":"",
            "binary_path":"/opt/codex","env_refs":{"OPENAI_BASE_URL":"env:HOST_URL"},"launch_args":["-c","x=1"]}))).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let receipt = &body["receipt"];
        assert_eq!(receipt["state"], "completed");
        let revision = receipt["output"]["instance"]["updated_at"].as_str().unwrap().to_owned();
        assert_eq!(receipt["output"]["instance"]["key"], "codex.app_server:default");
        let stored = state.store.command_receipt(receipt["command_id"].as_str().unwrap()).unwrap().unwrap();
        assert_eq!(stored["state"], "completed");
        let (status, body) = call(&app, "POST", "/api/agent-runtimes", Some(json!({
            "adapter_id":"codex.app_server","expected_revision":"stale","enabled":false}))).await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(body["receipt"]["error"]["code"], "runtime.instance.revision_conflict");
        for (payload, needle) in [
            (json!({"adapter_id":"codex","env_refs":{"A":"inline"}}), "env:"),
            (json!({"adapter_id":"codex","transport":{"experimental_api":"yes"}}), "experimental_api"),
            (json!({"adapter_id":"cli.codex","launch_args":["x"]}), "launch_args"),
            (json!({"adapter_id":"nope"}), "nope"),
            (json!({"adapter_id":"codex","instance_id":"a:b"}), "instance_id"),
        ] {
            let (status, body) = call(&app, "POST", "/api/agent-runtimes", Some(payload)).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
            assert!(body["receipt"]["error"]["message"].as_str().unwrap().contains(needle), "{body}");
        }
        let (status, _) = call(&app,"POST","/api/agent-runtimes",Some(json!({
            "adapter_id":"pi.rpc","instance_id":"secret-ref",
            "env_refs":{"MUTEKI_TEST_REF":"secret://platform/demo/token"}}))).await;
        assert_eq!(status,StatusCode::OK);
        let (status, configured) = call(&app,"POST","/api/agent-runtimes",Some(json!({
            "adapter_id":"pi.rpc","instance_id":"custom-sessions",
            "transport":{"session_dir":"/tmp/muteki-pi-sessions"}}))).await;
        assert_eq!(status,StatusCode::OK);
        assert_eq!(configured["receipt"]["output"]["instance"]["transport"]["session_dir"],"/tmp/muteki-pi-sessions");
        assert_eq!(transport_args("pi.rpc",&configured["receipt"]["output"]["instance"]["transport"]),
            vec!["--session-dir","/tmp/muteki-pi-sessions"]);
        let (_, view) = call(&app, "GET", "/api/agent-runtimes/codex.app_server:default", None).await;
        assert_eq!(view["configured"], true);
        assert_eq!(view["updated_at"], revision);
        assert_eq!(view["binary_path"], "/opt/codex");
        let (status, body) = call(&app, "DELETE", "/api/agent-runtimes/codex.app_server:default", None).await;
        assert_eq!((status, body["receipt"]["state"].clone()), (StatusCode::OK, json!("completed")));
        let (status, _) = call(&app, "DELETE", "/api/agent-runtimes/codex.app_server:default", None).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn probing_records_the_real_failure_of_a_missing_binary() {
        let (app, _) = app();
        call(&app, "POST", "/api/agent-runtimes", Some(json!({"adapter_id":"pi.rpc","binary_path":"/nonexistent/muteki-pi"}))).await;
        let (status, body) = call(&app, "POST", "/api/agent-runtimes/pi.rpc:default/probe", None).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["instance"]["health"]["healthy"], false);
        assert!(body["instance"]["health"]["detail"].as_str().unwrap().contains("实例配置的 binary 不可运行"));
        call(&app, "POST", "/api/agent-runtimes", Some(json!({"adapter_id":"omp.rpc_v2","binary_path":"/nonexistent/muteki-omp"}))).await;
        let (status, body) = call(&app, "POST", "/api/agent-runtimes/omp.rpc_v2:default/probe", None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["instance"]["health"]["error_code"], "runtime.binary_unrunnable");
        let (status, body) = call(&app, "POST", "/api/agent-runtimes/pi.rpc:default/probe",
            Some(json!({"credential_id":"system:pi"}))).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["receipt"]["error"]["code"], "runtime.instance.probe_scope_invalid");
        let (status, body) = call(&app, "POST", "/api/agent-runtimes/nope:default/probe", None).await;
        assert_eq!((status, body["receipt"]["error"]["code"].clone()), (StatusCode::BAD_REQUEST, json!("runtime.instance.invalid")));
    }

    #[tokio::test]
    async fn descriptors_publish_one_entry_per_engine_and_refuse_unknown_ones() {
        let (app, _) = app();
        let (status, body) = call(&app, "GET", "/api/agent-runtimes/descriptors", None).await;
        assert_eq!(status, StatusCode::OK);
        let engines: Vec<&str> = body["descriptors"].as_array().unwrap().iter().map(|d| d["identity"]["engine"].as_str().unwrap()).collect();
        let mut unique = engines.clone();
        unique.dedup();
        assert_eq!(engines, unique);
        assert!(engines.contains(&"codex"));
        assert!(body["instances"].as_array().unwrap().iter().any(|r| r["key"] == "claude.sdk:default"));
        let (status, body) = call(&app, "GET", "/api/agent-runtimes/descriptors?engine=nope", None).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(body["error"]["code"], "provider.descriptor_unknown");
    }

    #[tokio::test]
    async fn a_repeated_command_id_returns_the_first_receipt() {
        let (app, _) = app();
        let payload = json!({"command_id":"cmd-fixed","adapter_id":"claude","label":"A"});
        let (_, first) = call(&app, "POST", "/api/agent-runtimes", Some(payload.clone())).await;
        let (status, conflict) = call(&app, "POST", "/api/agent-runtimes", Some(json!({"command_id":"cmd-fixed","adapter_id":"claude","label":"B"}))).await;
        assert_eq!(status, StatusCode::CONFLICT, "{conflict}");
        let (_, second) = call(&app, "POST", "/api/agent-runtimes", Some(payload)).await;
        assert_eq!(second["receipt"]["deduplicated"], true);
        assert_eq!(second["receipt"]["output"], first["receipt"]["output"]);
    }
}
