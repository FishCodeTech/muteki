//! Native engine credentials in the settings API.
//!
//! The host's own CLI login of every engine is listed as `system:<engine>`
//! next to the stored accounts. Model tests and catalog refreshes for a
//! native runtime go through the engine itself: a test is one real model turn.
//! Reference-only engines expose the source project's labeled reference list.

use crate::{
    AppState,
    agent_runtime::{access, binaries, credentials, descriptors, host_login, pool},
    runtime_api,
};
use axum::{
    Json, Router,
    extract::{Query, State},
    http::StatusCode,
    routing::get,
};
use serde_json::{Value, json};
use std::{collections::BTreeMap, collections::HashMap, sync::Arc, time::Instant};

type ApiResult = Result<Json<Value>, (StatusCode, Json<Value>)>;

const HEALTH_CONCURRENCY: usize = 3;

pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/settings/system-login", get(system_login))
        .route("/api/engines", get(engines))
        .route("/api/engines/health", get(engines_health))
}

fn text<'a>(value: &'a Value, key: &str) -> &'a str {
    value[key].as_str().map(str::trim).unwrap_or("")
}

fn seconds() -> f64 {
    chrono::Utc::now().timestamp_millis() as f64 / 1000.0
}

fn storage(error: impl std::fmt::Display) -> (StatusCode, Json<Value>) {
    tracing::error!(%error, "Native credential storage failed");
    (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"detail":"Could not save model settings"})))
}

fn supported(engine: &descriptors::EngineDescriptor) -> bool {
    engine.identity.support_status == "supported"
}

fn listed_engines() -> Vec<&'static descriptors::EngineDescriptor> {
    descriptors::all().iter().filter(|engine| supported(engine)).collect()
}

pub fn system_ids() -> Vec<String> {
    listed_engines().iter().map(|engine| format!("system:{}", engine.engine())).collect()
}

fn model_ids(catalog: &Value, field: &str) -> Vec<String> {
    catalog[field]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|item| match item {
            Value::String(id) if !id.trim().is_empty() => Some(id.trim().to_owned()),
            Value::Object(_) if !text(item, "id").is_empty() => Some(text(item, "id").to_owned()),
            _ => None,
        })
        .collect()
}

fn push_unique_id(rows: &mut Vec<Value>, id: &str) {
    let trimmed = id.trim();
    if !trimmed.is_empty()
        && !rows.iter().any(|row| {
            row.as_str() == Some(trimmed) || row["id"].as_str() == Some(trimmed)
        })
    {
        rows.push(json!(trimmed));
    }
}

fn tests_of(store: &crate::store::Store, id: &str, environment: &str) -> (Value, Value) {
    let mut by_runtime = serde_json::Map::new();
    let mut latest = None::<Value>;
    for test in store.list("model_test_results").unwrap_or_default() {
        if test["credential_id"] != id || test["backend"].as_str().unwrap_or("local") != environment {
            continue;
        }
        let runtime = text(&test, "runtime_instance").to_owned();
        by_runtime.insert(runtime.clone(), test.clone());
        let (adapter_id, instance_id) = split_key(&runtime);
        if let Some((engine, _)) = descriptors::for_adapter(adapter_id) {
            for sibling in [&engine.identity.default_adapter_id, &engine.identity.cli_adapter_id] {
                if !sibling.is_empty() {
                    let sibling_key = format!("{sibling}:{instance_id}");
                    by_runtime.entry(sibling_key).or_insert_with(|| test.clone());
                }
            }
        }
        if latest.as_ref().is_none_or(|v| v["tested_at"].as_f64() < test["tested_at"].as_f64()) {
            latest = Some(test);
        }
    }
    (Value::Object(by_runtime), latest.unwrap_or(Value::Null))
}

