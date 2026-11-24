//! Inert worker-template preferences. These routes never start a worker or CLI.
use crate::AppState;
use axum::{Json, Router, extract::State, http::StatusCode, routing::get};
use serde_json::{Value, json};
use std::sync::{Arc, OnceLock};

type ApiResult = Result<Json<Value>, (StatusCode, Json<Value>)>;
const FIELDS: &[&str] = &[
    "engines",
    "max_workers",
    "worker_backend",
    "worker_node_id",
    "worker_network",
    "worker_container_scope",
    "worker_privilege",
    "worker_oci_runtime",
    "worker_memory",
    "worker_cpus",
    "worker_pids_limit",
    "worker_output_limit",
    "worker_disk_limit",
    "wall_clock_budget",
    "max_total_workers",
    "cost_budget_usd",
    "stage_policy",
    "llm_profiles",
    "worker_profiles",
    "seats",
    "credentials",
    "seat_alias",
    "credential_alias",
    "overrides",
    "review",
    "verifier",
    "dispatch_templates",
];
fn error(detail: &str) -> (StatusCode, Json<Value>) {
    (
        StatusCode::UNPROCESSABLE_ENTITY,
        Json(json!({"detail":detail})),
    )
}
fn storage(_: impl std::fmt::Display) -> (StatusCode, Json<Value>) {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({"detail":"Could not access worker template preferences"})),
    )
}
fn secret_key(key: &str) -> bool {
    matches!(
        key.to_ascii_lowercase().as_str(),
        "secret"
            | "api_key"
            | "token"
            | "access_token"
            | "refresh_token"
            | "authorization"
            | "codex_auth_json"
            | "password"
    )
}
fn has_secret(value: &Value) -> bool {
    match value {
        Value::Object(row) => row.iter().any(|(key, value)| {
            (secret_key(key) && !value.is_null() && value.as_str() != Some("")) || has_secret(value)
        }),
        Value::Array(rows) => rows.iter().any(has_secret),
        _ => false,
    }
}
fn redact(value: &mut Value) {
    match value {
        Value::Object(row) => {
            row.retain(|key, _| !secret_key(key));
            for value in row.values_mut() {
                redact(value);
            }
        }
        Value::Array(rows) => {
            for value in rows {
                redact(value)
            }
        }
        _ => {}
    }
}
fn defaults() -> Value {
    json!({"engines":[],"max_workers":10,"worker_backend":"container","worker_node_id":"local","worker_network":"bridge","worker_container_scope":"run","worker_privilege":"root","worker_oci_runtime":"","worker_memory":null,"worker_cpus":null,"worker_pids_limit":null,"worker_output_limit":null,"worker_disk_limit":null,"wall_clock_budget":0,"max_total_workers":0,"cost_budget_usd":0,"stage_policy":{"prepare":{},"coordinator":{"wall_clock_budget":0},"budgets":{"max_total_workers":0,"cost_budget_usd":0}},"llm_profiles":{"planner":{"provider":"","model":"","endpoint_id":"","base_url":"","connection":"default","temperature_mode":"omit"},"titler":{"provider":"","model":"","endpoint_id":"","base_url":"","connection":"default","temperature_mode":"omit"}},"worker_profiles":[],"seats":[],"credentials":[],"seat_alias":{},"credential_alias":{},"overrides":{},"review":{"enabled":false,"max_concurrent":1,"allow_review_fallback":false},"verifier":{"enabled":false,"max_concurrent":0,"allow_verifier_fallback":false},"dispatch_templates":[]})
}

pub(crate) fn redact_url_userinfo(raw: &str) -> String {
    let Ok(mut parsed) = url::Url::parse(raw.trim()) else {
        return raw.trim().to_owned();
    };
    if !parsed.username().is_empty() || parsed.password().is_some() {
        let _ = parsed.set_username("***");
        if parsed.password().is_some() {
            let _ = parsed.set_password(Some("***"));
        }
    }
    parsed.to_string()
}

