//! Worker settings companion endpoints: model catalogs, profile health/preflight,
//! model probe tests, planner/titler LLM probe, worker image status,
//! and identity aliases.

use crate::{
    AppState,
    agent_runtime::{binaries, credentials, descriptors, host_login},
    native_chat, runtime_api, settings_metadata,
};
use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post, put},
};
use serde_json::{Map, Value, json};
use std::{collections::HashMap, sync::Arc, time::Instant};

const MANUAL_MODELS_JSON: &str = include_str!("worker_models.manual.json");

pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/settings/worker-models", get(get_worker_models))
        .route(
            "/api/settings/worker-models/discover",
            post(discover_worker_models),
        )
        .route("/api/settings/profiles/health", get(profiles_health_get))
        .route(
            "/api/settings/profiles/{profile_id}/health",
            post(profile_health_single),
        )
        .route(
            "/api/settings/profiles/preflight-batch",
            post(profiles_preflight_batch),
        )
        .route(
            "/api/settings/worker-model/test-results",
            get(get_worker_model_test_results),
        )
        .route(
            "/api/settings/worker-model/test",
            post(test_worker_model_single),
        )
        .route(
            "/api/settings/worker-model/test-batch",
            post(test_worker_model_batch),
        )
        .route("/api/settings/llm/test", post(test_llm_endpoint))
        .route("/api/settings/worker-image", get(get_worker_image))
        .route("/api/settings/worker-image/pull", post(pull_worker_image))
        .route("/api/settings/identity", put(put_identity))
}

fn now_secs() -> f64 {
    chrono::Utc::now().timestamp_millis() as f64 / 1000.0
}

pub fn normalize_engine(raw: &str) -> String {
    let trimmed = raw.trim().to_ascii_lowercase();
    let stripped = trimmed.strip_prefix("cli-").unwrap_or(&trimmed);
    match stripped {
        "claude-code" => "claude".to_owned(),
        "cursor-agent" => "cursor".to_owned(),
        other => other.to_owned(),
    }
}

pub fn cli_engine_id(raw: &str) -> String {
    let norm = normalize_engine(raw);
    if norm.is_empty() {
        String::new()
    } else {
        format!("cli-{norm}")
    }
}

fn build_worker_models_payload(state: &AppState) -> Value {
    let parsed: Value = serde_json::from_str(MANUAL_MODELS_JSON).unwrap_or_else(|_| json!({}));
    let manual_updated_at = parsed["updated_at"]
        .as_str()
        .unwrap_or("2026-09-27")
        .to_owned();
    let manual_sources = parsed["sources"].as_object().cloned().unwrap_or_default();
    let manual_models_raw = parsed["models"].as_object().cloned().unwrap_or_default();

    let mut manual_models = Map::new();
    let mut discovered_models = Map::new();
    let mut merged_models = Map::new();
    let mut discovery = Map::new();

    for (eng, list) in &manual_models_raw {
        let cli_key = cli_engine_id(eng);
        let arr = list.as_array().cloned().unwrap_or_default();
        manual_models.insert(eng.clone(), json!(arr));
        manual_models.insert(cli_key.clone(), json!(arr));

        let sys_id = format!("system:{eng}");
        let mut disc = Vec::new();
        for cat in runtime_api::native_catalogs(&state.store, &sys_id).values() {
            if let Some(items) = cat["discovered_models"].as_array() {
                for item in items {
                    let entry = if let Some(s) = item.as_str() {
                        json!({"id": s, "label": s})
                    } else {
                        item.clone()
                    };
                    if let Some(id) = entry["id"].as_str()
                        && !id.is_empty()
                        && !disc.iter().any(|x: &Value| x["id"] == id)
                    {
                        disc.push(entry);
                    }
                }
            }
        }
        discovered_models.insert(eng.clone(), json!(disc));
        discovered_models.insert(cli_key.clone(), json!(disc));

        let mut merged = arr;
        for d in &disc {
            if !merged.iter().any(|x| x["id"] == d["id"]) {
                merged.push(d.clone());
            }
        }
        merged_models.insert(eng.clone(), json!(merged));
        merged_models.insert(cli_key.clone(), json!(merged));

        let detail = json!({
            "supported": true,
            "ok": true,
            "source": "manual_plus_runtime",
            "count": merged.len(),
            "error": "",
        });
        discovery.insert(eng.clone(), detail.clone());
        discovery.insert(cli_key, detail);
    }

    let mut models_by_profile = Map::new();
    if let Ok(cfg) = settings_metadata::configuration(state)
        && let Some(profiles) = cfg["worker_profiles"].as_array()
    {
        for p in profiles {
            let pid = p["id"].as_str().unwrap_or("").trim();
            let eng = normalize_engine(p["engine"].as_str().unwrap_or(""));
            if !pid.is_empty()
                && let Some(m) = merged_models.get(&eng)
            {
                models_by_profile.insert(pid.to_owned(), m.clone());
            }
        }
    }

    json!({
        "allow_custom": false,
        "manual_updated_at": manual_updated_at,
        "manual_sources": manual_sources,
        "manual_models": manual_models,
        "discovered_models": discovered_models,
        "models_by_profile": models_by_profile,
        "discovery": discovery,
        "models": merged_models,
    })
}