/// Fold an engine's native catalogs into a credential row: candidates from
/// what the engine listed, verified models from real tests.
fn merge_native(store: &crate::store::Store, row: &mut Value, primary: Option<&str>, environment: &str) -> Value {
    let id = text(row, "id").to_owned();
    let catalogs = runtime_api::native_catalogs_for_environment(store, &id, environment);
    let mut public = row["model_catalogs"].as_object().cloned().unwrap_or_default();
    let mut candidates = row["candidate_models"].as_array().cloned().unwrap_or_default();
    let mut verified = row["models"].as_array().cloned().unwrap_or_default();
    for (key, catalog) in &catalogs {
        if row["connection"] == "custom_endpoint" && public.contains_key(key) { continue; }
        public.insert(key.clone(), catalog.clone());
    }
    let primary_catalog = primary
        .and_then(|key| public.get(key).cloned())
        .or_else(|| public.values().next().cloned())
        .unwrap_or(json!({}));
    for model_id in model_ids(&primary_catalog, "discovered_models") {
        push_unique_id(&mut candidates, &model_id);
    }
    for model_id in model_ids(&primary_catalog, "verified_models") {
        push_unique_id(&mut verified, &model_id);
    }
    if row["last_test"]["ok"] == true {
        let tested_model = text(&row["last_test"], "model");
        if !tested_model.is_empty() {
            push_unique_id(&mut verified, tested_model);
        }
    }
    row["model_catalogs"] = Value::Object(public);
    row["candidate_models"] = json!(candidates);
    row["models"] = json!(verified);
    primary_catalog
}

/// Stored account rows of engine-owned accounts gain the engine's catalogs.
pub fn decorate_account(store: &crate::store::Store, row: &mut Value, environment: &str) {
    let engine = text(row, "worker_engine").to_owned();
    let engine = if engine.is_empty() { text(row, "engine").to_owned() } else { engine };
    if descriptors::find(&engine).is_none() {
        return;
    }
    let primary = runtime_api::default_runtime_key(&engine);
    let catalog = merge_native(store, row, primary.as_deref(), environment);
    if row.get("model_catalog").is_none_or(Value::is_null) && catalog.as_object().is_some_and(|c| !c.is_empty()) {
        row["model_catalog"] = catalog;
    }
}

/// A stored account holding a native login (not an HTTP endpoint) is
/// configured for its engine; whether it works is what its tests say.
pub fn native_account_status(store: &crate::store::Store, state_dir: &std::path::Path, account: &Value, row: &mut Value, environment: &str) {
    if row["http_compatible"] == true {
        return;
    }
    let Some(id) = account["account_id"].as_str() else { return };
    let (present, _) = crate::quota::stored_material(state_dir, id, account);
    if !present {
        return;
    }
    let (last_tests, last_test) = tests_of(store, &format!("account:{id}"), environment);
    row["present"] = json!(true);
    row["status"] = json!(match last_test["ok"].as_bool() {
        Some(true) => "ready",
        Some(false) => "failed",
        None => "untested",
    });
    row["status_detail"] = Value::Null;
    row["last_tests"] = last_tests;
    row["last_test"] = last_test;
}

/// One `system:<engine>` row per listed engine, detected concurrently.
pub async fn system_rows(
    store: &crate::store::Store,
    usage: &BTreeMap<String, Vec<Value>>,
    fresh: bool,
) -> Vec<Value> {
    let engines = listed_engines();
    let detections = futures_util::future::join_all(
        engines.iter().map(|engine| host_login::detect(engine.engine(), fresh)),
    )
    .await;
    engines
        .into_iter()
        .zip(detections)
        .map(|(engine, detected)| {
            let name = engine.engine();
            let id = format!("system:{name}");
            let present = match detected {
                host_login::PRESENT => json!(true),
                host_login::ABSENT => json!(false),
                _ => Value::Null,
            };
            let (last_tests, last_test) = tests_of(store, &id, "local");
            let mut status = match detected {
                host_login::PRESENT => "untested",
                host_login::ABSENT => "missing",
                _ => "unknown",
            };
            if !last_test.is_null() {
                status = if last_test["ok"] == true { "ready" } else { "failed" };
            }
            if detected == host_login::DISABLED {
                status = "unavailable";
            }
            let mut row = json!({
                "id":id,"credential_id":id,"label":format!("{name} 系统登录"),"engine":name,"source":"system",
                "connection":"official","credential_format":"host_login","detection_status":detected,
                "provider":"","base_url":"","present":present,"status":status,
                "models":[],"candidate_models":[],"model_catalogs":{},
                "usage":usage.get(&id).cloned().unwrap_or_default(),
                "last_tests":last_tests,"last_test":last_test,"http_compatible":false,
            });
            let primary = runtime_api::default_runtime_key(name);
            let catalog = merge_native(store, &mut row, primary.as_deref(), "local");
            row["default_model"] = catalog.get("default_model").cloned().filter(|v| !v.is_null()).unwrap_or(json!(""));
            row["model_catalog"] = catalog;
            if detected == host_login::DISABLED {
                row["status_detail"] = json!("此服务未提供宿主登录发现");
                row["discovery_code"] = json!("host_discovery_disabled");
            }
            row
        })
        .collect()
}

