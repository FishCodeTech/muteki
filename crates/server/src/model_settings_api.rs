//! Connectivity and discovery use only the selected, stored HTTP configuration.
mod draft_native_catalog;
use crate::{AppState, provider};
use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::StatusCode,
    routing::post,
};
use serde_json::{Value, json};
use std::{
    sync::Arc,
    time::{Duration, Instant},
};

type ApiResult = Result<Json<Value>, (StatusCode, Json<Value>)>;
fn error(status: StatusCode, detail: &str) -> (StatusCode, Json<Value>) {
    (status, Json(json!({"detail":detail})))
}
fn storage(_: impl std::fmt::Display) -> (StatusCode, Json<Value>) {
    error(
        StatusCode::INTERNAL_SERVER_ERROR,
        "Could not save model settings",
    )
}
fn text<'a>(row: &'a Value, key: &str) -> &'a str {
    row[key].as_str().unwrap_or("").trim()
}
fn runtime_key(body: &Value, engine: &str) -> String {
    let runtime = text(body, "runtime_instance");
    if runtime.contains(':') {
        runtime.into()
    } else {
        format!(
            "{engine}-http:{}",
            if runtime.is_empty() { "text" } else { runtime }
        )
    }
}

/// The native runtime a draft test starts. A named native instance wins.
/// An omitted or HTTP runtime still starts the engine the caller selected.
fn draft_runtime(body: &Value, engine: &str) -> Result<String, (StatusCode, Json<Value>)> {
    let requested = text(body, "runtime_instance");
    if crate::runtime_api::is_native_runtime(requested) {
        return Ok(requested.to_owned());
    }
    crate::runtime_api::default_runtime_key(engine).ok_or_else(|| {
        error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "Selected engine has no native runtime",
        )
    })
}
pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/settings/credentials/{id}/test", post(test_credential))
        .route(
            "/api/settings/credential-accounts/{id}/test",
            post(test_account),
        )
        .route(
            "/api/settings/model-endpoints/{id}/test",
            post(test_endpoint),
        )
        .route(
            "/api/settings/credentials/{id}/models/refresh",
            post(refresh_models),
        )
        .route("/api/settings/credential-drafts/test", post(test_draft))
        .route(
            "/api/settings/credential-models/refresh-stale",
            post(refresh_stale),
        )
}
async fn test_account(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    body: Option<Json<Value>>,
) -> ApiResult {
    test_credential(State(state), Path(format!("account:{id}")), body).await
}
async fn test_endpoint(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    body: Option<Json<Value>>,
) -> ApiResult {
    test_selected(
        state,
        format!("endpoint:{}", id.strip_prefix("endpoint:").unwrap_or(&id)),
        body.map(|v| v.0).unwrap_or_else(|| json!({})),
    )
    .await
}
pub(crate) async fn test_credential(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    body: Option<Json<Value>>,
) -> ApiResult {
    let body = body.map(|v| v.0).unwrap_or_else(|| json!({}));
    if let Some(runtime) = crate::native_credentials::native_target(&state.store, &id, &body) {
        return crate::native_credentials::test(state, &id, runtime, body).await;
    }
    test_selected(state, id, body).await
}
fn selected(state: &AppState, id: &str, body: &Value) -> Result<Value, (StatusCode, Json<Value>)> {
    if !body.is_object() {
        return Err(error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "Expected an object",
        ));
    }
    let row = provider::matching_account(&state.store, id)
        .map_err(storage)?
        .ok_or_else(|| error(StatusCode::NOT_FOUND, "Stored HTTP credential not found"))?;
    if !text(body, "secret").is_empty()
        || !text(body, "api_key").is_empty()
        || (!text(body, "base_url").is_empty()
            && text(body, "base_url").trim_end_matches('/')
                != text(&row, "base_url").trim_end_matches('/'))
    {
        return Err(error(
            StatusCode::CONFLICT,
            "Save the endpoint and credential before testing or refreshing models",
        ));
    }
    let engine = [text(&row, "worker_engine"), text(&row, "target_engine"), text(&row, "engine")]
        .into_iter().find(|engine| !engine.is_empty()).unwrap_or("");
    if !text(body, "engine").is_empty() && !engine.is_empty() && engine != "api" && text(body, "engine") != engine {
        return Err(error(
            StatusCode::CONFLICT,
            "Credential engine does not match the selected engine",
        ));
    }
    Ok(row)
}
fn client() -> Result<reqwest::Client, String> {
    crate::network_proxy::http_client_builder()
        .timeout(Duration::from_secs(45))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|_| "Could not initialize HTTP provider connection".into())
}
fn catalog_client() -> Result<reqwest::Client, String> {
    crate::network_proxy::http_client_builder()
        .timeout(Duration::from_secs(20))
        .redirect(reqwest::redirect::Policy::custom(|attempt| {
            let target = attempt.url();
            let previous = attempt.previous().last();
            if attempt.previous().len() < 10 && previous.is_some_and(|previous| {
                previous.scheme() == target.scheme()
                    && previous.host_str() == target.host_str()
                    && previous.port_or_known_default() == target.port_or_known_default()
            }) { attempt.follow() } else { attempt.stop() }
        }))
        .build()
        .map_err(|_| "Could not initialize HTTP provider connection".into())
}
async fn response_json(response: reqwest::Response) -> Result<Value, String> {
    if !response.status().is_success() {
        return Err(format!(
            "HTTP provider returned status {}",
            response.status().as_u16()
        ));
    }
    response.json::<Value>().await.map_err(|_| "HTTP provider returned invalid JSON".into())
}
pub(crate) async fn test_selected(state: Arc<AppState>, id: String, body: Value) -> ApiResult {
    if state.admission.draining(){return Err(error(StatusCode::SERVICE_UNAVAILABLE,"Service activation or maintenance is in progress"));}
    let row = selected(&state, &id, &body)?;
    let model = text(&body, "model");
    let backend = if text(&body, "backend") == "container" { "container" } else { "local" };
    let started = Instant::now();
    let result: Result<String, String> = async {
        if model.is_empty() { return Err("Select a model before running a real connectivity test".into()); }
        if backend == "container" {
            let engine = [text(&row, "worker_engine"), text(&row, "target_engine"), text(&row, "engine")]
                .into_iter().find(|engine| !engine.is_empty()).unwrap_or("api");
            let settings = crate::settings_metadata::configuration(&state)
                .map_err(|_| "Could not read worker settings")?;
            return crate::runs::container_exec::run_container_model_probe(
                &state, engine, &id, Some(model),
                (!text(&body, "reasoning_effort").is_empty()).then(|| text(&body, "reasoning_effort")),
                &settings,
            ).await.map(|reply| format!("{} 返回：{}", model, text(&reply, "reply")))
                .map_err(|error| error.message);
        }
        let config = provider::provider_from(&row,&id,Some(model)).map_err(|e|e.to_string())?;
        let mut probe = json!({"model":model,"messages":[{"role":"user","content":"Reply with OK."}],"stream":false,"max_tokens":1024});
        match text(&body,"temperature_mode") {
            "" | "default" | "omit" => {},
            "custom" => {
                let value = body["temperature"].as_f64().filter(|value|value.is_finite() && (0.0..=2.0).contains(value))
                    .ok_or_else(||"Temperature must be between 0 and 2".to_owned())?;
                probe["temperature"] = json!(value);
            },
            _ => return Err("Invalid temperature mode".into()),
        }
        let mut request = client()?.post(config.endpoint).json(&probe);
        if let Some(token) = config.token { request = request.bearer_auth(token); }
        let response = request.send().await.map_err(|_|"Could not connect to selected HTTP provider")?;
        let payload = response_json(response).await?;
        if payload.get("error").is_some_and(|e|!e.is_null()) { return Err("HTTP provider returned an API error".into()); }
        let content = &payload["choices"][0]["message"]["content"];
        let has_text = content.as_str().is_some_and(|s|!s.trim().is_empty())
            || content.as_array().is_some_and(|parts|parts.iter().any(|p|text(p,"text").len()>0));
        if !has_text { return Err("HTTP provider did not return a text completion".into()); }
        Ok("Selected HTTP model returned a text completion".into())
    }.await;
    state.store.document_scope(|| -> ApiResult {
    if provider::matching_account(&state.store, &id).map_err(storage)?.as_ref() != Some(&row) {
        return Err(error(
            StatusCode::CONFLICT,
            "Credential configuration changed during the test",
        ));
    }
    let ok = result.is_ok();
    let detail = result.unwrap_or_else(|error| error);
    let engine = [text(&row, "worker_engine"), text(&row, "target_engine"), text(&row, "engine")]
        .into_iter().find(|engine| !engine.is_empty()).unwrap_or("api");
    let runtime = runtime_key(&body, engine);
    let now = chrono::Utc::now().timestamp_millis() as f64 / 1000.0;
    let last = json!({"ok":ok,"status":if ok {"ok"} else {"failed"},"detail":detail,"model":model,"engine":engine,"backend":backend,"runtime_instance":runtime,"tested_at":now,"elapsed_ms":started.elapsed().as_millis() as u64,"layer":"model","credential_id":id});
    state
        .store
        .put("model_test_results", &if backend == "container" { format!("{id}|container|{runtime}") } else { format!("{id}|{runtime}") }, &last)
        .map_err(storage)?;
    if !model.is_empty() {
        let key = if backend == "container" { format!("http:container:{id}|{runtime}") } else { format!("http:{id}|{runtime}") };
        let mut catalog = state.store.get("model_catalogs", &key).map_err(storage)?.unwrap_or_else(||json!({"credential_id":id,"engine":engine,"environment":backend,"runtime_instance":runtime,"configured_models":row["models"],"discovered_models":[],"verified_models":[],"source":"http_openai_models","refresh_status":"missing"}));
        let mut verified = catalog["verified_models"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        verified.retain(|v| v.as_str() != Some(model));
        if ok {
            verified.push(json!(model));
        }
        catalog["verified_models"] = json!(verified);
        state
            .store
            .put("model_catalogs", &key, &catalog)
            .map_err(storage)?;
    }
    let mut response = last.clone();
    response["last_test"] = last;
    Ok(Json(response))
    }).map_err(storage)?
}

fn pi_catalog_error(message: impl std::fmt::Display) -> crate::agent_runtime::RuntimeError {
    crate::agent_runtime::RuntimeError::new("catalog_request_failed", message.to_string())
}

fn extract_positive_u64(item: &Value, direct_keys: &[&str], nested_paths: &[&str]) -> Option<u64> {
    direct_keys
        .iter()
        .filter_map(|key| item.get(*key))
        .chain(nested_paths.iter().filter_map(|path| item.pointer(path)))
        .find_map(|value| value.as_u64().filter(|count| *count > 0))
}

fn extract_input_modalities(item: &Value) -> Option<Value> {
    let raw = item
        .get("input")
        .filter(|value| value.is_array())
        .or_else(|| {
            item.pointer("/architecture/input_modalities")
                .filter(|value| value.is_array())
        })?;
    let mut values = Vec::new();
    for entry in raw.as_array()? {
        let Some(kind) = entry.as_str().map(str::trim).filter(|s| !s.is_empty()) else {
            continue;
        };
        if !values.iter().any(|existing| existing == kind) {
            values.push(kind.to_owned());
        }
    }
    (!values.is_empty()).then(|| json!(values))
}

async fn confirm_pi_endpoint_models(
    state: &AppState,
    id: &str,
    row: &Value,
    base_url: &str,
    secret: &str,
    environment: &str,
    runtime: &str,
    models: &mut Vec<Value>,
) -> Result<(), crate::agent_runtime::RuntimeError> {
    let account_base = id.strip_prefix("account:").map(|account_id| {
        crate::account_material::base(&crate::native_chat::state_dir(state),account_id,row)
            .map_err(pi_catalog_error)
    }).transpose()?;
    let definitions: Vec<Value> = models.iter().map(|model| -> Result<Value, crate::agent_runtime::RuntimeError> {
        let model_id = text(model, "id");
        let capability = crate::agent_runtime::credentials::pi_model_capabilities(
            account_base.as_deref(), row, model_id,
        )?;
        let mut definition = json!({"id":model_id,
            "input":capability["input"].as_array().cloned()
                .or_else(|| model["input"].as_array().cloned())
                .unwrap_or_else(||vec![json!("text")]),
            "reasoning":capability["reasoning"].as_bool()
                .unwrap_or_else(|| model["reasoning"]["supported"].as_bool().unwrap_or(false))});
        for (source, target) in [
            ("context_window", "contextWindow"), ("contextWindow", "contextWindow"),
            ("max_output_tokens", "maxTokens"), ("maxTokens", "maxTokens"),
            ("thinkingLevelMap", "thinkingLevelMap"),
        ] {
            if let Some(value) = capability.get(source) { definition[target] = value.clone(); }
        }
        if definition.get("contextWindow").is_none()
            && let Some(window) = model["context_window"].as_u64()
        {
            definition["contextWindow"] = json!(window);
        }
        if definition.get("maxTokens").is_none()
            && let Some(tokens) = model["max_output_tokens"].as_u64()
        {
            definition["maxTokens"] = json!(tokens);
        }
        Ok(definition)
    }).collect::<Result<Vec<_>, _>>()?;
    let scratch_root = crate::native_chat::state_dir(state).join("_tmp").join("pi-endpoint-catalogs");
    std::fs::create_dir_all(&scratch_root).map_err(pi_catalog_error)?;
    let scratch = tempfile::Builder::new().prefix("pi-catalog-").tempdir_in(&scratch_root)
        .map_err(pi_catalog_error)?;
    let store = crate::store::Store::open(scratch.path().join("muteki.sqlite"))
        .map_err(pi_catalog_error)?;
    store.put("system", "state_dir", &json!(scratch.path())).map_err(pi_catalog_error)?;
    store.put("system", "node_state_dir", &json!(crate::native_chat::node_state_dir(state))).map_err(pi_catalog_error)?;
    store.put("credential_accounts", "endpoint-catalog", &json!({
        "account_id":"endpoint-catalog","engine":"pi","worker_engine":"pi",
        "connection":"custom_endpoint","base_url":base_url,
        "secret":if secret.is_empty() { "catalog-only" } else { secret },
        "models":models.iter().map(|model| model["id"].clone()).collect::<Vec<_>>(),
        "default_model":models.first().map(|model| model["id"].clone()).unwrap_or(Value::Null),
        "present":true,
    })).map_err(pi_catalog_error)?;
    let scratch_state = state.scratch_for(store);
    let inventory = if environment == "container" {
        let settings = crate::settings_metadata::configuration(state)
            .map_err(|_| pi_catalog_error("Could not read worker settings"))?;
        crate::runs::container_exec::run_container_pi_catalog(
            &scratch_state, "account:endpoint-catalog", &settings, Some(&definitions),
        ).await?
    } else {
        let agent_dir = scratch.path().join("agent");
        std::fs::create_dir_all(&agent_dir).map_err(pi_catalog_error)?;
        let catalog_key = if secret.is_empty() { "catalog-only" } else { secret };
        let config = json!({"providers":{"muteki":{
            "baseUrl":base_url,"api":"openai-completions",
            "apiKey":catalog_key,"models":definitions}}});
        crate::agent_runtime::credentials::private_file(
            &agent_dir.join("models.json"), &serde_json::to_vec(&config).map_err(pi_catalog_error)?,
        )?;
        crate::agent_runtime::credentials::private_file(
            &agent_dir.join("settings.json"),
            &serde_json::to_vec(&json!({"defaultProvider":"muteki",
                "defaultModel":models.first().map(|model| model["id"].clone()).unwrap_or(Value::Null)}))
                .map_err(pi_catalog_error)?,
        )?;
        let script = scratch.path().join("pi-model-catalog.mjs");
        std::fs::write(&script, include_str!("../../../docker/worker-common/pi_model_catalog.mjs"))
            .map_err(pi_catalog_error)?;
        let descriptor = crate::agent_runtime::descriptors::find("pi")
            .ok_or_else(|| pi_catalog_error("Pi runtime descriptor is missing"))?;
        let configured = state.store.get(crate::runtime_api::INSTANCES, runtime)
            .map_err(pi_catalog_error)?;
        let explicit = configured.as_ref().and_then(|row| row["binary_path"].as_str())
            .filter(|path| !path.is_empty()).map(std::path::Path::new);
        let binary = crate::agent_runtime::binaries::resolve(descriptor, explicit)
            .await?;
        let mut node = crate::host_process::node_command();
        if let Ok(path_env) = crate::agent_runtime::binaries::launch_path(&binary.path) {
            node.env("PATH", path_env);
        }
        let output = tokio::time::timeout(Duration::from_secs(35),
            node
                .arg(&script).arg(&binary.path)
                .env("PI_CODING_AGENT_DIR", &agent_dir)
                .env("PI_PROVIDER", "muteki")
                .env("HOME", scratch.path())
                .env("XDG_CONFIG_HOME", scratch.path().join(".config"))
                .output())
            .await.map_err(|_| pi_catalog_error("Pi RPC model discovery timed out"))?
            .map_err(pi_catalog_error)?;
        if !output.status.success() {
            return Err(pi_catalog_error("Pi RPC model discovery failed"));
        }
        let payload: Value = serde_json::from_slice(&output.stdout)
            .map_err(|_| pi_catalog_error("Pi RPC returned invalid model JSON"))?;
        payload["models"].as_array().ok_or_else(|| pi_catalog_error("Pi RPC model inventory is missing"))?
            .iter().filter(|item| item["provider"] == "muteki")
            .filter_map(|item| item["id"].as_str().map(|id| json!({
                "id":id,"reasoning":crate::agent_runtime::catalog::reasoning(
                    &item["levels"], &item["default"], "pi_rpc")
            }))).collect()
    };
    for model in models {
        let item = inventory.iter().find(|item| item["id"] == model["id"])
            .ok_or_else(|| pi_catalog_error("Pi RPC returned an incomplete endpoint model catalog"))?;
        model["reasoning"] = item["reasoning"].clone();
    }
    Ok(())
}

async fn refresh_official_draft(
    state: &AppState,
    id: &str,
    saved: &Value,
    body: &Value,
    runtime: String,
) -> ApiResult {
    let root = crate::native_chat::state_dir(state).join("_tmp").join("credential-drafts");
    std::fs::create_dir_all(&root).map_err(storage)?;
    let scratch = tempfile::Builder::new().prefix("muteki-official-catalog-draft-")
        .tempdir_in(&root).map_err(storage)?;
    let store = crate::store::Store::open(scratch.path().join("muteki.sqlite")).map_err(storage)?;
    store.put("system", "state_dir", &json!(scratch.path())).map_err(storage)?;
    store.put("system", "node_state_dir", &json!(crate::native_chat::node_state_dir(state))).map_err(storage)?;
    let account_id = id.strip_prefix("account:").unwrap_or(id);
    let mut account = saved.clone();
    let supplied_secret = [text(body, "secret"), text(body, "api_key")]
        .into_iter().find(|secret| !secret.is_empty()).unwrap_or("");
    if !supplied_secret.is_empty() { account["secret"] = json!(supplied_secret); }
    account["base_url"] = json!("");
    account["connection"] = json!("official");
    store.put("credential_accounts", account_id, &account).map_err(storage)?;
    if let Some(config) = state.store.get(crate::runtime_api::INSTANCES, &runtime).map_err(storage)? {
        store.put(crate::runtime_api::INSTANCES, &runtime, &config).map_err(storage)?;
    }
    let scratch_state = Arc::new(state.scratch_for(store));
    struct DraftCleanup { state: Arc<AppState>, scratch: Option<tempfile::TempDir> }
    impl Drop for DraftCleanup {
        fn drop(&mut self) {
            let Some(scratch) = self.scratch.take() else { return; };
            let state = self.state.clone();
            if let Ok(runtime) = tokio::runtime::Handle::try_current() {
                runtime.spawn(async move {
                    if state.agents.close_in_workspaces(&[scratch.path().to_path_buf()]).await.is_err() {
                        let path = scratch.keep();
                        tracing::warn!(path=%path.display(),"Draft credential cleanup awaits owned-process recovery");
                    }
                });
            } else { let _ = scratch.keep(); }
        }
    }
    let _cleanup = DraftCleanup { state: scratch_state.clone(), scratch: Some(scratch) };
    let response = if text(body, "backend") == "container" {
        crate::native_credentials::refresh_container(scratch_state, id, runtime).await?
    } else {
        crate::native_credentials::refresh(scratch_state, id, runtime).await?
    };
    if provider::matching_account(&state.store, id).map_err(storage)?.as_ref() != Some(saved) {
        return Err(error(StatusCode::CONFLICT, "Credential configuration changed during catalog refresh"));
    }
    let Json(mut value) = response;
    value["catalog"]["draft"] = json!(true);
    Ok(Json(value))
}

pub(crate) async fn refresh_models(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    body: Option<Json<Value>>,
) -> ApiResult {
    let body = body.map(|v| v.0).unwrap_or_else(|| json!({}));
    if !body.is_object() {
        return Err(error(StatusCode::UNPROCESSABLE_ENTITY, "Expected an object"));
    }
    let saved = provider::matching_account(&state.store, &id).map_err(storage)?;
    let saved_base = saved.as_ref().map(|row| text(row, "base_url")).unwrap_or("");
    let base_url = if body.get("base_url").is_some() { text(&body, "base_url") } else { saved_base };
    let custom = !base_url.is_empty() || text(&body, "connection") == "custom_endpoint";
    if !custom {
        let saved_engine = saved.as_ref().map(|row| {
            [text(row, "worker_engine"), text(row, "target_engine"), text(row, "engine")]
                .into_iter().find(|engine| !engine.is_empty()).unwrap_or("")
        }).unwrap_or("");
        let engine = if text(&body, "engine").is_empty() { saved_engine } else { text(&body, "engine") };
        if matches!(engine, "codex" | "cursor")
            && (text(&body, "backend") == "container"
                || (!id.starts_with("system:") && saved.is_none())
                || !text(&body, "secret").is_empty()
                || !text(&body, "api_key").is_empty()) {
            return Ok(Json(json!({"ok":false,"source":engine,"models":[],
                "credential_id":id,"catalog":{"discovered_models":[],"draft":true},
                "error_code":"model_catalog.invalid_scope",
                "detail":format!("{engine} 模型目录需要本地环境与已保存的凭据配置。")})));
        }
        if saved.is_none() && !id.starts_with("system:") && matches!(engine, "pi" | "kimi" | "omp" | "droid" | "grok" | "opencode") {
            return if engine == "pi" {
                draft_native_catalog::refresh_pi(&state, &id, &body).await.map(Json)
            } else {
                draft_native_catalog::refresh_cli(&state, &id, &body, engine).await.map(Json)
            };
        }
        let draft = !text(&body, "secret").is_empty()
            || !text(&body, "api_key").is_empty() || !saved_base.is_empty();
        if draft && engine == "claude" {
            let reference: Value = serde_json::from_str(include_str!("runs/worker_models.manual.json"))
                .map_err(|_| error(StatusCode::INTERNAL_SERVER_ERROR, "Could not read reference catalog"))?;
            let models = reference["models"][engine].as_array().cloned().unwrap_or_default();
            let ids: Vec<_> = models.iter().filter_map(|model| model["id"].as_str()).collect();
            return Ok(Json(json!({"ok":true,"source":format!("{engine}_reference_catalog"),
                "detail":"Claude 没有稳定的模型清单命令；这里显示参考目录，真实可用性以手动连通测试为准",
                "models":ids,"credential_id":id,"catalog":{"discovered_models":models,"draft":true},"error_code":""})));
        }
        if let Some(runtime) = crate::native_credentials::native_target(&state.store, &id, &body)
            .or_else(|| (saved.is_some()
                && matches!(text(&body, "runtime_instance"), "" | "default"))
                .then(|| crate::runtime_api::default_runtime_key(engine)).flatten()) {
            if draft && let Some(saved) = &saved {
                return refresh_official_draft(&state, &id, saved, &body, runtime).await;
            }
            if text(&body, "backend") == "container" {
                return crate::native_credentials::refresh_container(state, &id, runtime).await;
            }
            return crate::native_credentials::refresh(state, &id, runtime).await;
        }
        return Err(error(StatusCode::NOT_IMPLEMENTED, "This credential has no model catalog endpoint"));
    }
    if base_url.is_empty() {
        return Err(error(StatusCode::BAD_REQUEST, "Custom endpoint Base URL is required"));
    }
    let supplied_secret = [text(&body, "secret"), text(&body, "api_key")]
        .into_iter().find(|secret| !secret.is_empty()).unwrap_or("");
    let saved_connection = if saved_base.is_empty() { "official" } else { "custom_endpoint" };
    let draft = saved.is_none() || !supplied_secret.is_empty()
        || base_url.trim_end_matches('/') != saved_base.trim_end_matches('/')
        || (!text(&body, "connection").is_empty() && text(&body, "connection") != saved_connection);
    if saved.is_some() && base_url.trim_end_matches('/') != saved_base.trim_end_matches('/') && supplied_secret.is_empty() {
        return Err(error(StatusCode::CONFLICT, "The changed endpoint needs its own API key"));
    }
    let saved_engine = saved.as_ref().map(|row| {
        [text(row, "worker_engine"), text(row, "target_engine"), text(row, "engine")]
            .into_iter().find(|engine| !engine.is_empty()).unwrap_or("")
    }).unwrap_or("");
    let engine = if text(&body, "engine").is_empty() { saved_engine } else { text(&body, "engine") };
    if engine.is_empty() {
        return Err(error(StatusCode::BAD_REQUEST, "Credential engine is required"));
    }
    if !saved_engine.is_empty() && saved_engine != "api" && saved_engine != engine {
        return Err(error(StatusCode::CONFLICT, "Credential engine does not match the selected engine"));
    }
    let secret = if supplied_secret.is_empty() {
        saved.as_ref().map(|row| text(row, "secret")).unwrap_or("")
    } else { supplied_secret };
    let row = saved.clone().unwrap_or_else(|| json!({}));
    let environment = if text(&body, "backend") == "container" { "container" } else { "local" };
    let anthropic = crate::agent_runtime::descriptors::find(engine)
        .is_some_and(|descriptor| descriptor.public()["models"]["endpoint_protocol"] == "anthropic");
    let pi_confirmed = engine == "pi";
    let source = if anthropic { "anthropic_endpoint" } else if pi_confirmed { "pi_cli_rpc" } else { "endpoint" };
    let stored_source = if anthropic { "http_anthropic_models" } else if pi_confirmed { "pi_cli_rpc" } else { "http_openai_models" };
    let runtime = runtime_key(&body, engine);
    let key = if environment == "container" { format!("http:container:{id}|{runtime}") } else { format!("http:{id}|{runtime}") };
    let old = if draft { None } else { state.store.get("model_catalogs", &key).map_err(storage)? };
    let mut failure_code = "catalog_request_failed";
    let result: Result<Vec<Value>, String> = async {
        let mut url = reqwest::Url::parse(base_url).map_err(|_| "Invalid HTTP provider URL")?;
        if !matches!(url.scheme(), "http" | "https") {
            return Err("Custom endpoint needs an HTTP(S) Base URL".into());
        }
        let path = url.path().trim_end_matches('/');
        if !path.ends_with("/models") {
            url.set_path(&if anthropic && !path.ends_with("/v1") {
                format!("{path}/v1/models")
            } else { format!("{path}/models") });
        }
        let mut request = catalog_client()?.get(url);
        if anthropic {
            request = request.header("anthropic-version", "2023-06-01");
            if !secret.is_empty() { request = request.header("x-api-key", secret); }
        } else if !secret.is_empty() {
            request = request.bearer_auth(secret);
        }
        let payload = response_json(
            request
                .send()
                .await
                .map_err(|_| "Could not connect to selected HTTP provider")?,
        )
        .await?;
        if payload.get("error").is_some_and(|e| !e.is_null()) {
            return Err("HTTP provider returned an API error".into());
        }
        let rows = if anthropic { payload["data"].as_array() }
            else { payload["models"].as_array().filter(|rows| !rows.is_empty())
                .or_else(|| payload["data"].as_array()) }
            .ok_or("HTTP provider returned an invalid model list")?;
        let mut models = Vec::new();
        for item in rows.iter().filter(|item| item.is_object()) {
            if anthropic {
                let model_id = text(item, "id");
                if !model_id.is_empty() && !models.iter().any(|model: &Value| model["id"] == model_id) {
                    let label = text(item, "display_name");
                    models.push(json!({"id":model_id,"label":if label.is_empty() { model_id } else { label }}));
                }
                continue;
            }
            let model_id = [text(item, "slug"), text(item, "id")]
                .into_iter().find(|value| !value.is_empty()).unwrap_or("");
            if model_id.is_empty() || models.iter().any(|model: &Value| model["id"] == model_id) { continue; }
            let label = [text(item, "display_name"), text(item, "displayName")]
                .into_iter().find(|value| !value.is_empty()).unwrap_or(model_id);
            let raw_levels = item.get("supported_reasoning_levels")
                .or_else(|| item.get("supportedReasoningEfforts"))
                .and_then(Value::as_array);
            let mut levels = Vec::<String>::new();
            for value in raw_levels.into_iter().flatten() {
                let value = value.as_str().or_else(|| value["effort"].as_str())
                    .or_else(|| value["reasoningEffort"].as_str()).unwrap_or("").trim();
                if value.is_empty() || value == "default" || value.len() > 64
                    || !value.chars().next().is_some_and(|c| c.is_ascii_alphanumeric())
                    || !value.chars().all(|c| c.is_ascii_alphanumeric() || "_.-".contains(c))
                    || levels.iter().any(|level| level == value) { continue; }
                levels.push(value.to_owned());
            }
            let requested_default = item["default_reasoning_level"].as_str()
                .or_else(|| item["defaultReasoningEffort"].as_str()).unwrap_or("");
            let default = if levels.iter().any(|level| level == requested_default) { requested_default } else { "" };
            let reasoning_supported = !levels.is_empty() || item["reasoning"].as_bool() == Some(true);
            let mut model = json!({"id":model_id,"label":label,"reasoning":{
                "supported":reasoning_supported,"levels":levels,"default":default,
                "kind":"effort","source":"model_metadata"}});
            if let Some(input) = extract_input_modalities(item) { model["input"] = input; }
            if let Some(context_window) = extract_positive_u64(
                item,
                &["context_window", "context_length", "contextWindow", "max_context_length", "max_model_len", "input_token_limit"],
                &["/top_provider/context_length", "/limits/context"],
            ) {
                model["context_window"] = json!(context_window);
            }
            if let Some(max_output_tokens) = extract_positive_u64(
                item,
                &["max_output_tokens", "max_completion_tokens", "max_tokens", "maxTokens", "output_token_limit"],
                &["/top_provider/max_completion_tokens", "/limits/output"],
            ) {
                model["max_output_tokens"] = json!(max_output_tokens);
            }
            models.push(model);
        }
        if models.is_empty() {
            return Err("HTTP provider returned an empty model catalog".into());
        }
        if pi_confirmed {
            confirm_pi_endpoint_models(&state, &id, &row, base_url, secret, environment, &runtime, &mut models)
                .await.map_err(|error| { failure_code = error.code; error.message })?;
        }
        Ok(models)
    }
    .await;
    if provider::matching_account(&state.store, &id).map_err(storage)? != saved {
        return Err(error(
            StatusCode::CONFLICT,
            "Credential configuration changed during catalog refresh",
        ));
    }
    let now = chrono::Utc::now().timestamp_millis() as f64 / 1000.0;
    let ok = result.is_ok();
    let (models, detail) = match result {
        Ok(models) => {
            let detail = if pi_confirmed {
                format!("从端点读到 {} 个模型，已由 Pi RPC 确认思考选项", models.len())
            } else { "Models read from selected endpoint".to_owned() };
            (models, detail)
        },
        Err(detail) => (Vec::new(), detail),
    };
    let catalog = if draft {
        json!({"discovered_models":models,"draft":true})
    } else {
        let catalog = json!({"credential_id":id,"engine":engine,"environment":environment,"runtime_instance":runtime,"configured_models":row["models"],"discovered_models":models,"verified_models":old.as_ref().map(|v|v["verified_models"].clone()).unwrap_or_else(||json!([])),"default_model":row["default_model"],"source":stored_source,"refresh_status":if ok {"fresh"} else {"failed"},"refreshed_at":if ok {json!(now)} else {old.as_ref().map(|v|v["refreshed_at"].clone()).unwrap_or(Value::Null)},"expires_at":if ok {json!(now+3600.0)} else {old.as_ref().map(|v|v["expires_at"].clone()).unwrap_or(Value::Null)},"last_error":if ok {""} else {&detail},"error_code":if ok {""} else {failure_code}});
        state.store.put("model_catalogs", &key, &catalog).map_err(storage)?;
        catalog
    };
    let ids: Vec<_> = models.iter().filter_map(|v| v["id"].as_str()).collect();
    Ok(Json(
        json!({"ok":ok,"detail":detail,"source":source,"models":ids,"credential_id":id,"catalog":catalog,"error_code":if ok {""} else {failure_code}}),
    ))
}

async fn test_draft(State(state): State<Arc<AppState>>, Json(body): Json<Value>) -> ApiResult {
    if !body.is_object() { return Err(error(StatusCode::UNPROCESSABLE_ENTITY,"Expected an object")); }
    let engine = text(&body,"engine");
    let model = text(&body,"model");
    let descriptor = crate::agent_runtime::descriptors::find(engine)
        .ok_or_else(||error(StatusCode::UNPROCESSABLE_ENTITY,"Unknown draft engine"))?;
    if descriptor.identity.support_status != "supported" { return Err(error(StatusCode::UNPROCESSABLE_ENTITY,"Selected engine is temporarily unavailable")); }
    if model.is_empty() || text(&body,"secret").is_empty() && text(&body,"codex_auth_json").is_empty() {
        return Err(error(StatusCode::UNPROCESSABLE_ENTITY,"Draft engine, model and credential material are required"));
    }
    if !text(&body,"codex_auth_json").is_empty() && engine != "codex" {
        return Err(error(StatusCode::UNPROCESSABLE_ENTITY,"codex_auth_json is only valid for Codex"));
    }
    let settings = crate::settings_metadata::configuration(&state)?;
    let backend = body["backend"].as_str().filter(|value|!value.is_empty())
        .unwrap_or(if settings["worker_backend"] == "container" {"container"}else{"local"});
    if !matches!(backend,"local" | "container") { return Err(error(StatusCode::UNPROCESSABLE_ENTITY,"backend must be local or container")); }
    let root = crate::native_chat::state_dir(&state).join("_tmp/credential-drafts");
    std::fs::create_dir_all(&root).map_err(storage)?;
    let scratch = tempfile::Builder::new().prefix("muteki-credential-draft-").tempdir_in(&root).map_err(storage)?;
    let store = crate::store::Store::open(scratch.path().join("muteki.sqlite")).map_err(storage)?;
    store.put("system","state_dir",&json!(scratch.path())).map_err(storage)?;
    store.put("system","node_state_dir",&json!(crate::native_chat::node_state_dir(&state))).map_err(storage)?;
    store.put("settings","worker_templates",&settings).map_err(storage)?;
    let runtime = draft_runtime(&body, engine)?;
    if let Some(mut instance) = state.store.get(crate::runtime_api::INSTANCES,&runtime).map_err(storage)? {
        let resolved = crate::agent_runtime::credentials::env_refs_with_platform(&state,&instance["env_refs"])
            .map_err(|failure|error(StatusCode::UNPROCESSABLE_ENTITY,&failure.message))?;
        instance["env_refs"] = json!({});
        let mut env = instance["env"].as_object().cloned().unwrap_or_default();
        for (key,value) in resolved { env.insert(key,json!(value)); }
        instance["env"] = Value::Object(env);
        store.put(crate::runtime_api::INSTANCES,&runtime,&instance).map_err(storage)?;
    }
    let scratch_state = Arc::new(state.scratch_for(store));
    let mut account = json!({"account_id":"draft-test","engine":engine,"worker_engine":engine,
        "connection":body["connection"].as_str().unwrap_or("official"),"models":[model],"default_model":model});
    for key in ["secret","codex_auth_json","base_url","provider","model_capabilities"] {
        if let Some(value) = body.get(key) { account[key] = value.clone(); }
    }
    let _ = crate::compat::put_credential_account(State(scratch_state.clone()),Path("draft-test".to_owned()),Json(account)).await?;
    let mut request = body.clone(); request["backend"] = json!(backend);
    let response = crate::native_credentials::test(scratch_state.clone(),"account:draft-test",runtime,request).await;
    response
}

/// Catalog scans enumerate credentials and runtimes; they never perform inference.
pub fn spawn_catalog_refresh_loop(state: Arc<AppState>) {
    let interval = std::env::var("MUTEKI_MODEL_CATALOG_SCAN_INTERVAL").ok().and_then(|value|value.parse::<f64>().ok()).unwrap_or(300.0);
    if !interval.is_finite() || interval <= 0.0 { return; }
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(5)).await;
        loop {
            let settings = crate::settings_metadata::configuration(&state).ok();
            let environment = if settings.as_ref().is_some_and(|row|row["worker_backend"] == "container") {"container"}else{"local"};
            let query = std::collections::HashMap::from([("environment".to_owned(),environment.to_owned())]);
            if let Err((status,_)) = refresh_stale(State(state.clone()),Query(query)).await {
                tracing::warn!(status=status.as_u16(),"Background credential catalog refresh failed");
            }
            tokio::time::sleep(Duration::from_secs_f64(interval)).await;
        }
    });
}