pub(crate) async fn get_worker_models(State(state): State<Arc<AppState>>) -> Json<Value> {
    Json(build_worker_models_payload(&state))
}

async fn discover_worker_models(State(state): State<Arc<AppState>>) -> Json<Value> {
    Json(build_worker_models_payload(&state))
}

pub async fn evaluate_profile_health(
    state: &AppState,
    profile: &Value,
    backend_override: Option<&str>,
) -> Value {
    let pid = profile["id"].as_str().unwrap_or("").trim().to_owned();
    let raw_engine = profile["engine"].as_str().unwrap_or("").trim().to_owned();
    let norm_engine = normalize_engine(&raw_engine);
    let model = profile["model"].as_str().unwrap_or("").trim().to_owned();
    let account_id = profile["account_id"]
        .as_str()
        .or_else(|| profile["credential_id"].as_str())
        .unwrap_or("")
        .trim()
        .to_owned();
    let enabled = profile["enabled"].as_bool().unwrap_or(true);
    let saved_cfg = settings_metadata::configuration(state).unwrap_or_else(|_| json!({}));
    let backend = backend_override
        .or_else(|| profile["backend"].as_str())
        .or_else(|| saved_cfg["worker_backend"].as_str())
        .unwrap_or("container")
        .trim()
        .to_owned();

    if !enabled {
        return json!({
            "profile_id": pid,
            "engine": raw_engine,
            "backend": backend,
            "status": "disabled",
            "layer": "config",
            "blocker": "",
            "detail": "Worker 已禁用",
            "model": model,
            "account_id": account_id,
            "binding_kind": if account_id.is_empty() { "inherited" } else { "explicit" },
            "effective_credential_id": account_id,
        });
    }

    let Some(desc) = descriptors::find(&norm_engine) else {
        return json!({
            "profile_id": pid,
            "engine": raw_engine,
            "backend": backend,
            "status": "blocked",
            "layer": "engine",
            "blocker": "unsupported_engine",
            "detail": format!("未知或不支持的引擎: {raw_engine}"),
            "model": model,
            "account_id": account_id,
            "binding_kind": "missing",
            "effective_credential_id": "",
        });
    };

    let runtime_key = runtime_api::default_runtime_key(&norm_engine).expect("known engine has a default runtime");
    let instance = match state.store.get(runtime_api::INSTANCES, &runtime_key) {
        Ok(instance) => instance.unwrap_or(Value::Null),
        Err(error) => return json!({"profile_id":pid,"engine":raw_engine,"backend":backend,
            "status":"blocked","layer":"runtime","blocker":"storage.failed","detail":error.to_string()}),
    };
    let explicit_binary = instance["binary_path"].as_str()
        .filter(|value| !value.is_empty()).map(std::path::Path::new);

    let container_img_opt = if backend == "container" {
        let img_status = super::container_exec::inspect_worker_image_status().await;
        if img_status["daemon"]["ok"].as_bool() != Some(true) {
            return json!({
                "profile_id": pid,
                "engine": raw_engine,
                "backend": backend,
                "status": "blocked",
                "layer": "runtime",
                "blocker": "docker_daemon_down",
                "detail": format!(
                    "Docker daemon 未就绪: {}",
                    img_status["daemon"]["detail"].as_str().unwrap_or("")
                ),
                "model": model,
                "account_id": account_id,
                "binding_kind": if account_id.is_empty() { "inherited" } else { "explicit" },
                "effective_credential_id": account_id,
            });
        }
        if img_status["pulled"]["ok"].as_bool() != Some(true) {
            return json!({
                "profile_id": pid,
                "engine": raw_engine,
                "backend": backend,
                "status": "blocked",
                "layer": "runtime",
                "blocker": "worker_image_missing",
                "detail": img_status["pulled"]["detail"]
                    .as_str()
                    .unwrap_or("本地未找到 Worker 容器镜像")
                    .to_owned(),
                "model": model,
                "account_id": account_id,
                "binding_kind": if account_id.is_empty() { "inherited" } else { "explicit" },
                "effective_credential_id": account_id,
            });
        }
        Some(img_status)
    } else {
        if let Err(err) = binaries::resolve(desc, explicit_binary).await {
            return json!({
                "profile_id": pid,
                "engine": raw_engine,
                "backend": backend,
                "status": "blocked",
                "layer": "binary",
                "blocker": "cli_missing",
                "detail": err.message,
                "model": model,
                "account_id": account_id,
                "binding_kind": if account_id.is_empty() { "inherited" } else { "explicit" },
                "effective_credential_id": account_id,
            });
        }
        None
    };

    if let Err(err) = crate::agent_runtime::version_policy::verify_worker_engine(
        state,
        &norm_engine,
        &backend,
        saved_cfg["worker_node_id"].as_str().unwrap_or("local"),
        None,
        explicit_binary,
        container_img_opt.as_ref(),
    )
    .await
    {
        return json!({
            "profile_id": pid,
            "engine": raw_engine,
            "backend": backend,
            "status": "blocked",
            "layer": "version",
            "blocker": err.code,
            "detail": err.message,
            "model": model,
            "account_id": account_id,
            "binding_kind": if account_id.is_empty() { "inherited" } else { "explicit" },
            "effective_credential_id": account_id,
        });
    }

    let (binding_kind, effective_cred) = resolve_effective_credential(state, &norm_engine, &account_id).await;
    if effective_cred.is_empty() {
        return json!({
            "profile_id": pid,
            "engine": raw_engine,
            "backend": backend,
            "status": "auth_failed",
            "layer": "credential",
            "blocker": "credential_missing",
            "detail": format!("{norm_engine} 缺少可用凭据或本机 CLI 登录"),
            "model": model,
            "account_id": account_id,
            "binding_kind": "missing",
            "effective_credential_id": "",
        });
    }

    let credential_check = if backend == "container" {
        super::container_exec::validate_container_credential(
            state,
            &norm_engine,
            &effective_cred,
            non_empty(&model),
        )
        .await
    } else {
        credentials::resolve(
            &state.store,
            &native_chat::state_dir(state),
            desc,
            &effective_cred,
            non_empty(&model),
        )
        .await
        .map(|_| ())
    };
    match credential_check {
        Ok(_) => json!({
            "profile_id": pid,
            "engine": raw_engine,
            "backend": backend,
            "status": "ok",
            "layer": "ready",
            "blocker": "",
            "detail": "就绪",
            "model": model,
            "account_id": account_id,
            "binding_kind": binding_kind,
            "effective_credential_id": effective_cred,
        }),
        Err(err) => json!({
            "profile_id": pid,
            "engine": raw_engine,
            "backend": backend,
            "status": "auth_failed",
            "layer": "credential",
            "blocker": err.code,
            "detail": err.message,
            "model": model,
            "account_id": account_id,
            "binding_kind": binding_kind,
            "effective_credential_id": effective_cred,
        }),
    }
}