/// The native runtime a credential test or refresh targets, or `None` when the
/// request belongs to the HTTP text path.
pub fn native_target(store: &crate::store::Store, id: &str, body: &Value) -> Option<String> {
    let requested = text(body, "runtime_instance");
    let native = (!requested.is_empty() && runtime_api::is_native_runtime(requested)).then(|| requested.to_owned());
    if let Some(engine) = id.strip_prefix("system:") {
        descriptors::find(engine)?;
        return native.or_else(|| runtime_api::default_runtime_key(engine));
    }
    let account_id = id.strip_prefix("account:").unwrap_or(id);
    let account = credentials::account(store, account_id)?;
    let engine = [text(&account, "worker_engine"), text(&account, "engine")]
        .into_iter()
        .find(|v| !v.is_empty())
        .unwrap_or("");
    descriptors::find(engine)?;
    native
}

fn canonical_id(id: &str) -> String {
    if id.starts_with("system:") || id.starts_with("account:") {
        id.to_owned()
    } else {
        format!("account:{id}")
    }
}

fn split_key(key: &str) -> (&str, &str) {
    key.rsplit_once(':').unwrap_or((key, "default"))
}

/// The access mode a probe runs under: supervised where offered, so any
/// action the model attempts is asked about and the probe cancels it.
fn probe_access(adapter_id: &str) -> String {
    let Ok((_, adapter, native)) = pool::transport(adapter_id) else {
        return String::new();
    };
    let offered = access::offered(pool::provider_of(native), adapter.capabilities.approval, &adapter.capabilities.access_modes);
    if offered.contains(&access::SUPERVISED) {
        access::SUPERVISED.to_owned()
    } else {
        offered.first().map(|m| (*m).to_owned()).unwrap_or_default()
    }
}

pub(crate) fn probe_runtime(credential_id: &str, runtime_key: &str, model: &str, effort: &str) -> Value {
    let (adapter, instance) = split_key(runtime_key);
    let adapter = descriptors::for_adapter(adapter)
        .filter(|(_, a)| a.native.is_none() && a.role == "cli")
        .map(|(e, _)| e.identity.default_adapter_id.as_str())
        .unwrap_or(adapter);
    let effort = (!effort.is_empty() && effort != "default").then_some(effort);
    json!({"adapter_id":adapter,"instance_id":instance,"model":(!model.is_empty()).then_some(model),
        "effort":effort,"credential_id":credential_id,"access_mode":probe_access(adapter)})
}

