use crate::AppState;
use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::StatusCode,
    routing::get,
};
use chrono::{Offset, TimeZone, Timelike};
use serde_json::{Value, json};
use std::sync::Arc;

type ApiResult = Result<Json<Value>, (StatusCode, Json<Value>)>;
fn internal(_: impl std::fmt::Display) -> (StatusCode, Json<Value>) {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({"detail":"Storage error"})),
    )
}
fn invalid(message: &str) -> (StatusCode, Json<Value>) {
    (
        StatusCode::UNPROCESSABLE_ENTITY,
        Json(json!({"detail":message})),
    )
}
pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/health", get(health))
        .route("/api/receipts/{id}", get(command_receipt))
        .route("/api/settings/ui", get(get_ui).put(put_ui))
        .route(
            "/api/conversation/preferences",
            get(get_conversation_preferences).put(put_conversation_preferences),
        )
        .route("/api/workspace-kinds", get(workspace_kinds))
        .route("/api/runs", get(runs))
        .route("/api/competitions", get(competitions))
        .route(
            "/api/settings/credential-accounts",
            get(credential_accounts),
        )
        .route(
            "/api/settings/credential-accounts/{id}",
            axum::routing::put(put_credential_account).delete(delete_credential_account),
        )
        .route("/api/settings/credentials", get(credentials))
        .route(
            "/api/settings/model-endpoints",
            get(model_endpoints).post(put_model_endpoint),
        )
        .route("/api/usage", get(usage))
        .route(
            "/api/settings/model-endpoints/{id}",
            axum::routing::delete(delete_model_endpoint),
        )
        .route("/api/usage/summary", get(usage_summary))
        .route(
            "/api/usage/import-history",
            axum::routing::post(import_usage_history),
        )
        .route("/api/usage/pricing", get(pricing_catalog))
        .route(
            "/api/usage/pricing/resolve",
            axum::routing::post(resolve_prices),
        )
        .route(
            "/api/usage/pricing/override",
            axum::routing::put(put_price_override).delete(delete_price_override),
        )
        .route(
            "/api/usage/pricing/reference",
            axum::routing::put(put_price_reference).delete(delete_price_reference),
        )
}
async fn health(State(state): State<Arc<AppState>>) -> Json<Value> {
    use sha2::{Digest, Sha256};
    let mut body = health_body();
    let root = crate::native_chat::state_dir(&state);
    body["storage_identity"] = json!(format!("{:x}", Sha256::digest(root.to_string_lossy().as_bytes())));
    Json(body)
}
pub(crate) fn health_body() -> Value {
    json!({"app":"muteki-rust","version":env!("CARGO_PKG_VERSION"),"features":{"ui.preferences":1,"settings.appearance":1,"settings.agents":1,"settings.shortcuts":1,"settings.notifications":1,"settings.access":1,"settings.archives":1,"settings.chat":1,"settings.import":1,"settings.agent-extensions":1,"settings.capabilities":1,"settings.control":1,"settings.extensions":1,"settings.operations":1,"settings.network-proxy":1,"settings.execution-guardrail":1}})
}
async fn command_receipt(State(state): State<Arc<AppState>>, Path(id): Path<String>) -> ApiResult {
    match state.store.command_receipt(&id).map_err(internal)? {
        Some(receipt) => Ok(Json(json!({"receipt":receipt}))),
        None => Err((
            StatusCode::NOT_FOUND,
            Json(
                json!({"error":{"code":"receipt.not_found","message":"Command receipt not found"}}),
            ),
        )),
    }
}
fn conversation_preferences_body(state: &AppState, thread_id: &str) -> ApiResult {
    let mut body = crate::quota_resume::preferences(state).map_err(crate::control::storage)?;
    body["auto_resume_on_quota_reset_supported"] = json!(true);
    if !thread_id.is_empty() {
        body["thread_id"] = json!(thread_id);
        let mode = state.store.get("conversation_threads", thread_id).map_err(crate::control::storage)?
            .and_then(|thread| thread["runtime"]["interaction_mode"].as_str().map(str::to_owned))
            .unwrap_or_else(|| "default".to_owned());
        body["interaction_mode"] = json!(mode);
    }
    Ok(Json(body))
}
fn check_preference_thread(
    state: &AppState,
    thread_id: &str,
) -> Result<(), (StatusCode, Json<Value>)> {
    if !thread_id.is_empty()
        && state
            .store
            .get("conversation_threads", thread_id)
            .map_err(crate::control::storage)?
            .is_none()
    {
        return Err((
            StatusCode::NOT_FOUND,
            Json(json!({"error":{
                "code":"conversation.thread.not_found", "message":"Thread not found"
            }})),
        ));
    }
    Ok(())
}
pub(crate) async fn get_conversation_preferences(
    State(state): State<Arc<AppState>>,
    Query(query): Query<std::collections::HashMap<String, String>>,
) -> ApiResult {
    let thread_id = query
        .get("thread_id")
        .map(String::as_str)
        .unwrap_or("")
        .trim();
    check_preference_thread(&state, thread_id)?;
    conversation_preferences_body(&state, thread_id)
}
pub(crate) async fn put_conversation_preferences(
    State(state): State<Arc<AppState>>,
    Query(query): Query<std::collections::HashMap<String, String>>,
    Json(body): Json<Value>,
) -> Result<(StatusCode, Json<Value>), (StatusCode, Json<Value>)> {
    let object = body
        .as_object()
        .ok_or_else(|| invalid("expected an object"))?;
    if object.keys().any(|key| {
        ![
            "version",
            "auto_resume_on_quota_reset",
            "thread_id",
            "interaction_mode",
        ]
        .contains(&key.as_str())
    }) {
        return Err(invalid("Unknown conversation preference"));
    }
    let thread_id = body.get("thread_id");
    if thread_id.is_some_and(|value| !value.is_null() && !value.is_string()) {
        return Err(invalid("thread_id must be a string"));
    }
    let thread_id = body
        .get("thread_id")
        .and_then(Value::as_str)
        .filter(|id| !id.trim().is_empty())
        .or_else(|| query.get("thread_id").map(String::as_str))
        .unwrap_or("")
        .trim();
    check_preference_thread(&state, thread_id)?;
    let auto_resume = match body.get("auto_resume_on_quota_reset") {
        Some(value) => Some(
            value
                .as_bool()
                .ok_or_else(|| invalid("auto_resume_on_quota_reset must be a boolean"))?,
        ),
        None => None,
    };
    if auto_resume.is_none() && body.get("interaction_mode").is_none() {
        return Err(invalid("auto_resume_on_quota_reset must be a boolean"));
    }
    if let Some(mode) = body.get("interaction_mode") {
        if thread_id.is_empty() {
            return Err(invalid("interaction_mode requires thread_id"));
        }
        let mode = mode.as_str().ok_or_else(|| crate::chat::fail_code(
            StatusCode::BAD_REQUEST,"conversation.interaction_mode_invalid",
            "interaction_mode must be default or plan"))?;
        let _guard = state.chat.active.lock().await;
        let mut thread = crate::chat::load(&state, thread_id)?;
        crate::chat::require_interaction_mode(&state, &thread["runtime"], mode)?;
        thread["runtime"]["interaction_mode"] = json!(mode);
        crate::chat::save(&state, thread_id, &mut thread)?;
    }
    let expected = match body.get("version").filter(|_| body.get("interaction_mode").is_none()) {
        Some(version) => Some(
            version
                .as_u64()
                .filter(|v| *v < 9_007_199_254_740_991)
                .ok_or_else(|| invalid("version must be a non-negative integer"))?,
        ),
        None => None,
    };
    if let Some(enabled) = auto_resume {
        if crate::quota_resume::write_preferences(&state, enabled,
            if body.get("interaction_mode").is_some() { None } else { expected })
            .map_err(crate::control::storage)?
            .is_none()
        {
            return Ok((
                StatusCode::CONFLICT,
                conversation_preferences_body(&state, thread_id)?,
            ));
        }
    }
    Ok((
        StatusCode::OK,
        conversation_preferences_body(&state, thread_id)?,
    ))
}
pub(crate) async fn get_ui(State(state): State<Arc<AppState>>) -> ApiResult {
    Ok(Json(
        state
            .store
            .get("settings", "ui")
            .map_err(internal)?
            .unwrap_or_else(|| json!({"version":0,"values":{}})),
    ))
}
pub(crate) async fn put_ui(
    State(state): State<Arc<AppState>>,
    Json(body): Json<Value>,
) -> Result<(StatusCode, Json<Value>), (StatusCode, Json<Value>)> {
    let obj = body
        .as_object()
        .ok_or_else(|| invalid("expected an object"))?;
    if obj.len() != 2 {
        return Err(invalid("expected version and values"));
    }
    let version = body
        .get("version")
        .and_then(Value::as_u64)
        .filter(|v| *v < 9_007_199_254_740_991)
        .ok_or_else(|| invalid("invalid version"))?;
    let values = body
        .get("values")
        .filter(|v| valid_values(v))
        .ok_or_else(|| invalid("invalid preference values"))?;
    match state
        .store
        .compare_and_put("settings", "ui", version, values.clone())
        .map_err(internal)?
    {
        Ok(saved) => Ok((StatusCode::OK, Json(saved))),
        Err(current) => Ok((StatusCode::CONFLICT, Json(current))),
    }
}
/// A host folder is absolute, `~`, or under `~`; Windows drive and UNC roots count as absolute.
pub(crate) fn host_folder(path: &str) -> bool {
    let path = path.trim();
    if path.is_empty() || path.chars().count() > 1024 || path.contains('\0') {
        return false;
    }
    let drive = path.len() > 2 && path.as_bytes()[0].is_ascii_alphabetic() && path.as_bytes()[1] == b':' && matches!(path.as_bytes()[2], b'/' | b'\\');
    path == "~" || path.starts_with("~/") || path.starts_with('/') || path.starts_with("\\\\") || drive
}
fn valid_values(values: &Value) -> bool {
    let Some(values) = values.as_object() else {
        return false;
    };
    values.iter().all(|(key, value)| {
        let member = |choices: &[&str]| value.as_str().is_some_and(|s| choices.contains(&s));
        match key.as_str() {
            "language" => value.as_str().is_some_and(|value| !value.is_empty()),
            "theme" => member(&["system", "light", "dark"]),
            "sendKey" => member(&["enter", "mod-enter"]),
            "runningSend" => member(&["queue", "steer"]),
            "defaultAccessMode" => {
                member(&["", "supervised", "auto-accept-edits", "auto", "full-access"])
            }
            "diffView" => member(&["unified", "split"]),
            "diffWrap" | "diffCollapseUnchanged" | "sidebarModelIcon" | "liveCommandPanelDisabled" | "inactiveFrameThrottleDisabled" | "scrollToLatestOnSend" | "shortenLongCommands" | "skipDeleteConfirmation" | "providerUpdateChecksDisabled" => value.is_boolean(),
            "newProjectDir" | "projectStartCustom" => value.as_str().is_some_and(|s| s.is_empty() || host_folder(s)),
            "projectStart" => member(&["home", "new_project_dir", "custom"]),
            "titleModel" => value.is_null()
                || value.as_object().is_some_and(|o| {
                    o.keys().all(|k| matches!(k.as_str(), "credentialId" | "modelId" | "runtimeKey"))
                        && ["credentialId", "modelId"].iter().all(|k| {
                            o.get(*k).and_then(Value::as_str).is_some_and(|s| !s.is_empty() && s.chars().count() <= 512)
                        })
                        && o.get("runtimeKey").is_none_or(|v| v.as_str().is_some_and(|s| s.chars().count() <= 512))
                }),
            "favoriteModels" => value.as_array().is_some_and(|a| {
                a.len() <= 512
                    && a.iter().all(|item| {
                        item.as_object().is_some_and(|o| {
                            o.len() == 2
                                && ["credentialId", "modelId"].iter().all(|k| {
                                    o.get(*k).and_then(Value::as_str).is_some_and(|s| !s.is_empty() && s.chars().count() <= 512)
                                })
                        })
                    })
            }),
            "engineLogoColor" => member(&["brand", "mono", "custom"]),
            "engineLogoCustomColor" => value.as_str().is_some_and(|s| {
                s.len() == 7 && s.starts_with('#') && s[1..].bytes().all(|b| b.is_ascii_hexdigit())
            }),
            "readingFontScale" => member(&["sm", "md", "lg"]),
            "readingDensity" => member(&["comfortable", "compact"]),
            "readingContentWidth" => member(&["narrow", "default", "wide"]),
            "hiddenModels" => value.as_array().is_some_and(|a| {
                a.len() <= 512
                    && a.iter()
                        .all(|v| v.as_str().is_some_and(|s| s.chars().count() <= 2048))
            }),
            "defaultModel" => {
                value.is_null()
                    || value.as_object().is_some_and(|o| {
                        o.len() == 2
                            && ["credentialId", "modelId"].iter().all(|k| {
                                o.get(*k)
                                    .and_then(Value::as_str)
                                    .is_some_and(|s| !s.is_empty() && s.chars().count() <= 512)
                            })
                    })
            }
            "accent" => value.as_object().is_some_and(|o| {
                (o.len() == 2 || (o.len() == 3 && o.get("kind").and_then(Value::as_str) == Some("custom") && o.get("hex").and_then(Value::as_str).is_some_and(|s| s.len() == 7 && s.starts_with('#') && s[1..].bytes().all(|b| b.is_ascii_hexdigit()))))
                    && match o.get("kind").and_then(Value::as_str) {
                        Some("preset") => o
                            .get("id")
                            .and_then(Value::as_str)
                            .is_some_and(|s| ["azure", "violet", "teal", "ember"].contains(&s)),
                        Some("custom") => o
                            .get("hue")
                            .and_then(Value::as_f64)
                            .is_some_and(|h| (0.0..360.0).contains(&h)),
                        _ => false,
                    }
            }),
            _ => false,
        }
    })
}
async fn workspace_kinds(State(state): State<Arc<AppState>>) -> ApiResult {
    let records = crate::operations_api::module_records(&state).map_err(internal)?;
    let kinds: Vec<Value> = records.into_iter()
        .filter(|record| !matches!(record["state"].as_str(), Some("disabled" | "unavailable")))
        .map(|record| {
            let descriptor = &record["descriptor"];
            let ui = &descriptor["ui_contributions"];
            json!({"schema_version":1,"id":descriptor["workspace_kind"],"module_id":descriptor["id"],
                "title":ui["title"],"description":ui["description"],"icon":ui["icon"],
                "route":ui["route"],"create_entry":ui["create_entry"],"aggregate_type":ui["aggregate_type"],
                "state":record["state"],"task_kinds":descriptor["task_kinds"]})
        }).collect();
    Ok(Json(json!(kinds)))
}
#[derive(serde::Deserialize, Default)]
struct RunsQuery {
    #[serde(default)]
    archived: u8,
}
async fn runs(State(state): State<Arc<AppState>>, Query(query): Query<RunsQuery>) -> ApiResult {
    let runs = crate::runs::list_runs(&state, query.archived != 0)
        .map_err(|e| internal(rusqlite::Error::ToSqlConversionFailure(e.into())))?;
    Ok(Json(json!({"runs": runs})))
}
async fn competitions(State(state): State<Arc<AppState>>) -> ApiResult {
    let competitions = state.store.list("competitions").map_err(internal)?;
    Ok(Json(json!(
        competitions
            .into_iter()
            .filter(|record| record["archived"] != true)
            .collect::<Vec<_>>()
    )))
}