pub(crate) fn redact_secrets(text: &str, extra_secrets: &[&str]) -> String {
    static RE_URL_CRED: OnceLock<regex::Regex> = OnceLock::new();
    static RE_BEARER: OnceLock<regex::Regex> = OnceLock::new();
    static RE_SK: OnceLock<regex::Regex> = OnceLock::new();
    static RE_APIKEY: OnceLock<regex::Regex> = OnceLock::new();

    let mut out = text.to_owned();
    for secret in extra_secrets {
        let trimmed = secret.trim();
        if trimmed.len() >= 3 {
            out = out.replace(trimmed, "[REDACTED]");
        }
    }
    let re_url = RE_URL_CRED.get_or_init(|| regex::Regex::new(r"(?i)\b((?:https?|socks5h?)://)([^:/@\s]+):([^@\s]+)@").unwrap());
    out = re_url.replace_all(&out, "${1}***:***@").into_owned();
    let re_bearer = RE_BEARER.get_or_init(|| regex::Regex::new(r"(?i)\b(Bearer\s+)[A-Za-z0-9._~+/=-]{6,}").unwrap());
    out = re_bearer.replace_all(&out, "${1}[REDACTED]").into_owned();
    let re_sk = RE_SK.get_or_init(|| regex::Regex::new(r"\bsk-[A-Za-z0-9_-]{8,}").unwrap());
    out = re_sk.replace_all(&out, "sk-[REDACTED]").into_owned();
    let re_kv = RE_APIKEY.get_or_init(|| regex::Regex::new(r"(?i)\b(api[_-]?key|x-api-key|authorization|proxy-authorization)\s*([:=])\s*([^\s,;]+)").unwrap());
    out = re_kv.replace_all(&out, "${1}${2}[REDACTED]").into_owned();
    out
}

