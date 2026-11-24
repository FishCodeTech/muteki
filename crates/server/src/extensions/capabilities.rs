//! Runtime capability dispatch for enabled task extensions.

use super::{ExtError, Result};
use crate::AppState;
use axum::{Json, Router, extract::State, response::{IntoResponse, Response}, routing::get};
use serde_json::{Value, json};
use std::sync::Arc;

const RECORDS: &str = "task_extensions";

fn token(raw: &str) -> String {
    raw.trim().chars().map(|ch| if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-') { ch } else { '_' })
        .collect::<String>().trim_matches(['.', '_', '-']).to_owned()
}

fn segment(kind: &str) -> String {
    match kind {
        "external-agent-adapter" => "external_agent".to_owned(),
        "platform-adapter" => "platform_adapter".to_owned(),
        "agent-capability-binding" => "capability_binding".to_owned(),
        "tool-integration" => "tool_integration".to_owned(),
        "protocol-binding" => "protocol_binding".to_owned(),
        other => token(other).replace('-', "_"),
    }
}

fn records(state: &AppState) -> Result<Vec<Value>> {
    state.store.list(RECORDS)
        .map_err(|error| ExtError::runtime("extension.store_failed", error.to_string()))
}

fn unavailable(id: &str, version: &str, kind: &str, capability_id: &str, api_version: u64, detail: &str) -> Value {
    json!({"type":kind,"id":capability_id,"api_version":api_version,"state":"unavailable",
        "origin":format!("extension:{id}"),"extension_id":id,"extension_version":version,"detail":detail})
}

pub(super) fn registered_entries(state: &AppState, id: &str, version: &str, manifest: &Value, runtime: &Value) -> Vec<Value> {
    let installed = records(state);
    manifest["provides"].as_array().into_iter().flatten().map(|provide| {
        let kind = provide["type"].as_str().unwrap_or("");
        let capability_id = provide["id"].as_str().unwrap_or("");
        let api_version = provide["api_version"].as_u64().unwrap_or(1);
        let Ok(installed) = installed.as_ref() else {
            return unavailable(id, version, kind, capability_id, api_version, "capability registry is unavailable");
        };
        let matching_runtime = runtime["provides"].as_array().into_iter().flatten()
            .find(|row| row["type"] == kind && row["id"] == capability_id);
        let detail = match matching_runtime {
            None => "running extension did not report this manifest provide",
            Some(row) if row["api_version"].as_u64().unwrap_or(1) != api_version => "runtime api_version does not match manifest",
            Some(_) => "",
        };
        if !detail.is_empty() { return unavailable(id, version, kind, capability_id, api_version, detail); }
        if installed.iter().any(|row| row["extension_id"] != id && row["registry_entries"].as_array()
            .is_some_and(|entries| entries.iter().any(|entry| entry["type"] == kind && entry["id"] == capability_id && entry["state"] == "ready"))) {
            return unavailable(id, version, kind, capability_id, api_version, "capability id is already owned by another extension");
        }
        if kind == "domain-module" && matches!(capability_id,
            "builtin.conversation" | "builtin.single-security-task" | "builtin.pentest" |
            "builtin.competition" | "builtin.src") {
            return unavailable(id, version, kind, capability_id, api_version, "module id is already owned by a built-in module");
        }
        if kind == "platform-adapter" && matches!(capability_id, "ctfd" | "rctf" | "gzctf" | "generic_browser") {
            return unavailable(id, version, kind, capability_id, api_version, "platform-adapter id collides with a built-in platform");
        }
        if kind == "external-agent-adapter" && crate::agent_runtime::descriptors::for_adapter(capability_id).is_some() {
            return unavailable(id, version, kind, capability_id, api_version, "adapter id collides with a built-in adapter");
        }
        if !matches!(kind, "ui-contribution" | "tool-integration" | "domain-module" | "platform-adapter" | "external-agent-adapter") {
            return unavailable(id, version, kind, capability_id, api_version,
                "product capability registry is not connected for this provide type");
        }
        json!({"type":kind,"id":capability_id,"api_version":api_version,"state":"ready",
            "origin":format!("extension:{id}"),"extension_id":id,"extension_version":version,
            "dispatch":if kind == "ui-contribution" {"declarative-ui"} else if kind == "domain-module" {"domain-module.registry"} else {"extension.command"},
            "event_namespaces":if kind == "domain-module" {manifest["permissions"]["events_write"].clone()} else {Value::Null},
            "detail":""})
    }).collect()
}

pub(super) fn set_entry_state(row: &mut Value, state: &str, detail: &str) {
    if let Some(entries) = row["registry_entries"].as_array_mut() {
        for entry in entries {
            entry["state"] = json!(state);
            entry["detail"] = json!(detail);
        }
    }
}

pub(crate) fn registry_rows(state: &AppState) -> Result<Vec<Value>> {
    Ok(records(state)?.into_iter()
        .flat_map(|row| row["registry_entries"].as_array().cloned().unwrap_or_default())
        .collect())
}

pub(crate) fn ready_capabilities(state: &AppState, kind: &str) -> std::result::Result<Vec<Value>, String> {
    Ok(registry_rows(state).map_err(|error| error.message)?
        .into_iter().filter(|entry| entry["type"] == kind && entry["state"] == "ready")
        .collect())
}

pub(crate) async fn invoke_capability(state: &Arc<AppState>, kind: &str,
    capability_id: &str, action: &str, payload: Value) -> std::result::Result<Value, String> {
    let entry = registry_rows(state).map_err(|error| error.message)?
        .into_iter().find(|entry| entry["type"] == kind && entry["id"] == capability_id)
        .ok_or_else(|| format!("extension capability {kind}:{capability_id} is unavailable"))?;
    if entry["state"] != "ready" {
        return Err(format!("extension capability {kind}:{capability_id} is unavailable"));
    }
    let owner = entry["extension_id"].as_str().unwrap_or("");
    let action = token(action);
    if action.is_empty() {
        return Err("extension capability action is required".to_owned());
    }
    let command_type = format!("ext.{owner}.{}.{}.{}", segment(kind), token(capability_id), action);
    invoke(state, &command_type, Some(owner), payload).await.map_err(|error| error.message)
}

pub(crate) fn module_records(state: &AppState) -> Result<Vec<Value>> {
    Ok(records(state)?.into_iter().flat_map(|row| {
        let id = row["extension_id"].as_str().unwrap_or("").to_owned();
        let version = row["active_version"].as_str().unwrap_or("").to_owned();
        row["registry_entries"].as_array().cloned().unwrap_or_default().into_iter()
            .filter(|entry| entry["type"] == "domain-module")
            .map(move |entry| {
                let module_id = entry["id"].as_str().unwrap_or("");
                let route = format!("/settings/extensions?extension={}", id.replace('.', "-").replace('_', "-"));
                let descriptor = json!({"schema_version":1,"id":module_id,"version":"1.0.0",
                    "task_kinds":[format!("extension.{id}")],"required_capabilities":[],
                    "default_executor":"external-agent.single","command_handlers":{},
                    "event_namespaces":entry["event_namespaces"],"api_routes":[],
                    "workspace_kind":format!("extension.{id}"),
                    "ui_contributions":{"title":format!("Extension · {id}"),"description":format!("{id}@{version} 提供的扩展工作区"),
                        "icon":"layers","route":route,"create_entry":route,"aggregate_type":"extension"},
                    "artifact_types":[],"graph_binding":null,"gate_binding":null});
                json!({"schema_version":1,"descriptor":descriptor,"state":entry["state"],
                    "error":if entry["state"] == "ready" { Value::Null } else { json!({"detail":entry["detail"]}) },
                    "components":[],"pending_components":[],"origin":format!("extension:{id}")})
            }).collect::<Vec<_>>()
    }).collect())
}

fn matching_entry(state: &AppState, command_type: &str) -> Result<(Value, Value)> {
    for row in records(state)? {
        let owner = row["extension_id"].as_str().unwrap_or("");
        for entry in row["registry_entries"].as_array().into_iter().flatten() {
            let kind = entry["type"].as_str().unwrap_or("");
            let capability_id = entry["id"].as_str().unwrap_or("");
            let prefix = format!("ext.{owner}.{}.{}.", segment(kind), token(capability_id));
            if let Some(action) = command_type.strip_prefix(&prefix) {
                if action.is_empty() || token(action) != action {
                    return Err(ExtError::validation("extension.command_denied", "extension capability action is invalid"));
                }
                if row["enabled"] != true || row["state"] != "ready" || entry["state"] != "ready" {
                    return Err(ExtError::state("extension.capability_unavailable", "extension capability is not ready"));
                }
                let entry = entry.clone();
                return Ok((row, entry));
            }
        }
    }
    Err(ExtError::not_found(command_type))
}

async fn invoke(state: &AppState, command_type: &str, aggregate_id: Option<&str>, payload: Value) -> Result<Value> {
    if !payload.is_object() {
        return Err(ExtError::validation("extension.invalid_payload", "payload must be an object"));
    }
    let (row, _) = matching_entry(state, command_type)?;
    let owner = row["extension_id"].as_str().unwrap_or("");
    if aggregate_id.is_some_and(|id| id != owner) {
        return Err(ExtError::validation("extension.aggregate_mismatch", "aggregate_id must identify the providing extension"));
    }
    let Some(process) = state.extension_host.get(owner).await else {
        return Err(ExtError::runtime("extension.process_unavailable", "extension process is not running"));
    };
    process.lock().await.rpc("command/handle", json!({"command_type":command_type,"payload":payload})).await
}

pub(crate) async fn dispatch_command(state: &Arc<AppState>, envelope: &Value) -> Option<Response> {
    let command_type = envelope["command_type"].as_str()?;
    if !command_type.starts_with("ext.") { return None; }
    if envelope.get("aggregate_type").is_some_and(|value| value != "extension") {
        return Some(ExtError::validation("extension.aggregate_mismatch", "aggregate_type must be extension").response());
    }
    let result = invoke(state, command_type, envelope["aggregate_id"].as_str(),
        envelope.get("payload").cloned().unwrap_or_else(|| json!({}))).await;
    Some(match result {
        Ok(value) => {
            let id = envelope["command_id"].as_str().filter(|id| !id.is_empty())
                .map(str::to_owned).unwrap_or_else(|| format!("cmd-{}", uuid::Uuid::new_v4().simple()));
            Json(json!({"receipt":{"schema_version":1,"command_id":id,
                "command_type":command_type,"state":"completed",
                "aggregate":{"type":"extension","id":envelope["aggregate_id"]},
                "completed_at":chrono::Utc::now().to_rfc3339()},"result":value})).into_response()
        }
        Err(error) => error.response(),
    })
}

pub(crate) fn dispatch_query(state: &AppState, envelope: &Value) -> Option<Response> {
    let kind = envelope["query_type"].as_str()?;
    let result = match kind {
        "extension.registry" => registry_rows(state).map(|rows| json!({"entries":rows})),
        "extension.capability" => registry_rows(state).map(|rows| {
            let params = &envelope["params"];
            rows.into_iter().find(|row| row["type"] == params["type"] && row["id"] == params["id"])
                .unwrap_or(Value::Null)
        }),
        _ => return None,
    };
    Some(result.map(|value| Json(value).into_response()).unwrap_or_else(|error| error.response()))
}

async fn list(State(state): State<Arc<AppState>>) -> Response {
    registry_rows(&state).map(|rows| Json(json!({"entries":rows})).into_response())
        .unwrap_or_else(|error| error.response())
}

pub(super) fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/extensions/capabilities", get(list))
}