/// A real model turn for one credential under one native runtime; the
/// result is stored like an HTTP test and marks the model verified.
pub async fn test(state: Arc<AppState>, id: &str, runtime_key: String, body: Value) -> ApiResult {
    let id = canonical_id(id);
    if !body.is_object() && !body.is_null() {
        return Err((StatusCode::UNPROCESSABLE_ENTITY, Json(json!({"detail":"Expected an object"}))));
    }
    let model = text(&body, "model").to_owned();
    let engine = descriptors::for_adapter(split_key(&runtime_key).0).map(|(e, _)| e.engine().to_owned()).unwrap_or_default();
    let account_snapshot = if let Some(account_id) = id.strip_prefix("account:") {
        Some((account_id.to_owned(), state.store.get("credential_accounts", account_id).map_err(storage)?))
    } else { None };
    let started = Instant::now();
    let backend = if text(&body, "backend") == "container" { "container" } else { "local" };
    let outcome: Result<Value, (String, String, &str)> = if model.is_empty() {
        Err(("请选择一个候选模型后执行真实测试".into(), "model.required".into(), "model"))
    } else if backend == "container" && id.starts_with("system:") {
        Err(("系统登录仅适用于本地运行环境".into(), "credential.container_unavailable".into(), "binding"))
    } else if backend == "container" {
        let settings = crate::settings_metadata::configuration(&state).map_err(|_| (
            "无法读取容器 Worker 配置".to_owned(), "runtime.configuration_failed".to_owned(), "binding"
        ));
        match settings {
            Ok(settings) => crate::runs::container_exec::run_container_model_probe(
                &state, &engine, &id, Some(&model),
                (!text(&body, "reasoning_effort").is_empty()).then(|| text(&body, "reasoning_effort")),
                &settings,
            ).await.map_err(|e| (e.message, e.code.to_owned(), "model")),
            Err(error) => Err(error),
        }
    } else {
        crate::native_chat::probe_model(&state, &probe_runtime(&id, &runtime_key, &model, text(&body, "reasoning_effort")))
            .await
            .map_err(|e| (e.message, e.code.to_owned(), "model"))
    };
    let ok = outcome.is_ok();
    let (detail, code, layer, served) = match &outcome {
        Ok(reply) => (format!("{} 返回：{}", text(reply, "model"), text(reply, "reply")), String::new(), "model", reply["model"].clone()),
        Err((detail, code, layer)) => (detail.clone(), code.clone(), *layer, Value::Null),
    };
    if let Some((account_id, original)) = account_snapshot
        && state.store.get("credential_accounts", &account_id).map_err(storage)? != original {
        return Err((StatusCode::CONFLICT, Json(json!({"detail":"Credential configuration changed during the test"}))));
    }
    let last = json!({"ok":ok,"status":if ok {"ok"} else {"failed"},"detail":detail,"model":model,
        "served_model":served,"engine":engine,"backend":backend,"runtime_instance":runtime_key,
        "tested_at":seconds(),"elapsed_ms":started.elapsed().as_millis() as u64,"layer":layer,
        "credential_id":id,"error_code":code});
    let test_key = if backend == "container" { format!("{id}|container|{runtime_key}") } else { format!("{id}|{runtime_key}") };
    state.store.put("model_test_results", &test_key, &last).map_err(storage)?;
    if !model.is_empty() {
        let key = if backend == "container" { format!("native:container:{id}|{runtime_key}") } else { format!("native:{id}|{runtime_key}") };
        let mut catalog = state.store.get("model_catalogs", &key).map_err(storage)?.unwrap_or_else(|| {
            json!({"credential_id":id,"engine":engine,"environment":backend,"runtime_instance":runtime_key,
                "configured_models":[],"discovered_models":[],"verified_models":[],
                "source":runtime_api::NATIVE_CATALOG_SOURCE,"refresh_status":"missing"})
        });
        let mut verified = catalog["verified_models"].as_array().cloned().unwrap_or_default();
        verified.retain(|v| v.as_str() != Some(model.as_str()));
        if ok {
            verified.push(json!(model));
        }
        catalog["verified_models"] = json!(verified);
        state.store.put("model_catalogs", &key, &catalog).map_err(storage)?;
    }
    let mut response = last.clone();
    response["last_test"] = last;
    Ok(Json(response))
}

pub async fn refresh(state: Arc<AppState>, id: &str, runtime_key: String) -> ApiResult {
    runtime_api::refresh_native_catalog(&state, &canonical_id(id), &runtime_key).await.map(Json)
}