fn non_empty(s: &str) -> Option<&str> {
    let t = s.trim();
    if t.is_empty() || t == "default" {
        None
    } else {
        Some(t)
    }
}

pub async fn resolve_effective_credential(
    state: &AppState,
    norm_engine: &str,
    explicit_account: &str,
) -> (&'static str, String) {
    let exp = explicit_account.trim();
    if !exp.is_empty() {
        if exp.starts_with("system:") || exp.starts_with("account:") {
            return ("explicit", exp.to_owned());
        }
        return ("explicit", format!("account:{exp}"));
    }
    if let Ok(accounts) = state.store.list("credential_accounts") {
        for acc in accounts {
            let acc_eng = normalize_engine(
                acc["worker_engine"]
                    .as_str()
                    .or_else(|| acc["engine"].as_str())
                    .unwrap_or(""),
            );
            let id = acc["id"].as_str().unwrap_or("").trim();
            if acc_eng == norm_engine && !id.is_empty() {
                return ("inherited", format!("account:{id}"));
            }
        }
    }
    if host_login::detect(norm_engine, false).await == host_login::PRESENT {
        return ("inherited", format!("system:{norm_engine}"));
    }
    ("missing", String::new())
}

pub(crate) async fn profiles_health_get(
    State(state): State<Arc<AppState>>,
    Query(query): Query<HashMap<String, String>>,
) -> Json<Value> {
    let backend = query.get("backend").map(String::as_str);
    let cfg = settings_metadata::configuration(&state).unwrap_or_else(|_| json!({}));
    let profiles = cfg["worker_profiles"].as_array().cloned().unwrap_or_default();
    let mut out = Vec::with_capacity(profiles.len());
    for p in &profiles {
        out.push(evaluate_profile_health(&state, p, backend).await);
    }
    Json(json!({"profiles": out}))
}