fn sanitize_roles(roles: &mut Vec<Value>) {
    let had_race = roles.iter().any(|r| r == "race");
    roles.retain(|r| r != "race");
    if had_race
        && !roles.iter().any(|r| {
            matches!(
                r.as_str(),
                Some("explore" | "bootstrap" | "respond" | "solve" | "scout" | "code" | "shell_agent")
            )
        })
    {
        roles.insert(0, json!("explore"));
    }
}
pub(crate) fn sanitize_worker_config(config: &mut Value) {
    let Some(obj) = config.as_object_mut() else {
        return;
    };
    for retired in [
        "start_workers",
        "race_scout",
        "race_timeout",
        "race_engines",
        "cli_race",
    ] {
        obj.remove(retired);
    }
    if let Some(sp) = obj.get_mut("stage_policy").and_then(Value::as_object_mut) {
        sp.remove("race");
        if let Some(coord) = sp.get_mut("coordinator").and_then(Value::as_object_mut) {
            coord.remove("dispatch_mode");
            if let Some(review) = coord.get_mut("review").and_then(Value::as_object_mut) {
                review.remove("after_race");
            }
        }
    }
    if let Some(review) = obj.get_mut("review").and_then(Value::as_object_mut) {
        review.remove("after_race");
    }
    if let Some(overrides) = obj.get_mut("overrides").and_then(Value::as_object_mut) {
        for entry in overrides.values_mut() {
            if let Some(cat_obj) = entry.as_object_mut() {
                cat_obj.remove("start_workers");
            }
        }
    }
    for collection in ["seats", "worker_profiles"] {
        if let Some(items) = obj.get_mut(collection).and_then(Value::as_array_mut) {
            for item in items {
                if let Some(item_obj) = item.as_object_mut() {
                    item_obj.remove("race");
                    if let Some(roles) = item_obj.get_mut("roles").and_then(Value::as_array_mut) {
                        sanitize_roles(roles);
                    }
                }
            }
        }
    }
}
pub(crate) fn project_seats(state: &AppState, config: &mut Value, force_profiles: bool) {
    let Some(seats) = config["seats"].as_array().cloned() else {
        return;
    };
    if seats.is_empty() {
        return;
    }
    let accounts = state.store.list("credential_accounts").unwrap_or_default();
    let mut existing_creds: Vec<Value> = config["credentials"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    for seat in &seats {
        let engine = seat["engine"].as_str().unwrap_or("").trim();
        let cred_id = seat["credential_id"].as_str().unwrap_or("").trim();
        if cred_id.is_empty()
            || existing_creds
                .iter()
                .any(|c| c["id"].as_str() == Some(cred_id))
        {
            continue;
        }
        if let Some(sys_eng) = cred_id.strip_prefix("system:") {
            let eng = if sys_eng.is_empty() { engine } else { sys_eng };
            existing_creds.push(json!({
                "id": cred_id,
                "label": format!("{eng} 系统登录"),
                "engine": eng,
                "kind": "system_inherit",
                "secret_ref": "",
            }));
        } else {
            let acct_id = cred_id.strip_prefix("account:").unwrap_or(cred_id);
            let account = accounts
                .iter()
                .find(|a| a["account_id"].as_str() == Some(acct_id));
            let connection = account
                .and_then(|a| a["connection"].as_str())
                .unwrap_or("official");
            let custom = connection == "custom_endpoint"
                || account
                    .and_then(|a| a["base_url"].as_str())
                    .is_some_and(|u| !u.trim().is_empty());
            let mut cred = json!({
                "id": cred_id,
                "label": acct_id,
                "engine": engine,
                "kind": if custom { "custom_endpoint" } else { "engine_key" },
                "secret_ref": acct_id,
            });
            if custom {
                cred["target_engine"] = json!(engine);
            }
            existing_creds.push(cred);
        }
    }
    config["credentials"] = json!(existing_creds);

    let profiles_empty = config["worker_profiles"]
        .as_array()
        .is_none_or(|p| p.is_empty());
    if force_profiles || profiles_empty {
        let profiles: Vec<Value> = seats
            .iter()
            .map(|seat| {
                let id = seat["id"].as_str().unwrap_or("").trim();
                let label = seat["label"].as_str().unwrap_or(id).trim();
                let engine = seat["engine"].as_str().unwrap_or("").trim();
                let cred_id = seat["credential_id"].as_str().unwrap_or("").trim();
                let cred = existing_creds
                    .iter()
                    .find(|c| c["id"].as_str() == Some(cred_id));
                let kind = cred
                    .and_then(|c| c["kind"].as_str())
                    .unwrap_or(if cred_id.starts_with("system:") || cred_id.is_empty() {
                        "system_inherit"
                    } else {
                        "engine_key"
                    });
                let credential_account = if kind == "system_inherit" {
                    ""
                } else {
                    cred.and_then(|c| c["secret_ref"].as_str())
                        .unwrap_or_else(|| cred_id.strip_prefix("account:").unwrap_or(cred_id))
                };
                let credential_mode = match kind {
                    "custom_endpoint" | "engine_key" => "api_key",
                    _ => "subscription",
                };
                let mut roles = seat["roles"]
                    .as_array()
                    .cloned()
                    .unwrap_or_else(|| {
                        vec![
                            json!("bootstrap"),
                            json!("explore"),
                            json!("respond"),
                            json!("solve"),
                            json!("scout"),
                        ]
                    });
                sanitize_roles(&mut roles);
                let max_running = seat["capacity"]["max_running"]
                    .as_u64()
                    .or_else(|| seat["max_running"].as_u64())
                    .unwrap_or(1);
                let max_review_running = seat["capacity"]["max_review_running"]
                    .as_u64()
                    .or_else(|| seat["max_review_running"].as_u64())
                    .unwrap_or(0);
                let max_verifier_running = seat["capacity"]["max_verifier_running"]
                    .as_u64()
                    .or_else(|| seat["max_verifier_running"].as_u64())
                    .unwrap_or(0);
                let priority = seat["priority"].as_u64().unwrap_or(10);
                let model = seat["model"].as_str().unwrap_or("").trim();
                let reasoning_effort = seat["reasoning_effort"]
                    .as_str()
                    .unwrap_or("default")
                    .trim();
                let enabled = seat["enabled"].as_bool().unwrap_or(true);
                json!({
                    "id": id,
                    "name": id,
                    "label": label,
                    "engine": engine,
                    "transport": engine,
                    "credential_mode": credential_mode,
                    "auth": credential_mode,
                    "credential_account": credential_account,
                    "credential_id": cred_id,
                    "credential_kind": kind,
                    "api_key_ref": "",
                    "base_url": "",
                    "wire_api": "",
                    "roles": roles,
                    "max_running": max_running,
                    "max_review_running": max_review_running,
                    "max_verifier_running": max_verifier_running,
                    "unlimited_workers": seat["unlimited_workers"].as_bool().unwrap_or(false),
                    "soft_reminder_seconds": seat["soft_reminder_seconds"].as_u64().unwrap_or(600),
                    "execution_mode": seat["execution_mode"].as_str().unwrap_or("step"),
                    "max_continuations": seat["max_continuations"].as_u64().unwrap_or(0),
                    "priority": priority,
                    "model": model,
                    "reasoning_effort": reasoning_effort,
                    "enabled": enabled,
                })
            })
            .collect();
        config["worker_profiles"] = json!(profiles);
    }
}
fn ordinary_seat(seat: &Value) -> bool {
    seat["roles"].as_array().filter(|roles| !roles.is_empty()).is_none_or(|roles| {
        roles.iter().any(|role| {
            matches!(
                role.as_str(),
                Some("bootstrap" | "explore" | "respond" | "solve" | "scout")
            )
        })
    })
}
fn valid_dispatch_templates(value: &Value) -> bool {
    let Some(rows) = value.as_array() else {
        return false;
    };
    let mut ids = std::collections::HashSet::new();
    rows.iter().all(|row| {
        let Some(seats) = row["seats"].as_array().filter(|seats| !seats.is_empty()) else {
            return false;
        };
        let mut seat_ids = std::collections::HashSet::new();
        row["id"]
            .as_str()
            .is_some_and(|id| !id.trim().is_empty() && ids.insert(id))
            && row["name"].as_str().is_some_and(|name| !name.trim().is_empty())
            && seats.iter().all(|seat| {
                seat["id"]
                    .as_str()
                    .is_some_and(|id| !id.trim().is_empty() && seat_ids.insert(id))
                    && seat["engine"].as_str().is_some_and(|engine| !engine.trim().is_empty())
                    && seat["credential_id"].as_str().is_some_and(|id| {
                        id.starts_with("system:") || id.starts_with("account:")
                    })
            })
    })
}
/// Replace the roster of a launch configuration with a saved dispatch template.
/// Dedicated Review/Verifier seats stay from the global roster so those channels keep working.
pub(crate) fn apply_dispatch_template(
    state: &AppState,
    config: &mut Value,
    template_id: &str,
) -> Result<(), String> {
    let template = config["dispatch_templates"]
        .as_array()
        .and_then(|rows| rows.iter().find(|row| row["id"] == template_id))
        .cloned()
        .ok_or_else(|| format!("出战模板不存在或已被删除：{template_id}"))?;
    let name = template["name"].as_str().unwrap_or(template_id).to_owned();
    let mut seats: Vec<Value> = template["seats"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|seat| seat["enabled"].as_bool().unwrap_or(true))
        .cloned()
        .collect();
    if seats.is_empty() {
        return Err(format!("出战模板「{name}」没有启用的 Worker"));
    }
    let engines: Vec<Value> = seats.iter().map(|seat| seat["id"].clone()).collect();
    let max_workers: u64 = seats
        .iter()
        .map(|seat| {
            seat["capacity"]["max_running"]
                .as_u64()
                .unwrap_or(1)
                .max(1)
        })
        .sum();
    let dedicated: Vec<Value> = config["seats"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|seat| !ordinary_seat(seat) && !engines.contains(&seat["id"]))
        .cloned()
        .collect();
    seats.extend(dedicated);
    config["seats"] = json!(seats);
    config["engines"] = json!(engines);
    config["max_workers"] = json!(max_workers.max(1));
    project_seats(state, config, true);
    config["dispatch_template"] = json!({"id": template_id, "name": name});
    Ok(())
}
pub(crate) fn configuration(state: &AppState) -> Result<Value, (StatusCode, Json<Value>)> {
    let saved = state
        .store
        .get("settings", "worker_templates")
        .map_err(storage)?;
    let source = match saved {
        Some(value) => value,
        None => state
            .store
            .get("import_metadata", "_worker_config.json")
            .map_err(storage)?
            .unwrap_or_else(|| json!({})),
    };
    let mut config = defaults();
    for key in FIELDS {
        if let Some(value) = source.get(*key) {
            config[*key] = value.clone();
        }
    }
    for collection in ["worker_profiles", "seats", "credentials"] {
        if source.get(collection).is_none() {
            config[collection] = json!(state.store.list(collection).map_err(storage)?);
        }
    }
    config["worker_privilege"] = json!("root");
    sanitize_worker_config(&mut config);
    project_seats(state, &mut config, false);
    let mut notices = Vec::new();
    if config["worker_backend"] == "container_dockerexec" {
        config["worker_backend"] = json!("container");
        notices.push(json!({"field":"worker_backend","from":"container_dockerexec","to":"container","detail":"旧版 container_dockerexec 执行方式已合并为容器执行；保存一次设置即可写入新值"}));
    }
    if config["worker_network"] == "none" {
        notices.push(json!({"field":"worker_network","from":"none","to":null,"detail":"已保存的 none 网络无法运行 Worker（无法连接控制通道与模型服务）；请改为 bridge 或 host"}));
    }
    config["migration_notices"] = json!(notices);
    if let Ok(limits) = crate::runs::container_exec::resolve_worker_resource_limits(Some(&config))
        && let Some(obj) = limits.to_settings_fields().as_object()
    {
        config["worker_resource_effective"] = Value::Object(obj.clone());
        for (k, v) in obj {
            if !config[k].is_null(){config[k] = v.clone();}
        }
    }
    let net_res = crate::runs::container_exec::project_worker_network(
        config["worker_network"].as_str().unwrap_or("bridge"),
    );
    config["effective_network"] = net_res.effective.map(Value::String).unwrap_or(Value::Null);
    config["network_requested"] = json!(net_res.requested);
    config["network_reason"] = net_res.reason.map(Value::String).unwrap_or(Value::Null);
    config["network_error"] = net_res.error.map(Value::String).unwrap_or(Value::Null);
    config["in_container"] = json!(crate::runs::container_exec::is_running_in_container());
    config["agent_version_policy"] =
        crate::agent_runtime::version_policy::policy_summary_json(&state.store);
    redact(&mut config);
    Ok(config)
}
pub fn router() -> Router<Arc<AppState>> {
    Router::new().route("/api/settings/workers", get(get_workers).put(put_workers))
}

pub(crate) fn planner_configured(state: &AppState, config: &Value) -> bool {
    let planner = &config["llm_profiles"]["planner"];
    if !planner["model"].as_str().is_some_and(|model| !model.trim().is_empty()) {
        return false;
    }
    planner["endpoint_id"]
        .as_str()
        .or_else(|| planner["credential_id"].as_str())
        .and_then(|reference| {
            reference.strip_prefix("endpoint:").or_else(|| reference.strip_prefix("account:"))
        })
        .filter(|id| !id.trim().is_empty())
        .and_then(|id| crate::agent_runtime::credentials::account(&state.store, id))
        .is_some_and(|account| {
            (account["worker_engine"] == "pi" || account["engine"] == "pi")
                && crate::compat::public_account(account)["present"] == true
        })
}

async fn get_workers(State(state): State<Arc<AppState>>) -> ApiResult {
    let mut config = configuration(&state)?;
    config["planner_configured"] = json!(planner_configured(&state, &config));
    Ok(Json(json!({"config":config})))
}
pub(crate) async fn put_workers(State(state): State<Arc<AppState>>, Json(patch): Json<Value>) -> ApiResult {
    let object = patch
        .as_object()
        .ok_or_else(|| error("Expected an object"))?;
    if has_secret(&patch) {
        return Err(error(
            "Save API keys in credential accounts and reference their IDs in worker templates",
        ));
    }
    if object.get("worker_network").and_then(Value::as_str) == Some("none") {
        return Err(error("worker_network=none 会切断 Worker 与控制通道和模型服务的连接，无法运行；请使用 bridge 或 host"));
    }
    if let Some(node) = object.get("worker_node_id").and_then(Value::as_str).filter(|node| *node != "local") {
        let row = state.store.get("runner_nodes", node).map_err(storage)?;
        if !row.as_ref().is_some_and(|row| row["state"] == "active") {
            return Err(error("所选执行节点不存在或未处于可用状态；请在“执行节点”中检查节点"));
        }
    }
    for (key, value) in object {
        if !FIELDS.contains(&key.as_str()) {
            return Err(error("Unknown worker template preference"));
        }
        let valid = match key.as_str() {
            "worker_node_id" => value.as_str().is_some_and(|v|!v.is_empty()&&v.len()<=160&&v.bytes().all(|b|b.is_ascii_alphanumeric()||matches!(b,b'-'|b'_'))),
            "worker_backend" => value
                .as_str()
                .is_some_and(|v| ["local", "container"].contains(&v)),
            "worker_network" => value
                .as_str()
                .is_some_and(|v| ["bridge", "host"].contains(&v)),
            "worker_oci_runtime" => value.as_str().is_some_and(|v| {
                v.is_empty() || (v.len() <= 64 && v.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.')))
            }),
            "worker_container_scope" => value
                .as_str()
                .is_some_and(|v| ["run", "shared"].contains(&v)),
            "worker_privilege" => value
                .as_str()
                .is_some_and(|v| ["root", "default", "elevated"].contains(&v)),
            "worker_memory" | "worker_output_limit" | "worker_disk_limit" => {
                value.is_null() || value.is_string() || value.is_number()
            }
            "worker_cpus" => {
                value.is_null()
                    || value.is_number()
                    || value
                        .as_str()
                        .is_some_and(|s| s.trim().parse::<f64>().is_ok())
            }
            "worker_pids_limit" => {
                value.is_null()
                    || value.is_u64()
                    || value.is_i64()
                    || value
                        .as_str()
                        .is_some_and(|s| s.trim().parse::<i64>().is_ok())
            }
            "engines" => value
                .as_array()
                .is_some_and(|rows| rows.iter().all(Value::is_string)),
            "worker_profiles" | "seats" | "credentials" => value.as_array().is_some_and(|rows| {
                let mut ids = std::collections::HashSet::new();
                rows.iter().all(|row| {
                    row.as_object().is_some()
                        && row["id"]
                            .as_str()
                            .is_some_and(|id| !id.trim().is_empty() && ids.insert(id))
                })
            }),
            "dispatch_templates" => {
                if !valid_dispatch_templates(value) {
                    return Err(error("出战模板需要唯一 ID、名称，以及至少一个绑定了凭据的 Worker"));
                }
                true
            }
            "stage_policy" | "llm_profiles" | "seat_alias" | "credential_alias" | "overrides" | "review" | "verifier" => {
                value.is_object()
            }
            "cost_budget_usd" => value.as_f64().is_some_and(|v| v.is_finite() && v >= 0.0),
            _ => value.as_u64().is_some(),
        };
        if !valid {
            return Err(error("Invalid worker template preference"));
        }
    }
    let mut config = configuration(&state)?;
    for (key, value) in object {
        config[key.as_str()] = value.clone();
    }
    config["worker_privilege"] = json!("root");
    sanitize_worker_config(&mut config);
    let limits = crate::runs::container_exec::resolve_worker_resource_limits(Some(&config))
        .map_err(|detail| error(&detail))?;
    if let Some(obj) = limits.to_settings_fields().as_object() {
        config["worker_resource_effective"] = Value::Object(obj.clone());
        for (k, v) in obj {
            if object.get(k).is_some_and(|value|!value.is_null()){config[k] = v.clone();}
        }
    }
    if object.contains_key("seats") && !object.contains_key("credentials") {
        config["credentials"] = json!([]);
    }
    project_seats(&state, &mut config, object.contains_key("seats") && !object.contains_key("worker_profiles"));
    if object.contains_key("seats")
        || object.contains_key("worker_profiles")
        || object.contains_key("engines")
        || object.contains_key("worker_backend")
    {
        let effective_backend = config["worker_backend"].as_str().unwrap_or("container");
        let effective_node = config["worker_node_id"].as_str().unwrap_or("local");
        let preloaded_img = if effective_backend == "container" && effective_node == "local" {
            Some(crate::runs::container_exec::inspect_worker_image_status().await)
        } else {
            None
        };
        let mut violations = Vec::new();
        let mut checked_engines = std::collections::HashSet::new();
        if let Some(profiles) = config["worker_profiles"].as_array() {
            for profile in profiles {
                if !profile["enabled"].as_bool().unwrap_or(true) {
                    continue;
                }
                let raw_eng = profile["engine"].as_str().unwrap_or("").trim();
                if raw_eng.is_empty() {
                    continue;
                }
                let norm_eng = crate::agent_runtime::version_policy::normalize_engine_id(raw_eng);
                if !checked_engines.insert(norm_eng.clone()) {
                    continue;
                }
                if let Err(err) = crate::agent_runtime::version_policy::verify_worker_engine(
                    &state,
                    &norm_eng,
                    effective_backend,
                    effective_node,
                    None,
                    None,
                    preloaded_img.as_ref(),
                )
                .await
                {
                    violations.push(json!({
                        "profile_id": profile["id"].as_str().unwrap_or(""),
                        "label": profile["label"].as_str().unwrap_or(&norm_eng),
                        "engine": norm_eng,
                        "backend": effective_backend,
                        "code": err.code,
                        "message": err.message,
                    }));
                }
            }
        }
        if checked_engines.is_empty()
            && let Some(engines) = config["engines"].as_array()
        {
            for raw in engines.iter().filter_map(Value::as_str) {
                let norm_eng = crate::agent_runtime::version_policy::normalize_engine_id(raw);
                if norm_eng.is_empty() || !checked_engines.insert(norm_eng.clone()) {
                    continue;
                }
                if let Err(err) = crate::agent_runtime::version_policy::verify_worker_engine(
                    &state,
                    &norm_eng,
                    effective_backend,
                    effective_node,
                    None,
                    None,
                    preloaded_img.as_ref(),
                )
                .await
                {
                    violations.push(json!({
                        "profile_id": "",
                        "label": norm_eng,
                        "engine": norm_eng,
                        "backend": effective_backend,
                        "code": err.code,
                        "message": err.message,
                    }));
                }
            }
        }
        if !violations.is_empty() {
            let messages: Vec<&str> = violations
                .iter()
                .filter_map(|v| v["message"].as_str())
                .collect();
            let detail = format!(
                "禁止设置出战 Worker（存在低于最低支持版本的已启用引擎）：{}",
                messages.join(" ")
            );
            return Err((
                StatusCode::UNPROCESSABLE_ENTITY,
                Json(json!({
                    "detail": detail,
                    "error": {
                        "code": crate::agent_runtime::version_policy::ERR_WORKER_VERSION_UNSUPPORTED,
                        "message": detail,
                        "violations": violations,
                    }
                })),
            ));
        }
    }
    if object.contains_key("llm_profiles") {
        let planner = &config["llm_profiles"]["planner"];
        if let Some(reference) = planner["endpoint_id"].as_str().filter(|id| !id.is_empty())
            .or_else(|| planner["credential_id"].as_str().filter(|id| !id.is_empty())) {
            let id = reference.strip_prefix("endpoint:").or_else(|| reference.strip_prefix("account:"))
                .ok_or_else(|| error("Planner 需要选择 Pi 账号端点"))?;
            let account = crate::agent_runtime::credentials::account(&state.store, id)
                .ok_or_else(|| error("Planner 账号不存在，请重新选择 Pi 账号端点"))?;
            if account["worker_engine"] != "pi" && account["engine"] != "pi" {
                return Err(error("Planner 需要 Pi 账号；请在凭据设置中配置 Pi 账号后重新选择"));
            }
        }
    }
    let net_res = crate::runs::container_exec::project_worker_network(
        config["worker_network"].as_str().unwrap_or("bridge"),
    );
    config["effective_network"] = net_res.effective.map(Value::String).unwrap_or(Value::Null);
    config["network_requested"] = json!(net_res.requested);
    config["network_reason"] = net_res.reason.map(Value::String).unwrap_or(Value::Null);
    config["network_error"] = net_res.error.map(Value::String).unwrap_or(Value::Null);
    config["in_container"] = json!(crate::runs::container_exec::is_running_in_container());
    config["agent_version_policy"] =
        crate::agent_runtime::version_policy::policy_summary_json(&state.store);
    redact(&mut config);
    let source = match state
        .store
        .get("settings", "worker_templates")
        .map_err(storage)?
    {
        Some(source) => source,
        None => state
            .store
            .get("import_metadata", "_worker_config.json")
            .map_err(storage)?
            .unwrap_or_else(|| json!({})),
    };
    let mut stored = config.clone();
    stored.as_object_mut().unwrap().remove("migration_notices");
    stored.as_object_mut().unwrap().remove("worker_resource_effective");
    stored.as_object_mut().unwrap().remove("agent_version_policy");
    for key in [
        "worker_memory",
        "worker_cpus",
        "worker_pids_limit",
        "worker_output_limit",
        "worker_disk_limit",
    ] {
        if !object.contains_key(key) && source.get(key).is_none() {
            stored.as_object_mut().unwrap().remove(key);
        }
    }
    state
        .store
        .put("settings", "worker_templates", &stored)
        .map_err(storage)?;
    Ok(Json(json!({"ok":true,"config":config})))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redact_secrets_masks_credentials_tokens_and_extra_values() {
        let raw = "connect http://alice:super-secret@proxy.local:8080 failed with Bearer abcdef123456 sk-projsecret99 x-api-key: topkey42 rawtok";
        let redacted = redact_secrets(raw, &["rawtok"]);
        assert!(!redacted.contains("super-secret"));
        assert!(!redacted.contains("abcdef123456"));
        assert!(!redacted.contains("projsecret99"));
        assert!(!redacted.contains("topkey42"));
        assert!(!redacted.contains("rawtok"));
        assert!(redacted.contains("http://***:***@proxy.local:8080"));
    }
}