pub async fn refresh_container(state: Arc<AppState>, id: &str, runtime_key: String) -> ApiResult {
    let id = canonical_id(id);
    let engine = descriptors::for_adapter(split_key(&runtime_key).0)
        .map(|(engine, _)| engine.engine()).unwrap_or("");
    if id.starts_with("system:") {
        return Err((StatusCode::UNPROCESSABLE_ENTITY, Json(json!({"detail":"系统登录仅适用于本地运行环境"}))));
    }
    if !matches!(engine, "pi" | "kimi" | "omp" | "droid" | "grok" | "opencode" | "cursor" | "codex" | "claude") {
        return Err((StatusCode::NOT_IMPLEMENTED, Json(json!({"detail":format!("{engine} 容器模型目录尚未接入")}))));
    }
    let account_id = id.strip_prefix("account:").unwrap_or(&id);
    let account = state.store.get("credential_accounts", account_id).map_err(storage)?
        .ok_or_else(|| (StatusCode::NOT_FOUND, Json(json!({"detail":"Credential account not found"}))))?;
    let key = format!("native:container:{id}|{runtime_key}");
    let old = state.store.get("model_catalogs", &key).map_err(storage)?;
    let runtime_snapshot = state.store.get(crate::runtime_api::INSTANCES,&runtime_key).map_err(storage)?;
    let result = if engine == "claude" {
        let reference: Value = serde_json::from_str(include_str!("runs/worker_models.manual.json"))
            .map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"detail":"Could not read reference catalog"}))))?;
        Ok(reference["models"][engine].as_array().cloned().unwrap_or_default())
    } else if engine == "codex" {
        crate::runs::container_exec::run_container_codex_bundled_catalog().await
    } else {
        let settings = crate::settings_metadata::configuration(&state)
            .map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"detail":"Could not read worker settings"}))))?;
        if engine == "pi" {
            crate::runs::container_exec::run_container_pi_catalog(&state, &id, &settings, None).await
        } else {
            crate::runs::container_exec::run_container_cli_catalog(&state, engine, &id, &settings).await
        }
    };
    if state.store.get("credential_accounts", account_id).map_err(storage)?.as_ref() != Some(&account) {
        return Err((StatusCode::CONFLICT, Json(json!({"detail":"Credential configuration changed during catalog refresh"}))));
    }
    let ok = result.is_ok();
    let discovery_source = match engine {
        "claude" => "claude_reference_catalog",
        "codex" => "codex_cli_bundled",
        _ => runtime_api::NATIVE_CATALOG_SOURCE,
    };
    let (models, detail, code) = match result {
        Ok(models) => {
            let detail = match engine {
                "claude" => format!("Claude 参考目录共 {} 个模型；真实可用性以模型连通测试为准", models.len()),
                "codex" => format!("从离线 Codex CLI 内置目录读到 {} 个模型；真实可用性以模型连通测试为准", models.len()),
                _ => format!("从 {engine} 容器读到 {} 个模型", models.len()),
            };
            (models, detail, String::new())
        }
        Err(error) => (old.as_ref().and_then(|v| v["discovered_models"].as_array())
            .cloned().unwrap_or_default(), error.message, error.code.to_owned()),
    };
    let now = seconds();
    let default_model = account["default_model"].as_str().filter(|v| !v.is_empty())
        .map(|v| json!(v)).or_else(|| models.first().map(|v| v["id"].clone())).unwrap_or(Value::Null);
    let catalog = json!({"credential_id":id,"engine":engine,"environment":"container",
        "runtime_instance":runtime_key,"configured_models":account["models"],
        "discovered_models":models,"verified_models":old.as_ref().map(|v| v["verified_models"].clone()).filter(Value::is_array).unwrap_or(json!([])),
        "default_model":default_model,"source":runtime_api::NATIVE_CATALOG_SOURCE,
        "discovery_source":discovery_source,
        "refresh_status":if ok { "fresh" } else if models.is_empty() { "failed" } else { "stale" },
        "refreshed_at":if ok {json!(now)} else {old.as_ref().map(|v| v["refreshed_at"].clone()).unwrap_or(Value::Null)},
        "expires_at":if ok {json!(now + 3600.0)} else {old.as_ref().map(|v| v["expires_at"].clone()).unwrap_or(Value::Null)},
        "last_error":if ok {""} else {detail.as_str()},"error_code":code});
    let mut catalog = catalog;
    catalog["configuration_revision"] = account["revision"].clone();
    if !state.store.put_many_if(&[("credential_accounts",account_id,Some(&account)),
        (crate::runtime_api::INSTANCES,runtime_key.as_str(),runtime_snapshot.as_ref()),("model_catalogs",key.as_str(),old.as_ref())],
        &[("model_catalogs",key.as_str(),&catalog)]).map_err(storage)? {
        return Err((StatusCode::CONFLICT,Json(json!({"detail":"Credential, Runtime or model evidence changed during catalog refresh"}))));
    }

    let ids: Vec<Value> = models.iter().map(|v| v["id"].clone()).collect();
    Ok(Json(json!({"ok":ok,"detail":detail,"source":discovery_source,
        "models":ids,"credential_id":id,"catalog":catalog,"error_code":code})))
}