async fn profile_health_single(
    State(state): State<Arc<AppState>>,
    Path(profile_id): Path<String>,
    body: Option<Json<Value>>,
) -> Json<Value> {
    let body = body.map(|Json(v)| v).unwrap_or_else(|| json!({}));
    let backend = body["backend"].as_str();
    let cfg = settings_metadata::configuration(&state).unwrap_or_else(|_| json!({}));
    let profile = cfg["worker_profiles"]
        .as_array()
        .and_then(|arr| arr.iter().find(|p| p["id"].as_str() == Some(profile_id.as_str())))
        .cloned()
        .unwrap_or_else(|| json!({"id": profile_id, "engine": body["engine"].as_str().unwrap_or("cli-claude"), "enabled": true}));
    let health = evaluate_profile_health(&state, &profile, backend).await;
    Json(json!({"profile": health}))
}

async fn profiles_preflight_batch(State(state): State<Arc<AppState>>,body: Option<Json<Value>>) -> Response {
    let body = body.map(|Json(value)|value).unwrap_or_else(||json!({}));
    let Some(profiles) = body["worker_profiles"].as_array().filter(|profiles|(1..=8).contains(&profiles.len())) else {
        return (StatusCode::BAD_REQUEST,Json(json!({"detail":"one to eight worker profiles required"}))).into_response();
    };
    if profiles.iter().any(|profile|!profile.is_object() || profile["enabled"] == false
        || descriptors::find(&normalize_engine(profile["engine"].as_str().unwrap_or(""))).is_none()) {
        return (StatusCode::BAD_REQUEST,Json(json!({"detail":"worker_profiles must contain enabled, known Worker profiles"}))).into_response();
    }
    let settings = match settings_metadata::configuration(&state) {Ok(value)=>value,Err(error)=>return error.into_response()};
    let mut settings = settings; settings["worker_backend"]=json!("local"); settings["worker_network"]=json!("host");
    let mut snapshot = serde_json::Map::new(); let mut failures = Vec::new();
    for profile in profiles {
        let engine = normalize_engine(profile["engine"].as_str().unwrap_or(""));
        let id = profile["id"].as_str().or_else(||profile["name"].as_str()).unwrap_or(&engine);
        let selected = ["credential_account","credential_id","account_id"].iter().find_map(|key|profile[*key].as_str().filter(|value|!value.is_empty())).unwrap_or("");
        let (_,credential) = resolve_effective_credential(&state,&engine,selected).await;
        let runtime = profile["runtime_instance"].as_str().map(str::to_owned).or_else(||runtime_api::default_runtime_key(&engine)).unwrap_or_default();
        match super::readiness::check(&state,profile,"local","host",&credential,&runtime,&settings).await {
            Ok(_) => {snapshot.insert(id.to_owned(),json!(true));},
            Err(error) => {snapshot.insert(id.to_owned(),json!(false)); failures.push(json!({"profile_id":id,"engine":engine,"model":profile["model"],"backend":"local","stage":"preflight","layer":"model","code":error.code,"detail":error.message}));},
        }
    }
    if !failures.is_empty() { return (StatusCode::SERVICE_UNAVAILABLE,Json(json!({"detail":{"failures":failures}}))).into_response(); }
    Json(json!({"ok":true,"profiles":snapshot})).into_response()
}