pub(crate) fn public_account(value: Value) -> Value {
    let present = ["secret", "codex_auth_json", "imported_home"]
        .iter()
        .any(|key| {
            value
                .get(*key)
                .and_then(Value::as_str)
                .is_some_and(|s| !s.is_empty())
        });
    let mut result = serde_json::Map::new();
    for key in [
        "id",
        "label",
        "account_id",
        "engine",
        "worker_engine",
        "connection",
        "base_url",
        "provider",
        "default_model",
        "models",
        "model_capabilities",
        "revision",
        "updated_at",
        "status",
        "usage",
    ] {
        if let Some(v) = value.get(key) {
            result.insert(key.into(), v.clone());
        }
    }
    if result.get("connection").and_then(Value::as_str) == Some("custom") {
        result.insert("connection".into(), json!("custom_endpoint"));
    }
    result.insert("present".into(), json!(present));
    result.insert("has_secret".into(), json!(present));
    let filled = |key: &str| {
        value
            .get(key)
            .and_then(Value::as_str)
            .is_some_and(|v| !v.is_empty())
    };
    result.insert(
        "mode".into(),
        json!(if filled("codex_auth_json") {
            "auth_json"
        } else if filled("imported_home") {
            "auth_home"
        } else {
            "api_key"
        }),
    );
    result.insert("credential_format".into(), result["mode"].clone());
    result.insert("writable_state".into(), json!(true));
    result.insert("details".into(), json!({}));
    Value::Object(result)
}
pub(crate) async fn credential_accounts(State(state): State<Arc<AppState>>) -> ApiResult {
    let accounts: Vec<_> = state
        .store
        .list("credential_accounts")
        .map_err(internal)?
        .into_iter()
        .map(public_account)
        .collect();
    Ok(Json(json!({"accounts": accounts})))
}
pub(crate) async fn put_credential_account(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> ApiResult {
    let input = body
        .as_object()
        .ok_or_else(|| invalid("expected an object"))?;
    if !crate::host_import::valid_account_id(&id) {
        return Err(invalid("invalid account id"));
    }
    let engine = input
        .get("worker_engine")
        .or_else(|| input.get("target_engine"))
        .or_else(|| input.get("engine"))
        .and_then(Value::as_str)
        .filter(|v| !v.trim().is_empty())
        .ok_or_else(|| invalid("worker_engine is required"))?;
    let connection = input
        .get("connection")
        .and_then(Value::as_str)
        .unwrap_or("official");
    if !["official", "custom_endpoint"].contains(&connection) {
        return Err(invalid("invalid connection"));
    }
    if connection == "custom_endpoint"
        && input
            .get("base_url")
            .and_then(Value::as_str)
            .is_none_or(|s| s.trim().is_empty())
    {
        return Err(invalid("base_url is required"));
    }
    for key in [
        "secret",
        "codex_auth_json",
        "base_url",
        "provider",
        "target_model",
    ] {
        if input
            .get(key)
            .is_some_and(|v| !v.is_null() && !v.is_string())
        {
            return Err(invalid("invalid account field"));
        }
    }
    if input
        .get("models")
        .is_some_and(|v| !v.as_array().is_some_and(|a| a.iter().all(Value::is_string)))
    {
        return Err(invalid("invalid models"));
    }
    let auth_json = input
        .get("codex_auth_json")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty() && engine == "codex");
    if auth_json.is_some_and(|raw| !serde_json::from_str::<Value>(raw).is_ok_and(|v| v.is_object()))
    {
        return Err(invalid("codex_auth_json must be a JSON object"));
    }
    let mut value = body.clone();
    let o = value.as_object_mut().expect("object checked");
    o.remove("create_only");
    o.remove("expected_revision");
    for field in ["material_generation","material_digest","imported_home","import_source"] { o.remove(field); }
    o.insert("account_id".into(), json!(id));
    o.insert("worker_engine".into(), json!(engine));
    o.insert("engine".into(), json!(engine));
    o.insert(
        "updated_at".into(),
        json!(chrono::Utc::now().timestamp_millis() as f64 / 1000.0),
    );
    o.insert("connection".into(), json!(connection));
    if let Some(model) = input.get("target_model") {
        o.insert("default_model".into(), model.clone());
    }
    if let Some(raw_capabilities) = input.get("model_capabilities") {
        if raw_capabilities.is_null() {
            o.remove("model_capabilities");
        } else {
            let normalized = crate::agent_runtime::credentials::normalize_model_capabilities(raw_capabilities)
                .map_err(|err| invalid(&err.message))?;
            o.insert("model_capabilities".into(), Value::Object(normalized));
        }
    }
    let expected = input.get("expected_revision").and_then(Value::as_str);
    match crate::account_material::save(&state,&id,value,
        input.get("create_only") == Some(&json!(true)),expected,true,None).map_err(internal)? {
        Ok(saved) => Ok(Json(json!({"ok":true,"account":public_account(saved)}))),
        Err(()) => Err((
            StatusCode::CONFLICT,
            Json(
                json!({"detail":{"code":"credential.account.conflict","message":"Account already exists or changed"}}),
            ),
        )),
    }
}
pub(crate) async fn delete_credential_account(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Query(query): Query<std::collections::HashMap<String, String>>,
) -> ApiResult {
    let _guard = state.chat.active.lock().await;
    let detach = query.get("detach_references").is_some_and(|v| v == "true");
    let changed_threads = if detach {
        state
            .store
            .credential_usage(&id)
            .map_err(internal)?
            .into_iter()
            .filter(|row| row["kind"] == "conversation")
            .filter_map(|row| row["id"].as_str().map(str::to_owned))
            .collect::<Vec<_>>()
    } else {
        Vec::new()
    };
    let result = crate::account_material::delete(&state,&id,detach).map_err(internal)?;
    match result {
        Err(references) => Err((
            StatusCode::CONFLICT,
            Json(
                json!({"detail":{"code":"credential.account.in_use","message":"Account is still referenced by configuration","references":references}}),
            ),
        )),
        Ok(None) => Err((
            StatusCode::NOT_FOUND,
            Json(
                json!({"detail":{"code":"credential.account.not_found","message":"Account not found; references unchanged"}}),
            ),
        )),
        Ok(Some(detached)) => {
            for tid in changed_threads {
                if state.store.conversation_exists(&tid).map_err(internal)? {
                    crate::chat::notify_imported(&state, &tid);
                }
            }
            let revocation = crate::runs::revoke_account_workers(&state, &id);
            Ok(Json(json!({"ok":true,"detached":detached,"worker_revocation":revocation})))
        }
    }
}
fn credential_usage_key(row: &Value) -> Option<&str> {
    row["account_id"]
        .as_str()
        .or_else(|| row["id"].as_str().filter(|id| id.starts_with("endpoint:")))
}
pub(crate) async fn credentials(
    State(state): State<Arc<AppState>>,
    Query(query): Query<std::collections::HashMap<String, String>>,
) -> ApiResult {
    let environment = query.get("environment").map(String::as_str).filter(|v| !v.is_empty()).unwrap_or("local");
    if !["local", "container"].contains(&environment) {
        return Err(invalid("environment must be local or container"));
    }
    let fresh = query.get("fresh").is_some_and(|v| v == "true" || v == "1");
    if fresh {
        let state_dir = crate::host_process::configured_state_dir(&state);
        let _ = crate::host_process::refresh_shell_env(
            state_dir.as_deref(),
            std::time::Duration::from_secs(15),
        )
        .await;
        crate::agent_runtime::host_login::invalidate();
    }
    let accounts = state.store.list("credential_accounts").map_err(internal)?;
    let catalog = crate::provider::public_catalog_for_environment(&state.store, environment).map_err(internal)?;
    let state_dir = crate::native_chat::state_dir(&state);
    let mut ids = catalog
        .iter()
        .filter_map(credential_usage_key)
        .map(str::to_owned)
        .collect::<Vec<_>>();
    ids.extend(crate::native_credentials::system_ids());
    let usage = state.store.credential_usage_index(&ids).map_err(internal)?;
    let rows = catalog
        .into_iter()
        .map(|mut row| {
            let models = row["models"]
                .as_array()
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .filter_map(|model| {
                    model["id"]
                        .as_str()
                        .or_else(|| model.as_str())
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .map(|s| json!(s))
                })
                .collect::<Vec<_>>();
            row["candidate_models"] = json!(models);
            let verified: Vec<Value> = row["model_catalogs"]
                .as_object()
                .into_iter()
                .flat_map(|c| c.values())
                .flat_map(|c| c["verified_models"].as_array().into_iter().flatten())
                .filter_map(|model| {
                    model["id"]
                        .as_str()
                        .or_else(|| model.as_str())
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .map(|s| json!(s))
                })
                .collect();
            row["models"] = json!(verified);
            row["usage"] = json!(
                credential_usage_key(&row)
                    .and_then(|id| usage.get(id))
                    .cloned()
                    .unwrap_or_default()
            );
            row["source"] = json!(if row["id"] == "system:openai" {
                "system"
            } else {
                "stored"
            });
            if row["source"] == "stored" && row["http_compatible"] == true {
                row["present"] = json!(true);
                row["status"] = json!("untested");
            }
            if let Some(account) = accounts.iter().find(|account| {
                account["account_id"] == row["account_id"] && row["account_id"].is_string()
            }) {
                let metadata = public_account(account.clone());
                for key in [
                    "base_url",
                    "revision",
                    "worker_engine",
                    "model_capabilities",
                    "details",
                    "has_secret",
                    "credential_format",
                    "mode",
                    "writable_state",
                ] {
                    if let Some(value) = metadata.get(key) {
                        row[key] = value.clone();
                    }
                }
                row["connection"] = json!(if account["base_url"]
                    .as_str()
                    .is_some_and(|v| !v.is_empty())
                {
                    "custom_endpoint"
                } else {
                    "official"
                });
                crate::native_credentials::native_account_status(
                    &state.store,
                    &state_dir,
                    account,
                    &mut row,
                    environment,
                );
            } else {
                row["connection"] = json!("custom_endpoint");
            }
            crate::native_credentials::decorate_account(&state.store, &mut row, environment);
            Ok(row)
        })
        .collect::<Result<Vec<_>, (StatusCode, Json<Value>)>>()?;
    let mut rows = rows;
    rows.extend(crate::native_credentials::system_rows(&state.store, &usage, fresh).await);
    Ok(Json(json!({"credentials":rows})))
}
fn public_endpoint(mut account: Value, id: &str) -> Value {
    account["id"] = json!(format!("endpoint:{id}"));
    account["label"] = account
        .get("label")
        .filter(|v| v.is_string())
        .cloned()
        .unwrap_or_else(|| json!(id));
    let mut value = public_account(account);
    value["status"] = json!(if value["present"] == true {
        "configured"
    } else {
        "missing"
    });
    if let Some(row) = value.as_object_mut() {
        for key in ["account_id", "connection"] {
            row.remove(key);
        }
    }
    value
}
pub(crate) async fn model_endpoints(State(state): State<Arc<AppState>>) -> ApiResult {
    let mut rows = std::collections::BTreeMap::new();
    let catalog = crate::provider::public_catalog(&state.store).map_err(internal)?;
    let accounts = state.store.list("credential_accounts").map_err(internal)?;
    let legacy_endpoints = state.store.list("model_endpoints").map_err(internal)?;
    let ids = accounts
        .iter()
        .filter_map(|row| row["account_id"].as_str())
        .map(|id| format!("endpoint:{id}"))
        .chain(
            legacy_endpoints
                .iter()
                .filter_map(|row| row["id"].as_str())
                .map(str::to_owned),
        )
        .collect::<Vec<_>>();
    let usage = state.store.credential_usage_index(&ids).map_err(internal)?;
    for account in &accounts {
        let id = account["account_id"].as_str().unwrap_or("").to_owned();
        if !id.is_empty()
            && account["base_url"].as_str().is_some_and(|v| !v.is_empty())
            && !account["codex_auth_json"]
                .as_str()
                .is_some_and(|v| !v.is_empty())
            && account["credential_format"]
                .as_str()
                .is_none_or(|v| v == "api_key")
            && account["connection"]
                .as_str()
                .is_none_or(|v| ["custom_endpoint", "custom"].contains(&v))
        {
            let mut row = public_endpoint(account.clone(), &id);
            if let Some(entry) = catalog
                .iter()
                .find(|row| row["id"] == format!("account:{id}"))
            {
                row["models"] = json!(
                    entry["models"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(|m| m["id"].as_str())
                        .collect::<Vec<_>>()
                );
            }
            row["usage"] = json!(
                usage
                    .get(&format!("endpoint:{id}"))
                    .cloned()
                    .unwrap_or_default()
            );
            rows.insert(id, row);
        }
    }
    for legacy in legacy_endpoints {
        let id = legacy["id"]
            .as_str()
            .unwrap_or("")
            .trim_start_matches("endpoint:")
            .to_owned();
        if id.is_empty() {
            continue;
        }
        let conflict = accounts
            .iter()
            .find(|account| account["account_id"] == id)
            .is_some_and(|a| !crate::store::same_http_identity(a, &legacy));
        let mut row = public_endpoint(legacy, &id);
        row["usage"] = json!(
            usage
                .get(&format!("endpoint:{id}"))
                .cloned()
                .unwrap_or_default()
        );
        if conflict {
            row["status"] = json!("conflict");
            row["detail"] = json!(
                "This legacy endpoint and account have different configurations; their identities are preserved. Rename one before editing."
            );
        }
        rows.insert(id, row);
    }
    Ok(Json(
        json!({"endpoints":rows.into_values().collect::<Vec<_>>()}),
    ))
}
pub(crate) async fn put_model_endpoint(
    State(state): State<Arc<AppState>>,
    Json(body): Json<Value>,
) -> ApiResult {
    let object = body
        .as_object()
        .ok_or_else(|| invalid("expected an object"))?;
    let field = |name: &str| {
        object
            .get(name)
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
    };
    let id = field("id");
    if id.is_empty()
        || id.len() > 128
        || id.contains(':')
        || id.contains('/')
        || field("base_url").is_empty()
        || field("model").is_empty()
    {
        return Err(invalid("id, base_url and model are required"));
    }
    let old = crate::provider::matching_account(&state.store, &format!("endpoint:{id}"))
        .map_err(internal)?;
    if field("api_key").is_empty()
        && old
            .as_ref()
            .and_then(|v| v["secret"].as_str())
            .is_none_or(|v| v.is_empty())
    {
        return Err(invalid("api_key is required for a new endpoint"));
    }
    let patch = json!({"base_url":field("base_url"),"provider":field("provider"),"secret":field("api_key"),"models":[field("model")],"default_model":field("model")});
    match state
        .store
        .save_http_endpoint(id, &patch)
        .map_err(internal)?
    {
        Ok((saved, updated)) => Ok(Json(
            json!({"ok":true,"endpoint":public_endpoint(saved,id),"updated":updated}),
        )),
        Err(()) => Err((
            StatusCode::CONFLICT,
            Json(
                json!({"detail":"Account and legacy endpoint have different configurations; rename one before editing"}),
            ),
        )),
    }
}
pub(crate) async fn usage(
    State(state): State<Arc<AppState>>,
    Query(query): Query<std::collections::HashMap<String, String>>,
) -> ApiResult {
    let parse = |key: &str, default: usize| -> Result<usize, (StatusCode, Json<Value>)> {
        query
            .get(key)
            .map(|v| {
                v.parse::<usize>()
                    .map_err(|_| invalid("invalid pagination"))
            })
            .unwrap_or(Ok(default))
    };
    let offset = parse("offset", 0)?;
    let limit = parse("limit", 100)?;
    if limit == 0 || limit > 500 {
        return Err(invalid("limit must be between 1 and 500"));
    }
    let parse_time = |key: &str| -> Result<Option<f64>, (StatusCode, Json<Value>)> {
        query
            .get(key)
            .map(|v| {
                v.parse::<f64>()
                    .ok()
                    .filter(|n| n.is_finite() && *n >= 0.0)
                    .ok_or_else(|| invalid("invalid time"))
            })
            .transpose()
    };
    let start = parse_time("start")?.unwrap_or(0.0);
    let end = parse_time("end")?;
    if end.is_some_and(|n| n < start) {
        return Err(invalid("end precedes start"));
    }
    let filters = [
        "run_id",
        "thread_id",
        "competition_id",
        "challenge_id",
        "model",
        "engine",
        "role",
        "actor_kind",
        "billing_scope",
        "workspace_kind",
        "generation",
    ];
    let mut rows = crate::usage_history::rows(&state.store, "muteki").map_err(internal)?;
    rows.retain(|r| {
        let at = r.get("at").and_then(Value::as_f64).unwrap_or(0.0);
        at >= start
            && end.is_none_or(|e| at <= e)
            && filters.iter().all(|key| {
                query.get(*key).is_none_or(|v| {
                    r.get(*key).is_some_and(|x| {
                        x.as_str()
                            .map(str::to_owned)
                            .unwrap_or_else(|| x.to_string())
                            == *v
                    })
                })
            })
    });
    crate::pricing_read::ensure(&state.store, false).await.map_err(internal)?;
    crate::usage_history::price_rows(&state.store, &mut rows).map_err(internal)?;
    rows.sort_by(|a, b| {
        b["at"]
            .as_f64()
            .unwrap_or(0.0)
            .total_cmp(&a["at"].as_f64().unwrap_or(0.0))
    });
    let aggregate = crate::usage_history::aggregate;
    let mut groups = serde_json::Map::new();
    for dimension in [
        "challenge_id",
        "role",
        "model",
        "engine",
        "workspace_kind",
        "worker_id",
    ] {
        let mut buckets = std::collections::BTreeMap::<String, Vec<Value>>::new();
        for row in &rows {
            buckets
                .entry(
                    row.get(dimension)
                        .and_then(Value::as_str)
                        .filter(|s| !s.is_empty())
                        .unwrap_or("未上报")
                        .to_owned(),
                )
                .or_default()
                .push(row.clone());
        }
        groups.insert(
            dimension.into(),
            Value::Array(
                buckets
                    .into_iter()
                    .map(|(name, items)| {
                        let mut agg = aggregate(&items);
                        agg["name"] = json!(name);
                        agg
                    })
                    .collect(),
            ),
        );
    }
    let totals = aggregate(&rows);
    let mut hours = std::collections::BTreeMap::<u64, Vec<Value>>::new();
    for row in &rows {
        if let Some(at) = row
            .get("at")
            .and_then(Value::as_f64)
            .filter(|n| n.is_finite() && *n >= 0.0)
        {
            hours
                .entry((at as u64 / 3600) * 3600)
                .or_default()
                .push(row.clone());
        }
    }
    let series: Vec<_> = hours
        .into_iter()
        .map(|(at, items)| {
            let mut value = aggregate(&items);
            value["at"] = json!(at);
            value
        })
        .collect();
    let count = rows.len();
    let records: Vec<_> = rows.into_iter().skip(offset).take(limit).collect();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(internal)?
        .as_secs_f64();
    Ok(Json(
        json!({"as_of":now,"scope":query,"totals":totals,"groups":groups,"series":series,"records":records,"count":count,"offset":offset,"limit":limit}),
    ))
}

pub(crate) async fn import_usage_history(State(state): State<Arc<AppState>>) -> ApiResult {
    let result = crate::usage_history::import_run_history(&state.store).map_err(|error| {
        tracing::error!(%error, "Historical usage import failed");
        internal(error)
    })?;
    Ok(Json(result))
}

pub(crate) async fn delete_model_endpoint(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Query(query): Query<std::collections::HashMap<String, String>>,
) -> ApiResult {
    delete_credential_account(
        State(state),
        Path(format!("endpoint:{}", id.trim_start_matches("endpoint:"))),
        Query(query),
    )
    .await
}

pub(crate) async fn pricing_catalog(
    State(state): State<Arc<AppState>>,
    Query(query): Query<std::collections::HashMap<String, String>>,
) -> ApiResult {
    let refresh = match query
        .get("refresh")
        .map(|value| value.to_lowercase())
        .as_deref()
    {
        None | Some("false" | "0" | "no" | "off") => false,
        Some("true" | "1" | "yes" | "on") => true,
        _ => return Err(invalid("refresh must be a boolean")),
    };
    crate::pricing_read::ensure(&state.store, refresh)
        .await
        .map_err(internal)?;
    crate::pricing_read::catalog(
        &state.store,
        query.get("search").map(String::as_str).unwrap_or(""),
        query.get("engine").map(String::as_str).unwrap_or(""),
    )
    .map(Json)
    .map_err(internal)
}
fn save_pricing_config(state: &AppState, kind: &str, body: Value, delete: bool) -> ApiResult {
    crate::pricing_read::write_config(&state.store, kind, &body, delete)
        .map(Json)
        .map_err(|error| match error {
            crate::pricing_read::ConfigError::Invalid(message) => invalid(&message),
            crate::pricing_read::ConfigError::Storage => internal("pricing storage"),
        })
}
pub(crate) async fn put_price_override(
    State(state): State<Arc<AppState>>,
    Json(body): Json<Value>,
) -> ApiResult {
    save_pricing_config(&state, "override", body, false)
}
pub(crate) async fn delete_price_override(
    State(state): State<Arc<AppState>>,
    Json(body): Json<Value>,
) -> ApiResult {
    save_pricing_config(&state, "override", body, true)
}
pub(crate) async fn put_price_reference(
    State(state): State<Arc<AppState>>,
    Json(body): Json<Value>,
) -> ApiResult {
    save_pricing_config(&state, "reference", body, false)
}
pub(crate) async fn delete_price_reference(
    State(state): State<Arc<AppState>>,
    Json(body): Json<Value>,
) -> ApiResult {
    save_pricing_config(&state, "reference", body, true)
}
pub(crate) async fn resolve_prices(State(state): State<Arc<AppState>>, Json(body): Json<Value>) -> ApiResult {
    let items = body["items"]
        .as_array()
        .filter(|items| items.len() <= 1000)
        .ok_or_else(|| invalid("items must be an array of at most 1000 objects"))?;
    crate::pricing_read::ensure(&state.store, false)
        .await
        .map_err(internal)?;
    crate::pricing_read::resolve(&state.store, items)
        .map(Json)
        .map_err(|message| invalid(&message))
}
fn summary_totals(rows: &[Value]) -> Value {
    let mut totals = crate::usage_history::aggregate(rows);
    let sum = |key: &str| -> Value {
        let values: Vec<_> = rows
            .iter()
            .filter_map(|r| r[key].as_f64())
            .filter(|n| n.is_finite() && *n >= 0.0)
            .collect();
        if values.is_empty() {
            Value::Null
        } else {
            json!(values.iter().sum::<f64>())
        }
    };
    let mut known = Vec::new();
    let mut unclassified = Vec::new();
    for row in rows {
        if let Some(input) = row["input_tokens"].as_f64() {
            let read = row["cache_read_tokens"].as_f64();
            let write = row["cache_write_tokens"].as_f64();
            if let (Some(read), Some(write)) = (read, write) {
                known.push((input - read - write).max(0.0));
                unclassified.push(0.0);
            } else {
                unclassified.push((input - read.unwrap_or(0.0) - write.unwrap_or(0.0)).max(0.0));
            }
        }
    }
    totals["uncached_input"] = if known.is_empty() {
        Value::Null
    } else {
        json!(known.iter().sum::<f64>())
    };
    totals["unclassified_input"] = if unclassified.is_empty() {
        Value::Null
    } else {
        json!(unclassified.iter().sum::<f64>())
    };
    totals["cache_read"] = sum("cache_read_tokens");
    totals["cache_write"] = sum("cache_write_tokens");
    totals["output"] = sum("output_tokens");
    totals["reasoning"] = sum("reasoning_tokens");
    totals["sessions"] = json!(
        rows.iter()
            .filter_map(|r| r["session_id"]
                .as_str()
                .or(r["thread_id"].as_str())
                .or(r["run_id"].as_str())
                .map(|id| (r["engine"].as_str().unwrap_or("unknown"), id)))
            .collect::<std::collections::BTreeSet<_>>()
            .len()
    );
    totals["missing"] = json!(
        rows.iter()
            .filter(|r| r["input_tokens"].is_null()
                || r["output_tokens"].is_null()
                || r["input_lower_bound"] == true)
            .count()
    );
    totals
}

fn usage_local_time(
    local: chrono::NaiveDateTime,
    zone: chrono_tz::Tz,
) -> Result<chrono::DateTime<chrono_tz::Tz>, (StatusCode, Json<Value>)> {
    if let Some(value) = zone.from_local_datetime(&local).earliest() {
        return Ok(value);
    }
    let gap = chrono_tz::GapInfo::new(&local, &zone)
        .and_then(|gap| gap.begin)
        .ok_or_else(|| invalid("Invalid local usage window boundary"))?;
    local
        .and_local_timezone(gap.1.fix())
        .single()
        .map(|dt| dt.with_timezone(&zone))
        .ok_or_else(|| invalid("Invalid local usage window boundary"))
}
fn usage_datetime(
    at: f64,
    zone: chrono_tz::Tz,
) -> Result<chrono::DateTime<chrono_tz::Tz>, (StatusCode, Json<Value>)> {
    chrono::DateTime::from_timestamp_millis((at * 1000.0) as i64)
        .map(|dt| dt.with_timezone(&zone))
        .ok_or_else(|| invalid("Usage time is out of range"))
}

pub(crate) async fn usage_summary(
    State(state): State<Arc<AppState>>,
    Query(query): Query<std::collections::HashMap<String, String>>,
) -> ApiResult {
    let now = chrono::Utc::now().timestamp_millis() as f64 / 1000.0;
    let scope = query.get("scope").map(String::as_str).unwrap_or("history");
    if !["history", "muteki"].contains(&scope) {
        return Err(invalid("invalid usage scope"));
    }
    if scope == "history"
        && [
            "run_id",
            "thread_id",
            "competition_id",
            "challenge_id",
            "role",
            "workspace_kind",
            "generation",
        ]
        .iter()
        .any(|key| query.contains_key(*key))
    {
        return Err(invalid(
            "Task and role filters apply only to internal Muteki usage",
        ));
    }
    let day_selection = query.get("days").map(String::as_str).unwrap_or("30");
    let days = match day_selection {
        "all" => 90,
        "custom" => 30,
        other => other.parse::<u32>().map_err(|_| invalid("invalid days"))?,
    };
    if ![1, 7, 30, 90].contains(&days) {
        return Err(invalid("days must be 1, 7, 30, 90, all or custom"));
    }
    let time_zone = query.get("tz").map(String::as_str).unwrap_or("UTC");
    let zone: chrono_tz::Tz = time_zone
        .parse()
        .map_err(|_| invalid("Unrecognized usage time zone"))?;
    let time = |key: &str, default: f64| -> Result<f64, (StatusCode, Json<Value>)> {
        query
            .get(key)
            .map(|v| {
                v.parse::<f64>()
                    .ok()
                    .filter(|n| n.is_finite() && *n >= 0.0)
                    .ok_or_else(|| invalid("invalid time"))
            })
            .unwrap_or(Ok(default))
    };
    let end = time("end", now)?;
    let end_local = usage_datetime(end, zone)?;
    let default_start = if query.contains_key("start") || day_selection == "all" {
        0.0
    } else {
        let local = if days == 1 {
            end_local
                .naive_local()
                .checked_sub_signed(chrono::Duration::hours(24))
        } else {
            end_local
                .date_naive()
                .checked_sub_days(chrono::Days::new(u64::from(days - 1)))
                .and_then(|date| date.and_hms_opt(0, 0, 0))
        }
        .ok_or_else(|| invalid("Usage time is out of range"))?;
        usage_local_time(local, zone)?.timestamp_millis() as f64 / 1000.0
    };
    let mut start = time("start", default_start)?;
    usage_datetime(start, zone)?;
    if end < start {
        return Err(invalid("end precedes start"));
    }
    let mut rows = crate::usage_history::rows(&state.store, scope).map_err(internal)?;
    rows.retain(|r| {
        r["at"].as_f64().is_some_and(|at| at >= start && at <= end)
            && [
                "engine",
                "run_id",
                "thread_id",
                "competition_id",
                "challenge_id",
                "workspace_kind",
                "generation",
                "model",
                "role",
            ]
            .iter()
            .all(|key| {
                query.get(*key).is_none_or(|v| {
                    r.get(*key).is_some_and(|x| {
                        x.as_str()
                            .map(str::to_owned)
                            .unwrap_or_else(|| x.to_string())
                            == *v
                    })
                })
            })
    });
    if start == 0.0 {
        start = rows
            .iter()
            .filter_map(|r| r["at"].as_f64())
            .min_by(f64::total_cmp)
            .unwrap_or(end);
    }
    let hourly = if query.contains_key("start") || day_selection == "all" {
        end - start <= 86400.0
    } else {
        days == 1
    };
    let slot_format = if hourly { "%Y-%m-%dT%H:00" } else { "%Y-%m-%d" };
    crate::pricing_read::ensure(&state.store, false).await.map_err(internal)?;
    crate::usage_history::price_rows(&state.store, &mut rows).map_err(internal)?;
    let pricing = crate::pricing_read::metadata(&state.store).map_err(internal)?;
    let totals = summary_totals(&rows);
    let mut providers = std::collections::BTreeMap::<String, Vec<Value>>::new();
    let mut models = std::collections::BTreeMap::<(String, String), Vec<Value>>::new();
    let mut slots = std::collections::BTreeMap::<String, Vec<Value>>::new();
    for row in &rows {
        let engine = row["engine"].as_str().unwrap_or("unknown").to_owned();
        providers
            .entry(engine.clone())
            .or_default()
            .push(row.clone());
        models
            .entry((
                engine,
                row["model"].as_str().unwrap_or("Unknown model").to_owned(),
            ))
            .or_default()
            .push(row.clone());
        if let Some(at) = row["at"].as_f64() {
            let dt = usage_datetime(at, zone)?;
            slots
                .entry(dt.format(slot_format).to_string())
                .or_default()
                .push(row.clone());
        }
    }

    let provider_rows: Vec<Value> = providers
        .into_iter()
        .map(|(engine, rows)| {
            let mut value = summary_totals(&rows);
            value["engine"] = json!(engine);
            value["status"] = json!("ledger");
            value
        })
        .collect();
    let model_rows: Vec<Value> = models
        .into_iter()
        .map(|((engine, model), rows)| {
            let mut value = summary_totals(&rows);
            value["engine"] = json!(engine);
            value["model"] = json!(model);
            value
        })
        .collect();
    let sources: Vec<_> = provider_rows.iter().map(|row|json!({"engine":row["engine"],"status":"ledger","scope":scope,"message":if scope=="history"{"Saved provider cache and internal supplemental observations only; live host history discovery is unavailable."}else{"Saved internal usage and current Rust chat observations; captured amounts and catalog/custom estimates."}})).collect();
    let since = usage_datetime(start, zone)?;
    if hourly {
        let mut point = since.with_timezone(&chrono::Utc)
            - chrono::Duration::seconds(i64::from(since.minute()) * 60 + i64::from(since.second()))
            - chrono::Duration::nanoseconds(i64::from(since.nanosecond()));
        while point.timestamp_millis() as f64 / 1000.0 <= end {
            slots
                .entry(point.with_timezone(&zone).format(slot_format).to_string())
                .or_default();
            point = point
                .checked_add_signed(chrono::Duration::hours(1))
                .ok_or_else(|| invalid("Usage time is out of range"))?;
        }
    } else {
        let mut date = since.date_naive();
        while date <= end_local.date_naive() {
            slots
                .entry(date.format(slot_format).to_string())
                .or_default();
            date = date
                .succ_opt()
                .ok_or_else(|| invalid("Usage time is out of range"))?;
        }
    }
    let slot_names: Vec<_> = slots.keys().cloned().collect();
    let series:Vec<_> = slots.into_iter().map(|(slot,rows)| {
        let totals=summary_totals(&rows);
        let mut by_engine=std::collections::BTreeMap::<String,Vec<Value>>::new();
        for row in rows { by_engine.entry(row["engine"].as_str().unwrap_or("unknown").to_owned()).or_default().push(row); }
        let by_engine:serde_json::Map<_,_>=by_engine.into_iter().map(|(engine, rows)| {
            let t=summary_totals(&rows); (engine,json!({"total_tokens":t["total_tokens"],"cost":t["cost"]}))
        }).collect();
        json!({"slot":slot,"total_tokens":if totals["records"]==0 {json!(0)}else{totals["total_tokens"].clone()},"cost":if totals["records"]==0 {json!(0)}else{totals["cost"].clone()},"providers":by_engine})
    }).collect();
    Ok(Json(
        json!({"as_of":now,"window":{"days":days,"resolution":if hourly {"hour"} else {"day"},
        "time_zone":time_zone,"slots":slot_names,"since_ms":start*1000.0,"until_ms":end*1000.0},
        "scope":scope,"totals":totals,"providers":provider_rows,"models":model_rows,"series":series,
        "sources":sources,"pricing":pricing,
        "cache_warning":""}),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credential_projection_excludes_secrets_and_unknown_input() {
        let public = public_account(
            json!({"account_id":"one","engine":"openai","secret":"private",
            "api_key":"also private","codex_auth_json":"{}","details":{"secret":"nested"},"base_url":"https://example.invalid"}),
        );
        assert_eq!(public["present"], true);
        assert_eq!(public["credential_format"], "auth_json");
        assert!(public.get("secret").is_none());
        assert!(public.get("codex_auth_json").is_none());
        assert!(public.get("api_key").is_none());
        assert_eq!(public["details"], json!({}));
    }

    #[test]
    fn usage_preserves_unknown_costs_and_counts_actual_observations() {
        let value = summary_totals(&[
            json!({"thread_id":"a","input_tokens":7,"output_tokens":3}),
            json!({"thread_id":"a","input_tokens":2,"output_tokens":5,"reported_cost":0.25,"cost":0.25}),
        ]);
        assert_eq!(value["records"], 2);
        assert_eq!(value["sessions"], 1);
        assert_eq!(value["total_tokens"], 17.0);
        assert_eq!(value["unpriced"], 1);
        assert_eq!(value["cost"], 0.25);
        assert!(summary_totals(&[])["total_tokens"].is_null());
    }

    #[tokio::test]
    async fn credential_account_validates_and_exposes_model_capabilities() {
        let root = tempfile::tempdir().unwrap();
        let store = crate::store::Store::open(":memory:").unwrap();
        store.put("system", "state_dir", &json!(root.path())).unwrap();
        let state = Arc::new(AppState::new(store));

        let err = put_credential_account(
            State(state.clone()),
            Path("pi-bad".into()),
            Json(json!({
                "worker_engine": "pi",
                "connection": "custom_endpoint",
                "base_url": "https://example.invalid/v1",
                "secret": "synthetic",
                "model_capabilities": {
                    "deepseek-flash": {"context_window": 0}
                }
            })),
        )
        .await
        .unwrap_err();
        assert_eq!(err.0, StatusCode::UNPROCESSABLE_ENTITY);

        let Json(saved) = put_credential_account(
            State(state.clone()),
            Path("pi-good".into()),
            Json(json!({
                "worker_engine": "pi",
                "engine": "api",
                "target_engine": "pi",
                "connection": "custom_endpoint",
                "base_url": "https://example.invalid/v1",
                "secret": "synthetic",
                "target_model": "deepseek-flash",
                "models": ["deepseek-flash"],
                "model_capabilities": {
                    "deepseek-flash": {
                        "contextWindow": 1_024_000,
                        "maxTokens": 65_536,
                        "reasoning": true,
                        "input": ["text"]
                    }
                }
            })),
        )
        .await
        .unwrap();
        assert_eq!(
            saved["account"]["model_capabilities"]["deepseek-flash"]["context_window"],
            1_024_000
        );
        assert_eq!(
            saved["account"]["model_capabilities"]["deepseek-flash"]["max_output_tokens"],
            65_536
        );
        let Json(listed) = credentials(State(state), Query(Default::default()))
            .await
            .unwrap();
        let row = listed["credentials"]
            .as_array()
            .unwrap()
            .iter()
            .find(|item| item["account_id"] == "pi-good")
            .unwrap();
        assert_eq!(
            row["model_capabilities"]["deepseek-flash"]["context_window"],
            1_024_000
        );
    }
}