/// Native catalogs due for a refresh: stored ones past their expiry, and the
/// default runtime of every engine whose host login is present but has none.
pub async fn stale_targets(store: &crate::store::Store) -> Vec<(String, String)> {
    let now = seconds();
    let catalogs: Vec<Value> = store
        .list("model_catalogs")
        .unwrap_or_default()
        .into_iter()
        .filter(|row| row["source"] == runtime_api::NATIVE_CATALOG_SOURCE)
        .collect();
    let mut targets: Vec<(String, String)> = catalogs
        .iter()
        .filter(|row| {
            row["refresh_status"] != "fresh"
                || (row["engine"] == "grok" && row["metadata_version"].as_u64() != Some(runtime_api::NATIVE_CATALOG_METADATA_VERSION))
                || row["expires_at"].as_f64().unwrap_or(0.0) <= now
                || row["discovered_models"]
                    .as_array()
                    .is_some_and(|models| models.iter().any(|model| model["reasoning"]["source"] == "acp_session"))
        })
        .map(|row| (text(row, "credential_id").to_owned(), text(row, "runtime_instance").to_owned()))
        .filter(|(id, key)| !id.is_empty() && !key.is_empty())
        .collect();
    for engine in listed_engines() {
        let Some(key) = runtime_api::default_runtime_key(engine.engine()) else {
            continue;
        };
        let id = format!("system:{}", engine.engine());
        let known = catalogs.iter().any(|row| row["credential_id"] == id.as_str() && row["runtime_instance"] == key.as_str());
        if !known && host_login::detect(engine.engine(), false).await == host_login::PRESENT {
            targets.push((id, key));
        }
    }
    targets.sort();
    targets.dedup();
    targets
}

async fn system_login() -> Json<Value> {
    let engines = listed_engines();
    let detections =
        futures_util::future::join_all(engines.iter().map(|engine| host_login::detect(engine.engine(), false))).await;
    let logins: serde_json::Map<String, Value> = engines
        .iter()
        .zip(detections)
        .map(|(engine, status)| (engine.engine().to_owned(), json!(status)))
        .collect();
    Json(json!({"logins":logins}))
}

/// Binary availability per engine. Polled by the browser, so no model turn.
async fn engines() -> Json<Value> {
    let engines = listed_engines();
    let resolved = futures_util::future::join_all(engines.iter().map(|engine| binaries::resolve(engine, None))).await;
    let rows: Vec<Value> = engines
        .iter()
        .zip(resolved)
        .map(|(engine, binary)| {
            let (bin, available, detail) = match binary {
                Ok(binary) => (binary.path.to_string_lossy().into_owned(), true, String::new()),
                Err(error) => (engine.binary.basename.to_owned(), false, error.message),
            };
            json!({"engine":engine.engine(),"bin":bin,"available":available,"healthy":null,
                "health_detail":"","detail":detail})
        })
        .collect();
    Json(json!({"engines":rows}))
}

async fn version(binary: &std::path::Path) -> Result<String, String> {
    let mut cmd = crate::host_process::binary_command(binary).map_err(|error| error.message)?;
    let child = cmd
        .arg("--version")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .output();
    let out = tokio::time::timeout(std::time::Duration::from_secs(20), child)
        .await
        .map_err(|_| "probe timed out".to_owned())?
        .map_err(|e| e.to_string())?;
    if !out.status.success() {
        return Err("binary not runnable (--version failed)".into());
    }
    let raw = if out.stdout.is_empty() { out.stderr } else { out.stdout };
    Ok(String::from_utf8_lossy(&raw).lines().next().unwrap_or("").trim().chars().take(80).collect())
}

pub(crate) fn container_binary(engine: &str) -> &str {
    match engine {
        "cursor" => "/home/kali/.local/bin/cursor-agent",
        "omp" => "/home/kali/.local/bin/omp",
        "grok" => "/home/kali/.grok/bin/grok",
        "claude" => "claude",
        "codex" => "codex",
        "pi" => "pi",
        "kimi" => "kimi",
        "opencode" => "opencode",
        "droid" => "droid",
        _ => engine,
    }
}