async fn get_worker_model_test_results(
    State(state): State<Arc<AppState>>,
) -> Json<Value> {
    let mut results = Map::new();
    if let Ok(rows) = state.store.list("worker_model_test_results") {
        for row in rows {
            if let Some(pid) = row["profile_id"].as_str() {
                results.insert(pid.to_owned(), row);
            }
        }
    }
    Json(json!({"results": results}))
}

fn classify_probe_error(code: &str, message: &str, proxy_mode: &str) -> &'static str {
    if code == "runtime.probe_timeout" {
        return "timeout";
    }
    let lower = message.to_ascii_lowercase();
    if lower.contains("407")
        || lower.contains("proxy authentication")
        || lower.contains("proxyconnect")
        || lower.contains("proxy connect")
        || lower.contains("proxy error")
        || lower.contains("socks")
        || lower.contains("tunnel connection failed")
        || lower.contains("err_proxy")
        || lower.contains("failed to connect to proxy")
        || (proxy_mode == "custom"
            && (lower.contains("econnrefused") || lower.contains("connection refused"))
            && (lower.contains("proxy") || lower.contains("127.0.0.1") || lower.contains("host.docker.internal")))
    {
        return "proxy_error";
    }
    if lower.contains("401")
        || lower.contains("403")
        || lower.contains("unauthorized")
        || lower.contains("invalid_api_key")
        || lower.contains("incorrect api key")
        || lower.contains("invalid x-api-key")
        || lower.contains("authentication")
        || lower.contains("permission_error")
        || lower.contains("expired token")
        || lower.contains("token expired")
    {
        return "auth_error";
    }
    if code.contains("credential") {
        return "credential_error";
    }
    if lower.contains("429")
        || lower.contains("500")
        || lower.contains("502")
        || lower.contains("503")
        || lower.contains("504")
        || lower.contains("rate limit")
        || lower.contains("model_not_found")
        || lower.contains("does not exist")
        || lower.contains("upstream")
        || lower.contains("overloaded")
        || lower.contains("econnreset")
        || lower.contains("etimedout")
        || lower.contains("enotfound")
        || lower.contains("tls")
        || lower.contains("ssl")
    {
        return "upstream_error";
    }
    "runtime_error"
}