pub(crate) async fn refresh_stale(
    State(state): State<Arc<AppState>>,
    Query(query): Query<std::collections::HashMap<String, String>>,
) -> ApiResult {
    let environment = query
        .get("environment")
        .map(String::as_str)
        .filter(|s| !s.is_empty())
        .unwrap_or("local");
    if !["local", "container"].contains(&environment) {
        return Err(error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "environment must be local or container",
        ));
    }
    if environment == "container" {
        let now = chrono::Utc::now().timestamp_millis() as f64 / 1000.0;
        let mut requested = 0;
        let mut failed = 0;
        let mut results = Vec::new();
        let instances = crate::runtime_api::list(State(state.clone()), Query(Default::default()))
            .await.map_err(|_| error(StatusCode::INTERNAL_SERVER_ERROR, "Could not read runtime instances"))?.0;
        for account in state.store.list("credential_accounts").map_err(storage)? {
            if account["present"] == false { continue; }
            let Some(account_id) = account["account_id"].as_str().or_else(|| account["id"].as_str()) else { continue };
            let engine = account["worker_engine"].as_str().filter(|v| !v.is_empty())
                .or_else(|| account["engine"].as_str()).unwrap_or("");
            let id = format!("account:{account_id}");
            let custom = !text(&account, "base_url").is_empty()
                || text(&account, "connection") == "custom_endpoint";
            let mut runtimes: Vec<(bool, String)> = instances["instances"].as_array().into_iter().flatten()
                .filter(|instance| instance["engine"] == engine)
                .filter(|instance| instance["key"].as_str().is_some_and(|key| {
                    let adapter = key.rsplit_once(':').map(|(adapter, _)| adapter).unwrap_or(key);
                    crate::agent_runtime::descriptors::for_adapter(adapter).is_some()
                }))
                .filter_map(|instance| instance["key"].as_str().map(|key| (instance["configured"] == true, key.to_owned())))
                .collect();
            runtimes.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
            runtimes.dedup_by(|a, b| a.1 == b.1);
            if runtimes.is_empty() {
                runtimes.push((false, crate::runtime_api::default_runtime_key(engine)
                    .unwrap_or_else(|| format!("cli.{engine}:default"))));
            }
            for (_, runtime) in runtimes {
                let key = if custom { format!("http:container:{id}|{runtime}") }
                    else { format!("native:container:{id}|{runtime}") };
                let existing = state.store.get("model_catalogs", &key).map_err(storage)?;
                if existing.as_ref().is_some_and(|catalog| catalog["refresh_status"] == "fresh"
                    && catalog["expires_at"].as_f64().unwrap_or(0.0) > now) { continue; }
                requested += 1;
                let result = if custom {
                    refresh_models(State(state.clone()), Path(id.clone()),
                        Some(Json(json!({"backend":"container","runtime_instance":runtime,"engine":engine}))))
                        .await
                } else {
                    crate::native_credentials::refresh_container(state.clone(), &id, runtime.clone()).await
                };
                let (ok, detail) = match result {
                    Ok(Json(row)) => (row["ok"] == true, text(&row, "detail").to_owned()),
                    Err((_, Json(row))) => (false, text(&row, "detail").to_owned()),
                };
                if !ok { failed += 1; }
                results.push(json!({"credential_id":id,"runtime_instance":runtime,"ok":ok,"detail":detail}));
            }
        }
        return Ok(Json(json!({"environment":environment,"requested":requested,"failed":failed,"results":results})));
    }
    let credentials = provider::public_catalog(&state.store).map_err(storage)?;
    let now = chrono::Utc::now().timestamp_millis() as f64 / 1000.0;
    let mut requested = 0;
    let mut failed = 0;
    let mut results = Vec::new();
    for credential in credentials {
        let id = text(&credential, "id");
        if id.starts_with("system:") {
            continue;
        }
        let engine = text(&credential, "engine");
        let runtime = runtime_key(&json!({}), engine);
        let catalog = &credential["model_catalogs"][runtime.as_str()];
        if catalog["refresh_status"] == "fresh"
            && catalog["expires_at"].as_f64().unwrap_or(0.0) > now
        {
            continue;
        }
        requested += 1;
        let result = refresh_models(
            State(state.clone()),
            Path(id.to_owned()),
            Some(Json(json!({"runtime_instance":runtime}))),
        )
        .await;
        let (ok, detail) = match result {
            Ok(Json(result)) => (result["ok"] == true, text(&result, "detail").to_owned()),
            Err((_, Json(result))) => (false, text(&result, "detail").to_owned()),
        };
        if !ok {
            failed += 1;
        }
        results.push(json!({"credential_id":id,"ok":ok,"detail":detail}));
    }
    for (id, runtime) in crate::native_credentials::stale_targets(&state.store).await {
        requested += 1;
        let (ok, detail) = match refresh_models(State(state.clone()), Path(id.clone()),
            Some(Json(json!({"runtime_instance":runtime})))).await {
            Ok(Json(result)) => (result["ok"] == true, text(&result, "detail").to_owned()),
            Err((_, Json(result))) => (false, text(&result, "detail").to_owned()),
        };
        if !ok {
            failed += 1;
        }
        results.push(json!({"credential_id":id,"runtime_instance":runtime,"ok":ok,"detail":detail}));
    }
    Ok(Json(
        json!({"environment":environment,"requested":requested,"failed":failed,"results":results}),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn endpoint_fixture() -> (String, tokio::task::JoinHandle<()>) {
        let app = Router::new().route("/v1/models", axum::routing::get(|| async {
            Json(json!({"data":[{"id":"endpoint-a"},{"id":"endpoint-b"},{"id":"endpoint-a"}]}))
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}/v1", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap(); });
        (base, server)
    }

    #[tokio::test]
    async fn custom_endpoint_inventory_wins_for_all_engine_runtimes() {
        let (base, server) = endpoint_fixture().await;
        let root = tempfile::tempdir().unwrap();
        let store = crate::store::Store::open(":memory:").unwrap();
        store.put("system", "state_dir", &json!(root.path())).unwrap();
        let state = Arc::new(AppState::new(store));
        #[cfg(unix)]
        let pi_binary = {
            use std::os::unix::fs::PermissionsExt;
            let path = root.path().join("fixture-pi");
            std::fs::write(&path, r##"#!/usr/bin/env node
const fs = require('node:fs');
const readline = require('node:readline');
const config = JSON.parse(fs.readFileSync(process.env.PI_CODING_AGENT_DIR + '/models.json', 'utf8'));
readline.createInterface({ input: process.stdin }).on('line', line => {
  const request = JSON.parse(line);
  const models = config.providers.muteki.models.map(model => ({ ...model, provider: 'muteki' }));
  const data = request.type === 'get_state' ? { model: models[0] } : { models };
  process.stdout.write(JSON.stringify({ type: 'response', id: request.id, success: true, data }) + '\n');
});
"##).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
            path
        };
        for engine in crate::agent_runtime::descriptors::all() {
            let id = format!("account:{}", engine.engine());
            state.store.put("credential_accounts", engine.engine(), &json!({
                "account_id":engine.engine(),"engine":engine.engine(),"worker_engine":engine.engine(),
                "base_url":base,"secret":"synthetic","connection":"custom_endpoint",
                "models":["muteki/old-model"],"default_model":"muteki/old-model"
            })).unwrap();
            for adapter in &engine.adapters {
                let runtime = format!("{}:default", adapter.adapter_id);
                if engine.engine() == "pi" {
                    #[cfg(unix)]
                    state.store.put(crate::runtime_api::INSTANCES, &runtime,
                        &json!({"binary_path":pi_binary})).unwrap();
                    #[cfg(not(unix))]
                    continue;
                }
                for backend in ["local", "container"] {
                    if engine.engine() == "pi" && backend == "container" { continue; }
                    for draft in [false, true] {
                        let mut body = json!({"engine":engine.engine(),"runtime_instance":runtime,"backend":backend});
                        if draft {
                            body["base_url"] = json!(base);
                            body["connection"] = json!("custom_endpoint");
                            body["secret"] = json!("synthetic-draft");
                        }
                        let Json(result) = refresh_models(State(state.clone()), Path(id.clone()), Some(Json(body)))
                            .await.unwrap();
                        assert_eq!(result["ok"], true, "{} {backend} draft={draft}: {result}", runtime);
                        assert_eq!(result["models"], json!(["endpoint-a", "endpoint-b"]), "{runtime}");
                        assert_eq!(result["catalog"]["draft"] == true, draft);
                        if !draft {
                            assert_eq!(result["catalog"]["environment"], backend);
                            assert_eq!(result["catalog"]["runtime_instance"], runtime);
                        }
                    }
                }
            }
        }
        server.abort();
    }

    #[tokio::test]
    async fn endpoint_failure_does_not_report_configured_models_as_discovered() {
        let app = Router::new().route("/v1/models", axum::routing::get(|| async { StatusCode::UNAUTHORIZED }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}/v1", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap(); });
        let store = crate::store::Store::open(":memory:").unwrap();
        store.put("credential_accounts", "pi", &json!({"account_id":"pi","engine":"pi",
            "base_url":base,"secret":"synthetic","models":["old-model"],"default_model":"old-model"})).unwrap();
        let state = Arc::new(AppState::new(store));
        let Json(result) = refresh_models(State(state), Path("account:pi".into()),
            Some(Json(json!({"runtime_instance":"pi.rpc:default"})))).await.unwrap();
        assert_eq!(result["ok"], false);
        assert_eq!(result["models"], json!([]));
        assert!(text(&result, "detail").contains("401"));
        server.abort();
    }

    #[tokio::test]
    async fn switching_endpoint_to_official_refreshes_the_draft_connection() {
        let store = crate::store::Store::open(":memory:").unwrap();
        store.put("credential_accounts", "claude", &json!({"account_id":"claude","engine":"claude",
            "base_url":"http://127.0.0.1:1","secret":"synthetic","models":["old-model"]})).unwrap();
        let state = Arc::new(AppState::new(store));
        let Json(result) = refresh_models(State(state), Path("account:claude".into()),
            Some(Json(json!({"runtime_instance":"claude.sdk:default","connection":"official","base_url":""})))).await.unwrap();
        assert_eq!(result["ok"], true);
        assert_eq!(result["catalog"]["draft"], true);
        assert_eq!(result["source"], "claude_reference_catalog");
    }

    #[test]
    fn draft_without_a_runtime_uses_the_engine_default() {
        let body = json!({"engine":"pi","model":"m","secret":"s"});
        assert_eq!(draft_runtime(&body, "pi").unwrap(), "pi.rpc:default");
    }

    #[test]
    fn draft_keeps_a_named_native_runtime() {
        let body = json!({"runtime_instance":"cli.pi:default"});
        assert_eq!(draft_runtime(&body, "pi").unwrap(), "cli.pi:default");
    }

    #[test]
    fn draft_http_runtime_does_not_replace_the_native_engine() {
        let body = json!({"runtime_instance":"pi-http:text"});
        assert_eq!(draft_runtime(&body, "pi").unwrap(), "pi.rpc:default");
    }

    #[test]
    fn extract_positive_u64_reads_direct_and_nested_model_limits() {
        assert_eq!(
            extract_positive_u64(
                &json!({"context_length": 262_144, "max_output_tokens": 65_536}),
                &["context_window", "context_length"],
                &["/top_provider/context_length"],
            ),
            Some(262_144)
        );
        assert_eq!(
            extract_positive_u64(
                &json!({"top_provider": {"context_length": 1_048_576, "max_completion_tokens": 32_768}}),
                &["context_window", "context_length"],
                &["/top_provider/context_length"],
            ),
            Some(1_048_576)
        );
        assert_eq!(
            extract_positive_u64(
                &json!({"context_length": 0, "top_provider": {"context_length": -1}}),
                &["context_length"],
                &["/top_provider/context_length"],
            ),
            None
        );
        assert_eq!(
            extract_positive_u64(
                &json!({"context_length": "128000"}),
                &["context_length"],
                &[],
            ),
            None
        );
    }
}

/// Runner-only discovery: fixed container backend and no user-supplied binary/argv/path.
pub(crate) async fn runner_draft_catalog(state:&AppState,id:&str,engine:&str,provider:&str)->ApiResult{
    let body=json!({"backend":"container","provider":provider});
    let result=match engine {
        "pi"=>draft_native_catalog::refresh_pi(state,id,&body).await,
        "codex"|"claude"|"cursor"|"grok"|"opencode"|"kimi"|"omp"|"droid"=>draft_native_catalog::refresh_cli(state,id,&body,engine).await,
        _=>return Err(error(StatusCode::UNPROCESSABLE_ENTITY,"Unsupported container catalog engine")),
    }?;
    Ok(Json(result))
}