async fn container_health() -> Value {
    if let Some(socket)=crate::runner::configured_socket(){
        return crate::runner::request(&socket,"/v1/engines/health").await.unwrap_or_else(|error|json!({"engines":[],"error":error}));
    }
    container_health_local().await
}
pub(crate) async fn container_health_local() -> Value {
    use std::{process::Stdio, time::Duration};
    let image = crate::runs::container_exec::resolve_worker_image();
    let inspect = tokio::time::timeout(
        Duration::from_secs(20),
        crate::host_process::command("docker")
            .args(["image", "inspect", &image])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .output(),
    )
    .await;
    let image_error = match inspect {
        Ok(Ok(output)) if output.status.success() => String::new(),
        Ok(Ok(_)) => format!("image missing: {image}"),
        Ok(Err(error)) if error.kind() == std::io::ErrorKind::NotFound => "docker not found".to_owned(),
        Ok(Err(error)) => error.to_string(),
        Err(_) => "docker image inspect timed out".to_owned(),
    };
    let semaphore = Arc::new(tokio::sync::Semaphore::new(HEALTH_CONCURRENCY));
    let tasks = listed_engines().into_iter().map(|engine| {
        let image = image.clone();
        let image_error = image_error.clone();
        let semaphore = semaphore.clone();
        async move {
            let name = engine.engine();
            let binary = container_binary(name);
            let mut row = json!({"engine":name,"bin":binary,"version":"","healthy":false,
                "detail":"","backend":"container"});
            if !image_error.is_empty() {
                row["detail"] = json!(image_error);
                return row;
            }
            let _permit = semaphore.acquire_owned().await;
            let command = format!("{binary} --version 2>&1 || echo MUTEKI_CLI_FAIL");
            let output = tokio::time::timeout(
                Duration::from_secs(60),
                crate::host_process::command("docker")
                    .args(["run", "--rm", "--network", "none", "--entrypoint", "bash", &image, "-lc", &command])
                    .stdin(Stdio::null())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .kill_on_drop(true)
                    .output(),
            )
            .await;
            match output {
                Ok(Ok(output)) => {
                    let raw = String::from_utf8_lossy(&output.stdout);
                    if output.status.success() && !raw.contains("MUTEKI_CLI_FAIL") {
                        row["healthy"] = json!(true);
                        row["version"] = json!(raw.lines().next().unwrap_or("").trim().chars().take(80).collect::<String>());
                    } else {
                        row["detail"] = json!(format!("{name} CLI not launchable in container"));
                    }
                }
                Ok(Err(error)) => row["detail"] = json!(error.to_string()),
                Err(_) => row["detail"] = json!("container probe timed out"),
            }
            row
        }
    });
    let engines = futures_util::future::join_all(tasks).await;
    json!({"engines":engines})
}