async fn run_single_worker_model_test(state: &Arc<AppState>, spec: &Value) -> Value {
    let started = Instant::now();
    let profile = &spec["profile"];
    let profile_id = spec["profile_id"]
        .as_str()
        .filter(|s| !s.trim().is_empty())
        .or_else(|| profile["id"].as_str())
        .unwrap_or("")
        .trim()
        .to_owned();
    let raw_engine = spec["engine"]
        .as_str()
        .filter(|s| !s.trim().is_empty())
        .or_else(|| profile["engine"].as_str())
        .unwrap_or("")
        .trim()
        .to_owned();
    let norm_engine = normalize_engine(&raw_engine);
    let model = spec["model"]
        .as_str()
        .filter(|s| !s.trim().is_empty())
        .or_else(|| profile["model"].as_str())
        .unwrap_or("")
        .trim()
        .to_owned();
    let saved_cfg = settings_metadata::configuration(state).unwrap_or_else(|_| json!({}));
    let backend = spec["backend"]
        .as_str()
        .filter(|s| !s.trim().is_empty())
        .or_else(|| profile["backend"].as_str())
        .or_else(|| saved_cfg["worker_backend"].as_str())
        .unwrap_or("container")
        .trim()
        .to_owned();
    let account_id = spec["account_id"]
        .as_str()
        .filter(|s| !s.trim().is_empty())
        .or_else(|| {
            profile["credential_account"]
                .as_str()
                .filter(|s| !s.trim().is_empty())
        })
        .or_else(|| {
            profile["credential_id"]
                .as_str()
                .filter(|s| !s.trim().is_empty())
        })
        .unwrap_or("")
        .trim()
        .to_owned();
    let reasoning_effort = spec["reasoning_effort"]
        .as_str()
        .filter(|s| !s.trim().is_empty())
        .or_else(|| profile["reasoning_effort"].as_str())
        .unwrap_or("")
        .trim()
        .to_owned();
    let reasoning_effort = if reasoning_effort == "default" || reasoning_effort == "inherit" {
        String::new()
    } else {
        reasoning_effort
    };

    let Some(adapter_key) = runtime_api::default_runtime_key(&norm_engine) else {
        let elapsed = started.elapsed().as_millis() as u64;
        return json!({
            "profile_id": profile_id,
            "ok": false,
            "detail": format!("不支持的引擎: {raw_engine}"),
            "model": model,
            "engine": raw_engine,
            "backend": backend,
            "elapsed_ms": elapsed,
            "layer": "engine",
            "tested_at": now_secs(),
            "logs": [{"stream": "stderr", "message": format!("Unsupported engine {raw_engine}"), "elapsed_ms": elapsed}],
        });
    };

    let instance = match state.store.get(runtime_api::INSTANCES, &adapter_key) {
        Ok(instance) => instance.unwrap_or(Value::Null),
        Err(error) => return json!({"profile_id":profile_id,"ok":false,"engine":raw_engine,
            "backend":backend,"layer":"runtime","code":"storage.failed","detail":error.to_string()}),
    };
    let explicit_binary = instance["binary_path"].as_str()
        .filter(|value| !value.is_empty()).map(std::path::Path::new);

    if let Err(err) = crate::agent_runtime::version_policy::verify_worker_engine(
        state,
        &norm_engine,
        &backend,
        saved_cfg["worker_node_id"].as_str().unwrap_or("local"),
        spec["mode"].as_str(),
        explicit_binary,
        None,
    )
    .await
    {
        let elapsed = started.elapsed().as_millis() as u64;
        let res = json!({
            "profile_id": profile_id,
            "ok": false,
            "detail": err.message,
            "model": model,
            "engine": raw_engine,
            "backend": backend,
            "elapsed_ms": elapsed,
            "layer": err.code,
            "tested_at": now_secs(),
            "logs": [{"stream": "stderr", "message": err.message, "elapsed_ms": elapsed}],
        });
        if !profile_id.is_empty() {
            let _ = state.store.put("worker_model_test_results", &profile_id, &res);
        }
        return res;
    }

    let resolved_proxy = crate::network_proxy::active();
    let proxy_mode = resolved_proxy.mode.clone();
    let proxy_display = resolved_proxy.redacted_url.clone().unwrap_or_default();
    let secret_refs: Vec<&str> = resolved_proxy.secrets.iter().map(String::as_str).collect();

    let (_, effective_cred) = resolve_effective_credential(state, &norm_engine, &account_id).await;
    if effective_cred.is_empty() {
        let elapsed = started.elapsed().as_millis() as u64;
        return json!({
            "profile_id": profile_id,
            "ok": false,
            "detail": format!("{norm_engine} 缺少可用凭据或本机登录"),
            "model": model,
            "engine": raw_engine,
            "backend": backend,
            "elapsed_ms": elapsed,
            "layer": "credential",
            "error_kind": "credential_error",
            "proxy_mode": proxy_mode,
            "proxy_display": proxy_display,
            "tested_at": now_secs(),
            "logs": [{"stream": "stderr", "message": "Missing credential or host login", "elapsed_ms": elapsed}],
        });
    }

    let probe_res = if backend == "container" {
        let cfg = settings_metadata::configuration(state).unwrap_or_else(|_| json!({}));
        super::container_exec::run_container_model_probe(
            state,
            &norm_engine,
            &effective_cred,
            non_empty(&model),
            non_empty(&reasoning_effort),
            &cfg,
        )
        .await
    } else {
        super::readiness::local_probe(state,&norm_engine,&effective_cred,&model,&reasoning_effort,&instance).await
    };

    match probe_res {
        Ok(reply) => {
            let elapsed = started.elapsed().as_millis() as u64;
            let served = reply["model"].as_str().unwrap_or(&model).to_owned();
            let text = settings_metadata::redact_secrets(
                reply["reply"].as_str().unwrap_or("OK"),
                &secret_refs,
            );
            let detail = settings_metadata::redact_secrets(&format!("{served} 响应: {text}"), &secret_refs);
            let res = json!({
                "profile_id": profile_id,
                "ok": true,
                "detail": detail,
                "model": served,
                "requested_model": reply["requested_model"],
                "model_identity_source": reply["model_identity_source"],
                "engine": raw_engine,
                "backend": backend,
                "elapsed_ms": elapsed,
                "layer": "model",
                "proxy_mode": proxy_mode,
                "proxy_display": proxy_display,
                "tested_at": now_secs(),
                "logs": [{"stream": "stdout", "message": text, "elapsed_ms": elapsed}],
            });
            if !profile_id.is_empty() {
                let _ = state.store.put("worker_model_test_results", &profile_id, &res);
            }
            res
        }
        Err(err) => {
            let elapsed = started.elapsed().as_millis() as u64;
            let error_kind = classify_probe_error(err.code, &err.message, &proxy_mode);
            let safe_msg = settings_metadata::redact_secrets(&err.message, &secret_refs);
            let res = json!({
                "profile_id": profile_id,
                "ok": false,
                "detail": safe_msg,
                "model": model,
                "engine": raw_engine,
                "backend": backend,
                "elapsed_ms": elapsed,
                "layer": err.code,
                "error_kind": error_kind,
                "proxy_mode": proxy_mode,
                "proxy_display": proxy_display,
                "tested_at": now_secs(),
                "logs": [{"stream": "stderr", "message": safe_msg, "elapsed_ms": elapsed}],
            });
            if !profile_id.is_empty() {
                let _ = state.store.put("worker_model_test_results", &profile_id, &res);
            }
            res
        }
    }
}

async fn test_worker_model_single(
    State(state): State<Arc<AppState>>,
    Json(body): Json<Value>,
) -> Json<Value> {
    Json(run_single_worker_model_test(&state, &body).await)
}

async fn test_worker_model_batch(
    State(state): State<Arc<AppState>>,
    Json(body): Json<Value>,
) -> Json<Value> {
    let saved_cfg = settings_metadata::configuration(&state).unwrap_or_else(|_| json!({}));
    let backend = body["backend"].as_str()
        .or_else(|| saved_cfg["worker_backend"].as_str())
        .unwrap_or("container").to_owned();
    let items = body["items"].as_array().cloned().unwrap_or_default();
    let mut results = Vec::new();
    for item in &items {
        let mut merged = item.clone();
        if merged["backend"].is_null() {
            merged["backend"] = json!(backend);
        }
        let res = run_single_worker_model_test(&state, &merged).await;
        results.push(res);
    }
    Json(json!({
        "backend": backend,
        "container_count": if backend == "container" { results.len() } else { 0 },
        "results": results,
    }))
}

pub(crate) async fn test_llm_endpoint(
    State(state): State<Arc<AppState>>,
    Json(body): Json<Value>,
) -> Json<Value> {
    let started = Instant::now();
    let endpoint_id = body["endpoint_id"].as_str().unwrap_or("").trim();
    let model = body["model"].as_str().unwrap_or("").trim();
    if endpoint_id.is_empty() {
        return Json(json!({
            "ok": false,
            "detail": "未选择模型端点",
            "model": model,
            "elapsed_ms": 0,
        }));
    }
    let cred_id = if endpoint_id.starts_with("account:") || endpoint_id.starts_with("endpoint:") || endpoint_id.starts_with("system:") {
        endpoint_id.to_owned()
    } else {
        format!("account:{endpoint_id}")
    };
    match crate::model_settings_api::test_selected(state,cred_id,json!({
        "model":model,"temperature_mode":body["temperature_mode"],"temperature":body["temperature"],"backend":"local",
    })).await {
        Ok(Json(reply)) => Json(reply),
        Err((status,Json(failure))) => Json(json!({"ok":false,
            "detail":failure["detail"].as_str().unwrap_or_else(||status.canonical_reason().unwrap_or("HTTP model test failed")),
            "model":model,"elapsed_ms":started.elapsed().as_millis() as u64})),
    }
}