/// Deep self-check: each engine's binary, then one real turn with the host
/// login under its default runtime. On demand only; it spends tokens.
async fn engines_health(
    State(state): State<Arc<AppState>>,
    Query(query): Query<HashMap<String, String>>,
) -> ApiResult {
    if query.get("backend").is_some_and(|b| b == "container") {
        return Ok(Json(container_health().await));
    }
    let semaphore = Arc::new(tokio::sync::Semaphore::new(HEALTH_CONCURRENCY));
    let mut tasks = Vec::new();
    for engine in listed_engines() {
        let (state, semaphore) = (state.clone(), semaphore.clone());
        tasks.push(tokio::spawn(async move {
            let _permit = semaphore.acquire_owned().await;
            let name = engine.engine();
            let mut row = json!({"engine":name,"bin":engine.binary.basename,"version":"","healthy":false,
                "detail":"","backend":"local","bin_source":"fallback","bin_env":engine.binary.env_override});
            let binary = match binaries::resolve(engine, None).await {
                Ok(binary) => binary,
                Err(error) => {
                    row["detail"] = json!(error.message);
                    return row;
                }
            };
            row["bin"] = json!(binary.path.to_string_lossy());
            row["bin_source"] = json!(binary.source);
            match version(&binary.path).await {
                Ok(v) => row["version"] = json!(v),
                Err(detail) => {
                    row["detail"] = json!(detail);
                    return row;
                }
            }
            let Some(key) = runtime_api::default_runtime_key(name) else {
                row["detail"] = json!("engine has no conversation runtime");
                return row;
            };
            let id = format!("system:{name}");
            let catalog = runtime_api::native_catalogs(&state.store, &id).get(&key).cloned().unwrap_or(json!({}));
            let model = text(&catalog, "default_model").to_owned();
            match crate::native_chat::probe_model(&state, &probe_runtime(&id, &key, &model, "")).await {
                Ok(reply) => {
                    row["healthy"] = json!(true);
                    row["detail"] = json!(format!("{} 返回：{}", text(&reply, "model"), text(&reply, "reply")));
                    row["model"] = reply["model"].clone();
                }
                Err(error) => {
                    row["detail"] = json!(error.message);
                    row["error_code"] = json!(error.code);
                }
            }
            row
        }));
    }
    let mut rows = Vec::new();
    for task in tasks {
        match task.await {
            Ok(row) => rows.push(row),
            Err(error) => rows.push(json!({"healthy":false,"detail":format!("probe task failed: {error}")})),
        }
    }
    Ok(Json(json!({"engines":rows})))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Store;

    #[test]
    fn native_targets_route_system_ids_and_native_runtimes_only() {
        let store = Store::open(":memory:").unwrap();
        store.put("credential_accounts", "work", &json!({"account_id":"work","engine":"claude","secret":"s"})).unwrap();
        store.put("credential_accounts", "api", &json!({"account_id":"api","engine":"api","secret":"s"})).unwrap();
        let codex = runtime_api::default_runtime_key("codex").unwrap();
        assert_eq!(native_target(&store, "system:codex", &json!({})), Some(codex.clone()));
        assert_eq!(native_target(&store, "system:openai", &json!({})), None);
        assert_eq!(native_target(&store, "account:work", &json!({"runtime_instance":"claude-http:text"})), None);
        assert_eq!(native_target(&store, "account:work", &json!({"runtime_instance":"claude.sdk:default"})), Some("claude.sdk:default".into()));
        assert_eq!(native_target(&store, "account:api", &json!({"runtime_instance":"claude.sdk:default"})), None);
    }

    #[test]
    fn endpoint_catalog_is_not_overwritten_by_a_legacy_native_catalog() {
        let store = Store::open(":memory:").unwrap();
        let runtime = "pi.rpc:default";
        store.put("model_catalogs", &format!("native:account:pi|{runtime}"), &json!({
            "credential_id":"account:pi","runtime_instance":runtime,"source":runtime_api::NATIVE_CATALOG_SOURCE,
            "discovered_models":[{"id":"muteki/old"}],"default_model":"muteki/old"})).unwrap();
        let endpoint = json!({"discovered_models":[{"id":"a"},{"id":"b"}],"source":"pi_cli_rpc"});
        let mut row = json!({"id":"account:pi","engine":"pi","connection":"custom_endpoint",
            "model_catalogs":{runtime:endpoint},"models":[],"candidate_models":[]});
        decorate_account(&store, &mut row, "local");
        assert_eq!(row["model_catalogs"][runtime], endpoint);
        assert_eq!(row["model_catalog"], endpoint);
        assert_eq!(row["candidate_models"], json!(["a","b"]));
    }

    #[tokio::test]
    async fn system_rows_project_catalogs_and_tests() {
        let store = Store::open(":memory:").unwrap();
        let key = runtime_api::default_runtime_key("codex").unwrap();
        store.put("model_catalogs", &format!("native:system:codex|{key}"), &json!({
            "credential_id":"system:codex","runtime_instance":key,"source":runtime_api::NATIVE_CATALOG_SOURCE,
            "discovered_models":[{"id":"gpt-x","label":"GPT X"},{"id":"gpt-y","label":"GPT Y"}],
            "verified_models":["gpt-y"],"default_model":"gpt-x"})).unwrap();
        store.put("model_test_results", &format!("system:codex|{key}"), &json!({
            "credential_id":"system:codex","runtime_instance":key,"ok":true,"tested_at":1.0,"model":"gpt-y"})).unwrap();
        let rows = system_rows(&store, &BTreeMap::new(), false).await;
        let codex = rows.iter().find(|r| r["id"] == "system:codex").unwrap();
        assert_eq!(codex["source"], "system");
        assert_eq!(codex["credential_format"], "host_login");
        assert_eq!(codex["status"], "ready");
        assert_eq!(codex["default_model"], "gpt-x");
        assert_eq!(codex["models"][0], "gpt-y");
        assert_eq!(codex["model_catalog"]["discovered_models"][1]["label"], "GPT Y");
        assert_eq!(codex["candidate_models"].as_array().unwrap().len(), 2);
        assert!(codex["model_catalogs"][&key].is_object());
        assert!(codex["last_tests"][&key].is_object());
        assert!(rows.iter().all(|r| r["engine"] != "openai"));
    }
}