pub(crate) async fn get_worker_image() -> Json<Value> {
    Json(super::container_exec::inspect_worker_image_status().await)
}

pub(crate) async fn pull_worker_image() -> Response {
    let (ok, payload) = super::container_exec::pull_or_verify_worker_image().await;
    if ok {
        Json(payload).into_response()
    } else {
        (StatusCode::UNPROCESSABLE_ENTITY, Json(payload)).into_response()
    }
}

async fn put_identity(
    State(state): State<Arc<AppState>>,
    Json(patch): Json<Value>,
) -> Response {
    let mut patch = patch;
    if let Some(object) = patch.as_object_mut() {
        object.retain(|key,_|matches!(key.as_str(),"seats" | "credentials" | "worker_backend" | "worker_network" | "seat_alias" | "credential_alias"));
    }
    settings_metadata::put_workers(State(state),Json(patch)).await.into_response()
}

#[cfg(test)]
mod tests {
    use super::classify_probe_error;

    #[test]
    fn classify_probe_error_covers_all_categories() {
        assert_eq!(classify_probe_error("runtime.probe_timeout", "timed out", "inherit"), "timeout");
        assert_eq!(classify_probe_error("runtime.failed", "HTTP 407 Proxy Authentication Required", "inherit"), "proxy_error");
        assert_eq!(classify_probe_error("runtime.failed", "proxyconnect tcp: dial error", "inherit"), "proxy_error");
        assert_eq!(classify_probe_error("runtime.failed", "connect ECONNREFUSED 127.0.0.1:7890", "custom"), "proxy_error");
        assert_eq!(classify_probe_error("runtime.failed", "401 Unauthorized: invalid_api_key", "custom"), "auth_error");
        assert_eq!(classify_probe_error("credential.missing", "missing", "inherit"), "credential_error");
        assert_eq!(classify_probe_error("runtime.failed", "HTTP 502 Bad Gateway", "inherit"), "upstream_error");
        assert_eq!(classify_probe_error("runtime.failed", "429 rate limit exceeded", "inherit"), "upstream_error");
        assert_eq!(classify_probe_error("runtime.failed", "model_not_found", "inherit"), "upstream_error");
        assert_eq!(classify_probe_error("runtime.failed", "unexpected subprocess exit 1", "inherit"), "runtime_error");
    }
}
