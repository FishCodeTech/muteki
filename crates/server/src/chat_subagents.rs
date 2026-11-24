//! Muteki-owned child conversation Threads.
use crate::{AppState, chat, runtime_api};
use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
};
use serde_json::{Value, json};
use std::{
    sync::{Arc, OnceLock},
    time::{Duration, Instant},
};
use uuid::Uuid;

type Failure = (StatusCode, Json<Value>);
type Api = Result<Json<Value>, Failure>;
const DEFAULT_WAIT_SECONDS: f64 = 45.0;
const MAX_WAIT_SECONDS: f64 = 1800.0;

#[derive(Clone, Copy)]
struct Limits {
    default_wait_seconds: f64,
    max_wait_seconds: f64,
}

fn configured_limits() -> &'static Limits {
    static LIMITS: OnceLock<Limits> = OnceLock::new();
    LIMITS.get_or_init(|| {
        let number = |name: &str, default: f64| {
            let raw = std::env::var(name).unwrap_or_default();
            if raw.trim().is_empty() {
                default
            } else {
                raw.parse::<f64>()
                    .unwrap_or_else(|_| panic!("{name} must be a number"))
            }
        };
        let default_wait = number("MUTEKI_SUBAGENT_WAIT_SECONDS", DEFAULT_WAIT_SECONDS);
        let max_wait = number("MUTEKI_SUBAGENT_MAX_WAIT_SECONDS", MAX_WAIT_SECONDS);
        assert!(
            default_wait.is_finite()
                && max_wait.is_finite()
                && default_wait > 0.0
                && default_wait <= max_wait,
            "subagent wait limits must satisfy 0 < default <= max"
        );
        Limits {
            default_wait_seconds: default_wait,
            max_wait_seconds: max_wait,
        }
    })
}

fn fail(status: StatusCode, code: &str, message: impl Into<String>) -> Failure {
    let category = match status {
        StatusCode::BAD_REQUEST => "validation",
        StatusCode::NOT_FOUND => "not_found",
        StatusCode::FORBIDDEN => "permission",
        _ => "state",
    };
    (
        status,
        Json(json!({"error":{"code":code,"message":message.into(),"category":category}})),
    )
}
fn storage(error: impl std::fmt::Display) -> Failure {
    fail(
        StatusCode::INTERNAL_SERVER_ERROR,
        "storage.unavailable",
        error.to_string(),
    )
}
fn text<'a>(value: &'a Value, key: &str) -> &'a str {
    value[key].as_str().map(str::trim).unwrap_or("")
}
fn effective_limits(_s: &AppState) -> Limits { *configured_limits() }
fn limits(s: &AppState) -> Value {
    let limits = effective_limits(s);
    json!({"default_wait_seconds":limits.default_wait_seconds,"max_wait_seconds":limits.max_wait_seconds})
}
fn thread(s: &AppState, id: &str) -> Result<Value, Failure> {
    chat::load(s, id)
}
fn lineage(doc: &Value) -> &Value {
    &doc["thread"]["state"]["lineage"]
}
fn direct_children(doc: &Value) -> Vec<String> {
    doc["thread"]["state"]["subagents"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|node| {
            node["agent_id"]
                .as_str()
                .or_else(|| node["thread_id"].as_str())
        })
        .map(str::to_owned)
        .collect()
}
fn descendants(s: &AppState, ancestor_id: &str) -> Result<Vec<String>, Failure> {
    let docs = s.store.list(chat::COLLECTION).map_err(storage)?;
    let mut by_parent = std::collections::HashMap::<String, Vec<String>>::new();
    for doc in &docs {
        let tid = text(&doc["thread"], "thread_id");
        if tid.is_empty() {
            continue;
        }
        let parent = text(lineage(doc), "parent_thread_id");
        if !parent.is_empty() {
            let entry = by_parent.entry(parent.to_owned()).or_default();
            if !entry.iter().any(|id| id == tid) {
                entry.push(tid.to_owned());
            }
        }
        for child in direct_children(doc) {
            let entry = by_parent.entry(tid.to_owned()).or_default();
            if !entry.iter().any(|id| id == &child) {
                entry.push(child);
            }
        }
    }
    let mut ordered = Vec::new();
    let mut frontier = vec![ancestor_id.to_owned()];
    let mut seen = std::collections::HashSet::new();
    seen.insert(ancestor_id.to_owned());
    loop {
        let mut next_frontier = Vec::new();
        for parent in &frontier {
            if let Some(children) = by_parent.get(parent) {
                for child in children {
                    if seen.insert(child.clone()) {
                        ordered.push(child.clone());
                        next_frontier.push(child.clone());
                    }
                }
            }
        }
        if next_frontier.is_empty() {
            break;
        }
        frontier = next_frontier;
    }
    Ok(ordered)
}
/// Whether `thread_id` was started, directly or through its own subagents,
/// by `ancestor_id`.
pub fn is_descendant_of(s: &AppState, thread_id: &str, ancestor_id: &str) -> bool {
    is_descendant(s, thread_id, ancestor_id)
}

fn is_descendant(s: &AppState, child_id: &str, ancestor_id: &str) -> bool {
    let mut current = child_id.to_owned();
    let mut seen = std::collections::HashSet::new();
    while seen.insert(current.clone()) {
        let Ok(doc) = thread(s, &current) else {
            break;
        };
        let parent = text(lineage(&doc), "parent_thread_id");
        if parent == ancestor_id {
            return true;
        }
        if parent.is_empty() {
            break;
        }
        current = parent.to_owned();
    }
    descendants(s, ancestor_id).is_ok_and(|list| list.iter().any(|id| id == child_id))
}
fn parent_node<'a>(parent: &'a Value, child_id: &str) -> Option<&'a Value> {
    parent["thread"]["state"]["subagents"]
        .as_array()?
        .iter()
        .find(|node| node["agent_id"] == child_id)
}
fn parent_node_mut<'a>(parent: &'a mut Value, child_id: &str) -> Option<&'a mut Value> {
    parent["thread"]["state"]["subagents"]
        .as_array_mut()?
        .iter_mut()
        .find(|node| node["agent_id"] == child_id)
}
fn ensure_parent_can_delegate(
    s: &AppState,
    caller_id: &str,
    caller: &Value,
) -> Result<(), Failure> {
    let ancestor_id = text(lineage(caller), "parent_thread_id");
    if ancestor_id.is_empty() {
        return Ok(());
    }
    let ancestor = thread(s, ancestor_id)?;
    if parent_node(&ancestor, caller_id).is_some_and(|node| node["cancel_requested"] == true) {
        return Err(fail(
            StatusCode::CONFLICT,
            "conversation.subagent.parent_cancelled",
            "The calling subagent has been cancelled",
        ));
    }
    Ok(())
}
fn terminal(status: &str) -> bool {
    matches!(status, "completed" | "failed" | "cancelled" | "interrupted")
}
fn live_status(child: &Value, node: &Value) -> (&'static str, Value, Value, Value) {
    let cancel = node["cancel_requested"] == true;
    let last = child["turns"]
        .as_array()
        .and_then(|rows| rows.iter().rev().find(|row| row["status"] != "superseded"));
    if let Some(last) = last
        && last["status"] == "running"
    {
        return ("running", last["turn_id"].clone(), Value::Null, Value::Null);
    }
    if let Some(queue) = child["queue"].as_array()
        && !queue.is_empty()
    {
        if cancel && !queue.iter().any(|item| item["status"] == "dispatching") {
            return ("cancelled", Value::Null, Value::Null, Value::Null);
        }
        if let Some(failed) = queue.iter().find(|item| item["status"] == "failed") {
            return (
                "failed",
                Value::Null,
                Value::Null,
                json!({"code":failed["error"]["code"].as_str().unwrap_or("conversation.queue.dispatch_failed"),
                    "message":failed["error"]["message"].as_str().unwrap_or("")}),
            );
        }
        return ("pending", Value::Null, Value::Null, Value::Null);
    }
    let Some(last) = last else {
        if let Some(status) = node["status"].as_str()
            && terminal(status)
        {
            let state = match status {
                "completed" => "completed",
                "failed" => "failed",
                "cancelled" => "cancelled",
                _ => "interrupted",
            };
            let error = if node["error_code"].is_string() {
                json!({"code":node["error_code"],"message":node["error"]})
            } else {
                Value::Null
            };
            return (state, Value::Null, Value::Null, error);
        }
        if cancel {
            return ("cancelled", Value::Null, Value::Null, Value::Null);
        }
        return ("pending", Value::Null, Value::Null, Value::Null);
    };
    let turn_id = last["turn_id"].clone();
    let message_id = child["messages"]
        .as_array()
        .into_iter()
        .flatten()
        .rev()
        .find(|message| message["role"] == "assistant" && message["turn_id"] == turn_id)
        .map(|message| message["message_id"].clone())
        .unwrap_or(Value::Null);
    match last["status"].as_str().unwrap_or("") {
        "completed" => ("completed", turn_id, message_id, Value::Null),
        "failed" => ("failed", turn_id, message_id, last["error"].clone()),
        "cancelled" | "interrupted" => (
            if cancel { "cancelled" } else { "interrupted" },
            turn_id,
            message_id,
            Value::Null,
        ),
        "queued" => ("pending", turn_id, message_id, Value::Null),
        _ => ("pending", turn_id, message_id, Value::Null),
    }
}

fn runtime_view(runtime: &Value) -> Value {
    let mut out = json!({});
    for key in [
        "adapter_id",
        "instance_id",
        "credential_id",
        "model",
        "effort",
        "service_tier",
        "access_mode",
    ] {
        if let Some(value) = runtime.get(key) {
            out[key] = value.clone();
        }
    }
    out
}

pub fn snapshot(s: &AppState, child_id: &str, include_text: bool) -> Result<Value, Failure> {
    let child = thread(s, child_id)?;
    let line = lineage(&child);
    let parent_id = text(line, "parent_thread_id");
    if parent_id.is_empty() {
        return Err(fail(
            StatusCode::NOT_FOUND,
            "conversation.subagent.not_found",
            "Subagent lineage is missing",
        ));
    }
    let parent = thread(s, parent_id)?;
    let node = parent_node(&parent, child_id)
        .cloned()
        .unwrap_or(Value::Null);
    let (status, turn_id, message_id, error) = live_status(&child, &node);
    let final_message = if terminal(status) && !message_id.is_null() {
        child["messages"].as_array().into_iter().flatten()
            .find(|message| message["message_id"] == message_id)
            .map(|message| {
                let body = text(message, "text");
                let mut result = json!({"message_id":message_id,"thread_id":child_id,
                    "turn_id":message["turn_id"],"chars":body.chars().count(),"complete":status == "completed"});
                if include_text { result["text"] = json!(body); }
                result
            }).unwrap_or(Value::Null)
    } else {
        Value::Null
    };
    Ok(json!({
        "subagent_id":child_id,"thread_id":child_id,"conversation_url":format!("/chat/{child_id}"),
        "origin":"app_owned","title":child["thread"]["title"],"status":status,"child_turn_id":turn_id,
        "parent_thread_id":parent_id,"parent_turn_id":line["parent_turn_id"],
        "root_thread_id":line["root_thread_id"],"depth":line["depth"],
        "runtime":runtime_view(&child["runtime"]),"isolation":line["isolation"],
        "workspace_id":line["workspace_id"],"worktree_path":line["worktree_path"],
        "worktree_state":node["worktree_state"],"cancel_requested":node["cancel_requested"] == true,
        "activity":node["activity"],"acceptance_criteria":node["acceptance_criteria"],
        "todo_id":node["todo_id"],"delegation_profile":node["delegation_profile"],
        "routing":node["routing"],"routing_reason":node["routing_reason"],"final_message":final_message,
        "error":error,"child_subagent_ids":direct_children(&child),
        "report": child["thread"]["state"]["subagent_report"]["state"],
        "delivery": crate::control_subagents::delivery_with_cancel(&child, status, node["cancel_requested"] == true),
        "execution": crate::control_subagents::execution(&child, status),
        "attention": crate::control_subagents::waiting_for(&child),
    }))
}

fn active_in_root(s: &AppState, root: &str) -> Result<usize, Failure> {
    let mut count = 0;
    for doc in s.store.list(chat::COLLECTION).map_err(storage)? {
        if lineage(&doc)["root_thread_id"] != root {
            continue;
        }
        let id = text(&doc["thread"], "thread_id");
        if id.is_empty() {
            continue;
        }
        let parent = s
            .store
            .get(chat::COLLECTION, text(lineage(&doc), "parent_thread_id"))
            .map_err(storage)?;
        let node = parent
            .as_ref()
            .and_then(|row| parent_node(row, id))
            .cloned()
            .unwrap_or(Value::Null);
        let (status, _, _, _) = live_status(&doc, &node);
        if matches!(status, "pending" | "running") {
            count += 1;
        }
    }
    Ok(count)
}

async fn wait_for_terminal(
    s: &Arc<AppState>,
    child_id: &str,
    seconds: f64,
) -> Result<(Value, bool, f64), Failure> {
    let start = Instant::now();
    let mut updates = s.chat.subscribe();
    loop {
        let current = snapshot(s, child_id, true)?;
        let waited = start.elapsed().as_secs_f64();
        if terminal(text(&current, "status")) {
            return Ok((current, true, waited));
        }
        if waited >= seconds {
            return Ok((current, false, waited));
        }
        let remaining = Duration::from_secs_f64((seconds - waited).min(5.0));
        let _ = tokio::time::timeout(remaining, updates.recv()).await;
    }
}

fn wait_options(payload: &Value) -> Result<(bool, f64), Failure> {
    let wait = match payload.get("wait") {
        None | Some(Value::Null) => false,
        Some(Value::Bool(wait)) => *wait,
        _ => {
            return Err(fail(
                StatusCode::BAD_REQUEST,
                "conversation.subagent.argument_invalid",
                "wait must be a boolean",
            ));
        }
    };
    let seconds = match payload.get("timeout_seconds") {
        None | Some(Value::Null) => configured_limits().default_wait_seconds,
        Some(Value::Number(number)) => number.as_f64().unwrap_or(0.0),
        _ => 0.0,
    };
    let max_wait = configured_limits().max_wait_seconds;
    if !(seconds > 0.0 && seconds <= max_wait) {
        return Err(fail(
            StatusCode::BAD_REQUEST,
            "conversation.subagent.timeout_invalid",
            format!("timeout_seconds must be within (0, {max_wait}]"),
        ));
    }
    Ok((wait, seconds))
}

pub fn supports_query(kind: &str) -> bool {
    matches!(
        kind,
        "conversation.subagent.status" | "conversation.subagent.runtimes"
    )
}
pub fn tool_query_type(name: &str) -> Option<&'static str> {
    match name {
        "muteki_get_subagents" => Some("conversation.subagent.status"),
        "muteki_list_providers" | "muteki_list_runtimes" => Some("conversation.subagent.runtimes"),
        _ => None,
    }
}
pub fn tool_command_type(name: &str) -> Option<&'static str> {
    match name {
        "muteki_start_thread" => Some("conversation.subagent.spawn"),
        "muteki_send_message" => Some("conversation.subagent.send"),
        "muteki_cancel_thread" => Some("conversation.subagent.cancel"),
        _ => None,
    }
}

pub async fn query(
    s: &Arc<AppState>,
    parent_id: &str,
    kind: &str,
    params: &Value,
) -> Result<Value, Failure> {
    let parent = thread(s, parent_id)?;
    if !matches!(parent["thread"]["mode"].as_str(), Some("conversation" | "src")) {
        return Err(fail(
            StatusCode::FORBIDDEN,
            "conversation.subagent.mode_invalid",
            "A conversation Thread is required",
        ));
    }
    match kind {
        "conversation.subagent.status" => {
            let child_id = text(params, "subagent_id");
            if params
                .get("subagent_id")
                .is_some_and(|value| !value.is_null() && !value.is_string())
            {
                return Err(fail(
                    StatusCode::NOT_FOUND,
                    "conversation.subagent.not_found",
                    "subagent_id must identify a child Thread",
                ));
            }
            if params
                .get("subagent_id")
                .is_some_and(|value| value.is_string() && text(params, "subagent_id").is_empty())
            {
                return Err(fail(
                    StatusCode::NOT_FOUND,
                    "conversation.subagent.not_found",
                    "subagent_id must identify a child Thread",
                ));
            }
            let wait = match params.get("wait_seconds") {
                None | Some(Value::Null) => 0.0,
                Some(Value::Number(number)) => number.as_f64().unwrap_or(f64::NAN),
                _ => f64::NAN,
            };
            if !(0.0..=configured_limits().max_wait_seconds).contains(&wait) {
                return Err(fail(
                    StatusCode::BAD_REQUEST,
                    "conversation.subagent.timeout_invalid",
                    "wait_seconds is out of range",
                ));
            }
            if child_id.is_empty() {
                if wait > 0.0 {
                    return Err(fail(
                        StatusCode::BAD_REQUEST,
                        "conversation.subagent.argument_invalid",
                        "wait_seconds requires subagent_id",
                    ));
                }
                let children = direct_children(&parent);
                let rows = children
                    .iter()
                    .map(|id| snapshot(s, id, false))
                    .collect::<Result<Vec<_>, _>>()?;
                let root = text(lineage(&parent), "root_thread_id");
                let root = if root.is_empty() { parent_id } else { root };
                Ok(
                    json!({"thread_id":parent_id,"subagents":rows,"count":rows.len(),
                    "root_thread_id":root,"active_in_root":active_in_root(s,root)?,"limits":limits(s)}),
                )
            } else {
                if !is_descendant(s, child_id, parent_id) {
                    return Err(fail(
                        StatusCode::NOT_FOUND,
                        "conversation.subagent.not_found",
                        "Subagent does not belong to this Thread",
                    ));
                }
                if wait > 0.0 {
                    let (row, finished, waited) = wait_for_terminal(s, child_id, wait).await?;
                    Ok(
                        json!({"subagent":row,"wait":{"requested":true,"outcome":if finished {"finished"} else {"pending"},
                        "timeout_seconds":wait,"waited_seconds":waited}}),
                    )
                } else {
                    Ok(json!({"subagent":snapshot(s,child_id,true)?}))
                }
            }
        }
        "conversation.subagent.runtimes" => runtimes(s, &parent, params).await,
        _ => Err(fail(
            StatusCode::NOT_IMPLEMENTED,
            "capability.unavailable",
            "Unknown subagent query",
        )),
    }
}

pub async fn list_runtime_inventory(s: &Arc<AppState>, params: &Value) -> Result<Value, Failure> {
    let empty = json!({});
    runtimes(s, &empty, params).await
}

async fn runtimes(s: &Arc<AppState>, parent: &Value, params: &Value) -> Result<Value, Failure> {
    if params.get("engine").is_some_and(|value| !value.is_string()) {
        return Err(fail(
            StatusCode::BAD_REQUEST,
            "conversation.subagent.argument_invalid",
            "engine must be a string",
        ));
    }
    let selected = text(params, "engine").to_ascii_lowercase();
    if !selected.is_empty() && crate::agent_runtime::descriptors::find(&selected).is_none() {
        return Err(fail(
            StatusCode::BAD_REQUEST,
            "conversation.subagent.runtime_invalid",
            format!("unknown engine {selected}"),
        ));
    }
    let instances = runtime_api::list_views(s, "", true, "structured")?;
    let saved_catalogs = s.store.list("model_catalogs").map_err(storage)?;
    let mut accounts = Vec::new();
    for account in s.store.list("credential_accounts").map_err(storage)? {
        let id = text(&account, "account_id");
        let id = if id.is_empty() {
            text(&account, "id")
        } else {
            id
        };
        if id.is_empty() {
            continue;
        }
        let engine = text(&account["details"], "target_engine");
        let engine = if engine.is_empty() {
            text(&account, "worker_engine")
        } else {
            engine
        };
        let engine = if engine.is_empty() {
            text(&account, "engine")
        } else {
            engine
        };
        let mut row = account.clone();
        row["engine"] = json!(engine);
        row["credential_id"] = json!(format!("account:{id}"));
        accounts.push(row);
    }
    accounts
        .extend(crate::native_credentials::system_rows(&s.store, &Default::default(), false).await);
    let mut rows = Vec::new();
    for instance in instances {
        let engine = text(&instance, "engine");
        if !selected.is_empty() && selected != engine {
            continue;
        }
        let allowed = instance["access_modes"].as_array().cloned().unwrap_or_default();
        for account in accounts
            .iter()
            .filter(|account| account["engine"] == engine)
        {
            let credential_id = text(account, "credential_id");
            let runtime_key = text(&instance, "key");
            let stored = saved_catalogs.iter().find(|catalog| {
                catalog["credential_id"] == credential_id
                    && catalog["runtime_instance"] == runtime_key
            });
            let catalog = stored.unwrap_or_else(|| {
                if account["model_catalog"].is_object() {
                    &account["model_catalog"]
                } else {
                    &account["model_catalogs"][runtime_key]
                }
            });
            let mut models = Vec::<Value>::new();
            for field in ["discovered_models", "configured_models", "verified_models"] {
                for model in catalog[field].as_array().into_iter().flatten() {
                    add_model(&mut models, model, catalog);
                }
            }
            for field in ["models", "candidate_models"] {
                for model in account[field].as_array().into_iter().flatten() {
                    add_model(&mut models, model, catalog);
                }
            }
            let mut reasons = Vec::new();
            if instance["enabled"] != true {
                reasons.push("Runtime 未启用或尚未通过探测");
            }
            if account["present"] != true
                || matches!(account["status"].as_str(), Some("failed" | "missing"))
            {
                reasons.push("凭据不可用，请在设置中检查登录或测试结果");
            }
            if models.is_empty() {
                reasons.push("没有该凭据和 Runtime 的模型目录，请在设置中刷新");
            }
            if allowed.is_empty() {
                reasons.push("此 Runtime 未提供访问模式");
            }
            rows.push(json!({"engine":engine,"adapter_id":instance["adapter_id"],"instance_id":instance["instance_id"],
                "credential_id":account["credential_id"],"available":reasons.is_empty(),"reason":reasons.join("；"),
                "models":models,"default_model":catalog["default_model"].as_str().filter(|v|!v.is_empty()).unwrap_or(text(account,"default_model")),
                "access_modes":allowed,"catalog_status":catalog["refresh_status"],"catalog_error_code":catalog["error_code"]}));
        }
    }
    let providers: Vec<Value> = rows.iter().map(|row| {
        let mut provider = row.clone();
        provider["name"] = row["engine"].clone();
        provider["profile"] = row["credential_id"].clone();
        provider
    }).collect();
    Ok(
        json!({"thread_id":parent["thread"]["thread_id"],"current_runtime":runtime_view(&parent["runtime"]),
        "providers":providers,"runtimes":rows,"limits":limits(s),"note":"目录与实际调用结果不同；未标记 verified 的模型尚未验证。成本未知时不要猜测价格。"}),
    )
}

fn add_model(models: &mut Vec<Value>, model: &Value, catalog: &Value) {
    let id = model.as_str().unwrap_or_else(|| text(model, "id"));
    if id.is_empty() || models.iter().any(|row| row["id"] == id) {
        return;
    }
    let verified = catalog["verified_models"]
        .as_array()
        .is_some_and(|rows| rows.iter().any(|row| row == id));
    let metadata = catalog["discovered_models"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|row| row["id"] == id);
    let mut row = json!({"id":id,"verified":verified});
    if let Some(reasoning) = metadata
        .and_then(|row| row.get("reasoning"))
        .or_else(|| model.get("reasoning"))
    {
        row["reasoning"] = reasoning.clone();
    }
    models.push(row);
}

fn patch_node(
    s: &AppState,
    parent_id: &str,
    child_id: &str,
    patch: &Value,
    event_type: &str,
) -> Result<(), Failure> {
    let mut parent = thread(s, parent_id)?;
    let before = parent.clone();
    let node = parent_node_mut(&mut parent, child_id).ok_or_else(|| {
        fail(
            StatusCode::NOT_FOUND,
            "conversation.subagent.not_found",
            "Parent subagent node is missing",
        )
    })?;
    for (key, value) in patch.as_object().into_iter().flatten() {
        if event_type == "core.subagent.cancel_requested"
            && matches!(
                key.as_str(),
                "reason" | "requested_by_thread_id" | "cascade_from"
            )
        {
            continue;
        }
        node[key] = value.clone();
    }
    let mut payload = patch.clone();
    payload["subagent_id"] = json!(child_id);
    chat::emit(s, parent_id, &mut parent, event_type, payload);
    let (events, watermark) = s
        .store
        .chat_commit_checked(parent_id, &parent, Some(&before), None)
        .map_err(storage)?
        .ok_or_else(|| {
            fail(
                StatusCode::CONFLICT,
                "conversation.subagent.concurrent_update",
                "Parent Thread changed while updating its subagent",
            )
        })?;
    chat::finish_save(s, parent_id, &mut parent, events, watermark);
    Ok(())
}

pub fn sync_node(s: &AppState, child_id: &str) -> Result<(), Failure> {
    for _ in 0..8 {
        match sync_node_once(s, child_id) {
            Err((StatusCode::CONFLICT, Json(error)))
                if error["error"]["code"] == "conversation.subagent.concurrent_update" =>
            {
                continue;
            }
            result => return result,
        }
    }
    Err(fail(
        StatusCode::CONFLICT,
        "conversation.subagent.concurrent_update",
        "Parent Thread kept changing while updating its subagent",
    ))
}

fn sync_node_once(s: &AppState, child_id: &str) -> Result<(), Failure> {
    let child = thread(s, child_id)?;
    let parent_id = text(lineage(&child), "parent_thread_id");
    if parent_id.is_empty() {
        return Ok(());
    }
    let parent = thread(s, parent_id)?;
    let Some(node) = parent_node(&parent, child_id) else {
        return Ok(());
    };
    let (status, turn_id, message_id, error) = live_status(&child, node);
    let mut patch = json!({});
    let result = if let Some(report) = child["thread"]["state"]["subagent_report"]["last_report"]["text"].as_str() {
        json!(report)
    } else if message_id.is_null() {
        Value::Null
    } else {
        child["messages"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|message| message["message_id"] == message_id)
            .map(|message| message["text"].clone())
            .unwrap_or(Value::Null)
    };
    let mut activity = match status {
        "running" => "正在执行",
        "pending" => "等待执行",
        "completed" => "执行结束，结果仍需主代理独立验收",
        _ => "",
    };
    let state = &child["thread"]["state"];
    if state["pending_approvals"]
        .as_object()
        .is_some_and(|rows| !rows.is_empty())
        || state["pending_approval"]
            .as_object()
            .is_some_and(|row| !row.is_empty())
    {
        activity = "等待你的审批，请打开子会话处理";
    } else if state["pending_user_input"]
        .as_object()
        .is_some_and(|row| !row.is_empty())
    {
        activity = "等待你的回答，请打开子会话处理";
    }
    let same_terminal_turn = terminal(status) && node["child_turn_id"] == turn_id;
    let error_message = if same_terminal_turn && error.is_null() {
        node["error"].clone()
    } else {
        error["message"].clone()
    };
    let error_code = if same_terminal_turn && error.is_null() {
        node["error_code"].clone()
    } else {
        error["code"].clone()
    };
    let cancel_requested = node["cancel_requested"] == true;
    let blocker_text = error_message
        .as_str()
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(str::to_owned);
    let report_back_text = result
        .as_str()
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(str::to_owned);
    let fields = [
        ("status", json!(status)),
        ("report", child["thread"]["state"]["subagent_report"]["state"].clone()),
        ("delivery", json!(crate::control_subagents::delivery_with_cancel(&child, status, cancel_requested))),
        ("execution", json!(crate::control_subagents::execution(&child, status))),
        ("attention", json!(crate::control_subagents::waiting_for(&child))),
        ("child_turn_id", turn_id.clone()),
        ("message_id", message_id),
        ("result", result),
        ("error", error_message),
        ("error_code", error_code),
        (
            "activity",
            if activity.is_empty() {
                Value::Null
            } else {
                json!(activity)
            },
        ),
    ];
    for (key, value) in fields {
        if node[key] != value {
            patch[key] = value;
        }
    }
    if let Some(turn) = child["turns"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|turn| turn["turn_id"] == turn_id)
    {
        for (field, source) in [
            ("started_at", "created_at"),
            ("completed_at", "completed_at"),
        ] {
            if node[field] != turn[source] {
                patch[field] = turn[source].clone();
            }
        }
        let runtime = &turn["runtime_snapshot"];
        for field in ["adapter_id", "model", "effort", "access_mode"] {
            if !runtime[field].is_null() && node[field] != runtime[field] {
                patch[field] = runtime[field].clone();
            }
        }
    } else if matches!(status, "pending" | "running") && !node["completed_at"].is_null() {
        patch["completed_at"] = Value::Null;
    }
    let mut patched = false;
    if patch.as_object().is_some_and(|fields| !fields.is_empty()) {
        patch_node(s, parent_id, child_id, &patch, "core.subagent.updated")?;
        patched = true;
    }
    if patched && child["thread"].get("src_delegation").is_some_and(Value::is_object) {
        let wait = crate::control_subagents::waiting_for(&child);
        let exec = crate::control_subagents::execution(&child, status);
        let todo_state = match (exec, wait) {
            ("waiting", Some("answer")) => "waiting_input",
            ("waiting", _) => "waiting_approval",
            ("working" | "background", _) => "running",
            ("finished", _) => "done",
            ("failed" | "cancelled" | "interrupted", _) => "blocked",
            _ => exec,
        };
        let _ = crate::runs::src_api::sync_todo_from_subthread(
            s,
            child_id,
            todo_state,
            wait,
            blocker_text.as_deref(),
            report_back_text.as_deref(),
        );
    }
    Ok(())
}

fn child_runtime(s: &AppState, parent: &Value, payload: &Value, workspace_id: &Value) -> Result<Value, Failure> {
    let current = &parent["runtime"];
    let mut runtime = current.clone();
    for key in [
        "engine",
        "adapter_id",
        "instance_id",
        "credential_id",
        "model",
        "effort",
        "access_mode",
    ] {
        if payload
            .get(key)
            .is_some_and(|value| !value.is_null() && !value.is_string())
        {
            return Err(fail(
                StatusCode::BAD_REQUEST,
                "conversation.subagent.argument_invalid",
                format!("{key} must be a string"),
            ));
        }
    }
    let engine = text(payload, "engine").to_ascii_lowercase();
    let adapter = text(payload, "adapter_id");
    let inherited_adapter = text(payload, "inherited_adapter_id");
    let inherited_instance = text(payload, "inherited_instance_id");
    let selected_adapter = if engine.is_empty() {
        if !adapter.is_empty() {
            adapter.to_owned()
        } else {
            inherited_adapter.to_owned()
        }
    } else {
        let descriptor = crate::agent_runtime::descriptors::find(&engine).ok_or_else(|| {
            fail(
                StatusCode::BAD_REQUEST,
                "conversation.subagent.runtime_invalid",
                format!("unknown engine {engine}"),
            )
        })?;
        if !adapter.is_empty() && descriptor.adapter(adapter).is_none() {
            return Err(fail(
                StatusCode::BAD_REQUEST,
                "conversation.subagent.runtime_invalid",
                format!("adapter_id {adapter} does not belong to engine {engine}"),
            ));
        }
        if !adapter.is_empty() {
            adapter.to_owned()
        } else if !inherited_adapter.is_empty() && descriptor.adapter(inherited_adapter).is_some() {
            inherited_adapter.to_owned()
        } else {
            descriptor.identity.default_adapter_id.clone()
        }
    };
    let selected_adapter = if selected_adapter.is_empty() {
        text(current, "adapter_id").to_owned()
    } else {
        selected_adapter
    };
    if selected_adapter.is_empty() {
        return Err(fail(
            StatusCode::BAD_REQUEST,
            "conversation.subagent.runtime_invalid",
            "The parent Thread has no Runtime selection",
        ));
    }
    let parent_engine = crate::agent_runtime::descriptors::for_adapter(text(current, "adapter_id"))
        .map(|(engine, _)| engine.engine().to_owned())
        .unwrap_or_default();
    let child_engine = crate::agent_runtime::descriptors::for_adapter(&selected_adapter)
        .map(|(engine, _)| engine.engine().to_owned())
        .unwrap_or_default();
    let same_engine = selected_adapter == text(current, "adapter_id")
        || (!parent_engine.is_empty() && parent_engine == child_engine);
    let same_adapter = selected_adapter == text(current, "adapter_id");
    if payload["provider"].is_string() && !same_engine {
        runtime["instance_id"] = json!("default");
        runtime["credential_id"] = json!(format!("system:{child_engine}"));
        runtime["model"] = Value::Null;
        runtime["effort"] = Value::Null;
        runtime["service_tier"] = Value::Null;
        runtime["options"] = json!({});
    }
    runtime["adapter_id"] = json!(selected_adapter);
    if !same_adapter {
        runtime["instance_id"] = json!("default");
        runtime["service_tier"] = json!("");
    }
    if !same_engine {
        runtime["credential_id"] = json!("");
        runtime["model"] = json!("");
        runtime["effort"] = json!("");
    }
    for key in [
        "instance_id",
        "credential_id",
        "model",
        "effort",
        "access_mode",
    ] {
        if let Some(value) = payload.get(key) {
            if let Some(value) = value.as_str()
                && !value.trim().is_empty()
            {
                runtime[key] = json!(value.trim());
            }
        }
    }
    if text(payload, "instance_id").is_empty() && !inherited_instance.is_empty() && same_adapter {
        runtime["instance_id"] = json!(inherited_instance);
    }
    if let Some(options) = payload.get("options") { runtime["options"] = options.clone(); }
    if let Some(cwd) = payload["cwd"].as_str() {
        let workspace = s.store.get("workspaces", workspace_id.as_str().unwrap_or("")).map_err(storage)?
            .ok_or_else(|| fail(StatusCode::NOT_FOUND, "conversation.subagent.project_missing", "The target project has no registered checkout"))?;
        let root = crate::workspace_read_api::canonical_workspace_root(text(&workspace, "root_path"))
            .map_err(|(_, body)| fail(StatusCode::BAD_REQUEST, "conversation.subagent.cwd_invalid", body.to_string()))?;
        let relative = std::path::Path::new(cwd);
        if relative.is_absolute() || relative.components().any(|part| matches!(part, std::path::Component::ParentDir)) {
            return Err(fail(StatusCode::BAD_REQUEST, "conversation.subagent.cwd_invalid", "cwd must be relative to the project root"));
        }
        let selected = root.join(relative).canonicalize().map_err(storage)?;
        if !selected.is_dir() || !selected.starts_with(&root) {
            return Err(fail(StatusCode::BAD_REQUEST, "conversation.subagent.cwd_invalid", "cwd must be a directory within the project"));
        }
        if payload["isolation"] == "worktree" && selected != root {
            return Err(fail(StatusCode::BAD_REQUEST, "conversation.subagent.cwd_invalid", "a dedicated worktree requires a repository root"));
        }
        if payload["isolation"] != "worktree" { runtime["cwd"] = json!(selected); }
    }
    let same_model = same_engine && text(&runtime, "model") == text(current, "model");
    if !same_model {
        runtime["service_tier"] = json!("");
        if text(payload, "effort").is_empty() {
            runtime["effort"] = json!("");
        }
    }
    let policy = crate::control::settings(s)?.subagent_policy;
    let mode = payload["access_mode"].as_str().unwrap_or_else(|| {
        let parent_mode = text(current, "access_mode");
        if parent["thread"]["mode"] == "src" && !parent_mode.is_empty() {
            parent_mode
        } else if policy.approval_mode == "always_allow" {
            "full-access"
        } else {
            "supervised"
        }
    });
    runtime["access_mode"] = json!(mode);
    let admitted = crate::agent_runtime::pool::admit(&selected_adapter, mode).map_err(|e| fail(StatusCode::BAD_REQUEST,"conversation.subagent.runtime_invalid",e.to_string()))?;
    if let Some(agent::OptionDescriptor::Select { id, default_value, recommended, permissive, .. }) = agent::permission_control(admitted.provider) {
        let selected = match policy.approval_mode.as_str() {
            "auto" => recommended.or(default_value),
            "always_allow" => permissive.or(default_value),
            _ => default_value,
        };
        if payload.get("access_mode").is_none() && payload["options"].get(&id).is_none() && let Some(value) = selected {
            if !runtime["options"].is_object() { runtime["options"] = json!({}); }
            runtime["options"][id] = json!(value);
        }
    }
    chat::resolve_runtime(s, &runtime)?;
    Ok(runtime)
}

async fn spawn(
    s: Arc<AppState>,
    parent_id: String,
    body: Value,
    expected: Option<Option<u64>>,
) -> Api {
    let command_id = text(&body, "command_id").to_owned();
    if command_id.is_empty() {
        return Err(fail(
            StatusCode::BAD_REQUEST,
            "conversation.subagent.argument_invalid",
            "command_id is required",
        ));
    }
    let key = format!("thread:{parent_id}:{command_id}");
    let payload = &body["payload"];
    if !payload.is_object() {
        return Err(fail(
            StatusCode::BAD_REQUEST,
            "conversation.subagent.argument_invalid",
            "payload must be an object",
        ));
    }
    let prompt = payload["prompt"].as_str().unwrap_or("");
    if prompt.trim().is_empty() && payload["attachments"].as_array().is_none_or(Vec::is_empty) {
        return Err(fail(
            StatusCode::BAD_REQUEST,
            "conversation.subagent.argument_invalid",
            "prompt must be a non-empty string",
        ));
    }
    if payload
        .get("title")
        .is_some_and(|value| !value.is_null() && !value.is_string())
    {
        return Err(fail(
            StatusCode::BAD_REQUEST,
            "conversation.subagent.argument_invalid",
            "title must be a string",
        ));
    }
    let title = text(payload, "title");
    if payload
        .get("isolation")
        .is_some_and(|value| !value.is_string())
    {
        return Err(fail(
            StatusCode::BAD_REQUEST,
            "conversation.subagent.argument_invalid",
            "isolation must be shared or worktree",
        ));
    }
    let isolation = payload["isolation"].as_str().unwrap_or("shared");
    if !matches!(isolation, "shared" | "worktree") {
        return Err(fail(
            StatusCode::BAD_REQUEST,
            "conversation.subagent.argument_invalid",
            "isolation must be shared or worktree",
        ));
    }
    let (wait, timeout) = wait_options(payload)?;
    for field in ["acceptance_criteria", "routing_reason"] {
        if let Some(value) = payload.get(field)
            && value.as_str().is_none_or(|value| value.trim().is_empty())
        {
            return Err(fail(
                StatusCode::BAD_REQUEST,
                "conversation.subagent.argument_invalid",
                format!("{field} must be a non-empty string"),
            ));
        }
    }
    let guard = s.chat.active.lock().await;
    if let Some(prior) = s
        .store
        .get("conversation_commands", &key)
        .map_err(storage)?
    {
        if prior["request"] != body {
            return Err(fail(
                StatusCode::CONFLICT,
                "command.idempotency_conflict",
                "command_id was reused with different content",
            ));
        }
        return Ok(Json(prior["response"].clone()));
    }
    let mut parent = thread(&s, &parent_id)?;
    if !matches!(parent["thread"]["mode"].as_str(), Some("conversation" | "src")) {
        return Err(fail(
            StatusCode::FORBIDDEN,
            "conversation.subagent.mode_invalid",
            "Subagents require a conversation Thread",
        ));
    }
    if parent["thread"]["archived"] == true {
        return Err(fail(
            StatusCode::CONFLICT,
            "conversation.subagent.parent_archived",
            "The parent Thread is archived",
        ));
    }
    ensure_parent_can_delegate(&s, &parent_id, &parent)?;
    let parent_lineage = lineage(&parent);
    let depth = parent_lineage["depth"].as_u64().unwrap_or(0) + 1;
    let root = text(parent_lineage, "root_thread_id");
    let root = if root.is_empty() {
        parent_id.as_str()
    } else {
        root
    };
    let child_id = Uuid::new_v4().to_string();
    let parent_turn = parent["thread"]["state"]["running_turn_id"].clone();
    let project_id = payload["project_id"].as_str().filter(|id|!id.is_empty()).unwrap_or_else(||text(&parent["thread"],"project_id"));
    let mut workspace_id = if project_id == text(&parent["thread"],"project_id") { parent["thread"]["workspace_id"].clone() } else {
        if let Some(workspace) = s.store.list("workspaces").map_err(storage)?.into_iter().find(|workspace|workspace["project_id"]==project_id && workspace["settings"]["mode"].as_str().is_none_or(str::is_empty)) {
            workspace["workspace_id"].clone()
        } else if s.store.get("projects", project_id).map_err(storage)?.is_some() {
            Value::Null
        } else {
            return Err(fail(StatusCode::NOT_FOUND,"conversation.subagent.project_missing","The target project has no registered checkout"));
        }
    };
    let mut runtime = child_runtime(&s, &parent, payload, &workspace_id)?;
    if project_id != text(&parent["thread"],"project_id") && payload["cwd"].as_str().is_none() && !workspace_id.is_null() {
        let workspace = s.store.get("workspaces", workspace_id.as_str().unwrap_or("")).map_err(storage)?
            .ok_or_else(|| fail(StatusCode::NOT_FOUND, "conversation.subagent.project_missing", "The target project has no registered checkout"))?;
        runtime["cwd"] = workspace["root_path"].clone();
    }
    if isolation == "worktree" { if let Some(runtime)=runtime.as_object_mut(){runtime.remove("cwd");} }
    let mut worktree_path = Value::Null;
    let mut parent_workspace_root = Value::Null;
    if isolation == "worktree" {
        let parent_workspace = workspace_id.as_str().unwrap_or("");
        let workspace = s
            .store
            .get("workspaces", parent_workspace)
            .map_err(storage)?
            .ok_or_else(|| {
                fail(
                    StatusCode::CONFLICT,
                    "conversation.subagent.worktree_unavailable",
                    "The parent has no workspace",
                )
            })?;
        let root_path = text(&workspace, "root_path");
        if root_path.is_empty() || text(&parent["thread"], "project_id").is_empty() {
            return Err(fail(
                StatusCode::CONFLICT,
                "conversation.subagent.worktree_unavailable",
                "A Git project workspace is required",
            ));
        }
        crate::git_process::run(
            std::path::Path::new(root_path),
            &["rev-parse", "--show-toplevel"],
            30,
            "conversation.subagent.worktree_unavailable",
            true,
        )
        .await?;
        let id = format!("ws_{}", Uuid::new_v4().simple());
        let state_dir = s
            .store
            .get("system", "state_dir")
            .map_err(storage)?
            .and_then(|value| value.as_str().map(str::to_owned))
            .ok_or_else(|| {
                fail(
                    StatusCode::CONFLICT,
                    "conversation.subagent.worktree_unavailable",
                    "Managed workspace directory is unavailable",
                )
            })?;
        worktree_path = json!(
            std::path::Path::new(&state_dir)
                .join("workspaces/worktrees")
                .join(&id)
                .to_string_lossy()
        );
        workspace_id = json!(id);
        parent_workspace_root = json!(root_path);
    }
    let before = parent.clone();
    let display_title = if title.is_empty() {
        prompt.lines().next().unwrap_or(prompt)
    } else {
        title
    };
    if isolation == "worktree" { if let Some(runtime)=runtime.as_object_mut(){runtime.remove("cwd");} }
    let child_body = json!({"command_id":command_id,"thread_id":child_id,
        "project_id":project_id,"workspace_id":workspace_id,
        "title":if title.is_empty(){""}else{title},"title_source":if title.is_empty(){"fallback"}else{"user"},
        "runtime":runtime});
    let is_src_parent = parent["thread"]["mode"] == "src";
    let child_mode = if is_src_parent { "src" } else { "conversation" };
    let (_, mut child, _) = chat::prepare_created_thread(&s, &child_body, child_mode)?;
    let mut effective_prompt = prompt.to_owned();
    let mut bound_src_delegation = false;
    let mut bound_src_task_id = String::new();
    if is_src_parent {
        let bound = crate::runs::src_api::bind_subthread_delegation(&s, &parent_id, &child_id, payload)
            .map_err(|error| (
                error.status,
                Json(json!({"error":{"code":error.code,"message":error.message,"detail":error.detail}})),
            ))?;
        if let Some(bound) = bound {
            bound_src_delegation = true;
            bound_src_task_id = bound["task_id"].as_str().unwrap_or("").to_owned();
            child["thread"]["src_delegation"] = json!({
                "task_id": bound["task_id"],
                "main_thread_id": bound["main_thread_id"],
                "parent_thread_id": parent_id,
                "todo_id": bound["delegation"]["todo_id"],
                "profile": bound["delegation"]["profile"],
                "allowed_tools": bound["delegation"]["allowed_tools"],
                "policy_version": bound["delegation"]["policy_version"],
                "policy_digest": bound["delegation"]["policy_digest"],
            });
            if let Some(refs) = bound["resolved_refs"].as_array().filter(|rows| !rows.is_empty()) {
                let snapshot_json = serde_json::to_string_pretty(refs).map_err(storage)?;
                effective_prompt = format!("{effective_prompt}\n\n[委派资料引用快照]\n{snapshot_json}");
            }
        }
    }
    let mut attachments = Vec::new();
    for reference in payload["attachments"].as_array().into_iter().flatten().filter_map(Value::as_str) {
        let (meta, _) = crate::files::read_control_attachment(&s, &parent_id, reference).map_err(|e|fail(StatusCode::BAD_REQUEST,"conversation.subagent.attachment_invalid",e))?;
        attachments.push(meta);
    }
    child["artifacts"] = json!(attachments);
    child["thread"]["started_by"] = json!({"thread_id":parent_id,"request_id":payload["request_id"].as_str().unwrap_or(&command_id)});
    child["thread"]["control_ancestor_restrictions"] = json!(crate::control::ancestor_restrictions(&s, &parent_id).map_err(|error| fail(StatusCode::INTERNAL_SERVER_ERROR,"control.storage_failed",error.to_string()))?);
    let mut routing = payload["routing"].clone();
    if routing.is_object() {
        routing["resolved"] = json!({"adapter_id":runtime["adapter_id"],"instance_id":runtime["instance_id"],
            "profile":runtime["credential_id"],"model":runtime["model"],"effort":runtime["effort"]});
    }
    let line = json!({"origin":"app_owned","subagent_id":child_id,"parent_thread_id":parent_id,
        "parent_turn_id":parent_turn,"root_thread_id":root,"depth":depth,"isolation":isolation,
        "workspace_id":workspace_id,"worktree_path":worktree_path,"role":payload["role"],"routing":routing});
    child["thread"]["routing"] = routing.clone();
    child["thread"]["state"]["lineage"] = line.clone();
    crate::control_subagents::open(&mut child);
    chat::emit(
        &s,
        &child_id,
        &mut child,
        "core.thread.lineage_set",
        json!({"thread_id":child_id,"lineage":line}),
    );
    let node = json!({"agent_id":child_id,"origin":"app_owned","title":display_title,
        "turn_id":parent_turn,"session_ref":child_id,"thread_id":child_id,"status":"pending",
        "request":effective_prompt,"result":null,"depth":depth,"adapter_id":runtime["adapter_id"],
        "role":payload["role"],"routing":routing,"todo_id":payload["todo_id"],
        "delegation_profile":payload["delegation_profile"],
        "model":runtime["model"],"effort":runtime["effort"],"access_mode":runtime["access_mode"],
        "acceptance_criteria":payload["acceptance_criteria"],"routing_reason":payload["routing_reason"],"attachments":payload["attachments"].as_array().cloned().unwrap_or_default(),
        "isolation":isolation,"workspace_id":workspace_id,"worktree_path":worktree_path,
        "parent_workspace_root":parent_workspace_root,"worktree_state":null,
        "spawn_state":"planned","spawn_command_id":command_id,
        "principal_id":body["actor"]["id"],"cancel_requested":false});
    if !parent["thread"]["state"]["subagents"].is_array() {
        parent["thread"]["state"]["subagents"] = json!([]);
    }
    parent["thread"]["state"]["subagents"]
        .as_array_mut()
        .unwrap()
        .push(node.clone());
    chat::emit(
        &s,
        &parent_id,
        &mut parent,
        "core.subagent.spawned",
        json!({"subagent_id":child_id,"node":node}),
    );
    let initial = json!({"receipt":{"command_id":command_id,"state":"pending",
        "aggregate":{"type":"thread","id":parent_id},"output":{"subagent_id":child_id,
            "thread_id":child_id,"conversation_url":format!("/chat/{child_id}"),"status":"pending",
            "wait":{"requested":false}}}});
    let record = json!({"request":body.clone(),"response":initial});
    let commit_res = s
        .store
        .chat_commit_subagent_spawn(
            &parent_id,
            &parent,
            &before,
            expected.flatten(),
            &child_id,
            &child,
            &key,
            &record,
        )
        .map_err(storage);
    let Some((parent_events, child_events, watermark)) = (match commit_res {
        Ok(Some(committed)) => Some(committed),
        Ok(None) => None,
        Err(error) => {
            if bound_src_delegation {
                let _ = crate::runs::src_api::revoke_subthread_delegation_for_task(&s, Some(&bound_src_task_id), &child_id, "subagent_spawn_aborted");
            }
            return Err(error);
        }
    })
    else {
        if bound_src_delegation {
            let _ = crate::runs::src_api::revoke_subthread_delegation_for_task(&s, Some(&bound_src_task_id), &child_id, "subagent_spawn_aborted");
        }
        return Err(fail(
            StatusCode::CONFLICT,
            "conversation.subagent.concurrent_update",
            "Parent Thread changed while spawning a subagent",
        ));
    };
    chat::finish_save(&s, &parent_id, &mut parent, parent_events, watermark);
    chat::finish_save(&s, &child_id, &mut child, child_events, watermark);
    drop(guard);
    if let Err(error) = dispatch_first(&s, &child_id).await {
        if bound_src_delegation {
            let _ = crate::runs::src_api::revoke_subthread_delegation(&s, &child_id, "subagent_spawn_failed");
        }
        return Ok(Json(settle_spawn_failure(
            &s,
            &parent_id,
            &child_id,
            &command_id,
            &error,
        )?));
    }
    let row = if wait {
        let (row, finished, waited) = wait_for_terminal(&s, &child_id, timeout).await?;
        let mut row = row;
        row["wait"] = json!({"requested":true,"outcome":if finished {"finished"}else{"pending"},
            "timeout_seconds":timeout,"waited_seconds":waited});
        row
    } else {
        let mut row = snapshot(&s, &child_id, false)?;
        row["wait"] = json!({"requested":false});
        row
    };
    let response = json!({"receipt":{"command_id":command_id,"state":"completed",
        "aggregate":{"type":"thread","id":parent_id},"output":row}});
    s.store
        .put(
            "conversation_commands",
            &key,
            &json!({"request":body,"response":response}),
        )
        .map_err(storage)?;
    Ok(Json(response))
}

fn settle_spawn_failure(
    s: &AppState,
    parent_id: &str,
    child_id: &str,
    command_id: &str,
    error: &Failure,
) -> Result<Value, Failure> {
    let failure = &error.1.0["error"];
    patch_node(
        s,
        parent_id,
        child_id,
        &json!({"status":"failed","spawn_state":"failed",
        "error_code":failure["code"],"error":failure["message"]}),
        "core.subagent.updated",
    )?;
    let response = json!({"receipt":{"command_id":command_id,"state":"failed",
        "aggregate":{"type":"thread","id":parent_id},"error":failure,
        "output":snapshot(s,child_id,false)?}});
    let key = format!("thread:{parent_id}:{command_id}");
    let mut record = s
        .store
        .get("conversation_commands", &key)
        .map_err(storage)?
        .ok_or_else(|| {
            fail(
                StatusCode::INTERNAL_SERVER_ERROR,
                "conversation.subagent.spawn_record_missing",
                "Spawn command receipt is missing",
            )
        })?;
    record["response"] = response.clone();
    s.store
        .put("conversation_commands", &key, &record)
        .map_err(storage)?;
    Ok(response)
}

async fn dispatch_first(s: &Arc<AppState>, child_id: &str) -> Result<(), Failure> {
    let child = thread(s, child_id)?;
    let parent_id = text(lineage(&child), "parent_thread_id");
    let parent = thread(s, parent_id)?;
    let node = parent_node(&parent, child_id).ok_or_else(|| {
        fail(
            StatusCode::NOT_FOUND,
            "conversation.subagent.spawn_record_invalid",
            "Parent spawn record is missing",
        )
    })?;
    if node["spawn_state"] == "dispatched" || node["spawn_state"] == "failed" {
        return Ok(());
    }
    if node["cancel_requested"] == true {
        patch_node(
            s,
            parent_id,
            child_id,
            &json!({"spawn_state":"skipped","status":"cancelled"}),
            "core.subagent.updated",
        )?;
        return Ok(());
    }
    if child["turns"]
        .as_array()
        .is_some_and(|rows| !rows.is_empty())
        || child["queue"]
            .as_array()
            .is_some_and(|rows| !rows.is_empty())
    {
        patch_node(
            s,
            parent_id,
            child_id,
            &json!({"spawn_state":"dispatched"}),
            "core.subagent.updated",
        )?;
        return Ok(());
    }
    if lineage(&child)["isolation"] == "worktree" {
        let workspace_id = text(lineage(&child), "workspace_id");
        if s.store
            .get("workspaces", workspace_id)
            .map_err(storage)?
            .is_none()
        {
            let parent_root = std::path::PathBuf::from(text(node, "parent_workspace_root"));
            let data = crate::native_chat::state_dir(s).join("workspaces");
            let name = child_id.to_owned();
            let created = tokio::task::spawn_blocking(move || tcode_control_services::worktree::provision(&parent_root, &name, None, &data))
                .await.map_err(|error|fail(StatusCode::INTERNAL_SERVER_ERROR,"conversation.subagent.worktree_failed",error.to_string()))?
                .map_err(|error|fail(StatusCode::CONFLICT,"conversation.subagent.worktree_failed",error.to_string()))?;
            let root = crate::workspace_read_api::project_bound_root(s,text(&child["thread"],"project_id"))?;
            let workspace = json!({"workspace_id":workspace_id,"project_id":child["thread"]["project_id"],"kind":"local","root_path":created.path,"created_at":crate::control::now(),"updated_at":crate::control::now(),"settings":{"mode":"new_worktree","branch":created.branch,"base_ref":created.base,"parent_root":root,"created_by":"subagent","subagent_id":child_id,"seed_summary":{"manifest_found":created.seed_summary.manifest_found,"copied_files":created.seed_summary.copied_files,"skipped":created.seed_summary.skipped}}});
            s.store.put("workspaces",workspace_id,&workspace).map_err(storage)?;
            let guard=s.chat.active.lock().await;
            let mut current=thread(s,child_id)?;
            current["thread"]["state"]["lineage"]["worktree_path"]=json!(created.path);
            chat::save(s,child_id,&mut current)?;
            drop(guard);
            patch_node(s,parent_id,child_id,&json!({"worktree_path":created.path}),"core.subagent.updated")?;
        }
        patch_node(
            s,
            parent_id,
            child_id,
            &json!({"spawn_state":"workspace_ready","worktree_state":"active"}),
            "core.subagent.updated",
        )?;
    }
    let acceptance = text(node, "acceptance_criteria");
    let prompt = text(node, "request");
    let prompt = if acceptance.is_empty() {
        prompt.to_owned()
    } else {
        format!("{prompt}\n\n[本次委派的验收条件]\n{acceptance}")
    };
    let send = json!({"command_id":format!("subagent-send:{}",text(node,"spawn_command_id")),
        "command_type":"conversation.turn.send","actor":{"kind":"agent","id":parent_id},"payload":{"text":prompt, "attachments":node["attachments"].as_array().cloned().unwrap_or_default(),
            "client_message_id":format!("subagent:{child_id}")}});
    let _ = Box::pin(chat::command(
        State(s.clone()),
        Path(child_id.to_owned()),
        Json(send),
    ))
    .await?;
    patch_node(
        s,
        parent_id,
        child_id,
        &json!({"spawn_state":"dispatched"}),
        "core.subagent.updated",
    )?;
    sync_node(s, child_id)?;
    Ok(())
}

pub fn supports_command(kind: &str) -> bool {
    matches!(
        kind,
        "conversation.subagent.spawn"
            | "conversation.subagent.send"
            | "conversation.subagent.cancel"
    )
}

pub async fn command(
    s: Arc<AppState>,
    parent_id: String,
    body: Value,
    platform_expected: Option<Option<u64>>,
) -> Api {
    let kind = text(&body, "command_type");
    if kind == "conversation.subagent.spawn" {
        return spawn(s, parent_id, body, platform_expected).await;
    }
    let command_id = text(&body, "command_id").to_owned();
    if command_id.is_empty() {
        return Err(fail(
            StatusCode::BAD_REQUEST,
            "conversation.subagent.argument_invalid",
            "command_id is required",
        ));
    }
    let key = format!("thread:{parent_id}:{command_id}");
    if let Some(prior) = s
        .store
        .get("conversation_commands", &key)
        .map_err(storage)?
    {
        if prior["request"] != body {
            return Err(fail(
                StatusCode::CONFLICT,
                "command.idempotency_conflict",
                "command_id was reused with different content",
            ));
        }
        return Ok(Json(prior["response"].clone()));
    }
    let parent = thread(&s, &parent_id)?;
    if !matches!(parent["thread"]["mode"].as_str(), Some("conversation" | "src")) || parent["thread"]["archived"] == true {
        return Err(fail(
            StatusCode::CONFLICT,
            "conversation.subagent.mode_invalid",
            "An active conversation Thread is required",
        ));
    }
    if platform_expected
        .flatten()
        .is_some_and(|expected| chat::stream_head(&parent) != expected)
    {
        return Err(fail(
            StatusCode::CONFLICT,
            "command.version_conflict",
            "Parent Thread version changed",
        ));
    }
    let payload = &body["payload"];
    if !payload.is_object() {
        return Err(fail(
            StatusCode::BAD_REQUEST,
            "conversation.subagent.argument_invalid",
            "payload must be an object",
        ));
    }
    let output = match kind {
        "conversation.subagent.send" => send(&s, &parent_id, &command_id, &body, platform_expected.flatten()).await?,
        "conversation.subagent.cancel" => cancel(&s, &parent_id, &command_id, payload, Some(&body)).await?,
        _ => {
            return Err(fail(
                StatusCode::NOT_IMPLEMENTED,
                "capability.unavailable",
                "Unknown subagent command",
            ));
        }
    };
    let failed = kind == "conversation.subagent.cancel"
        && output["outcomes"].as_array().is_some_and(|rows| {
            rows.iter().any(|row| {
                row["errors"]
                    .as_array()
                    .is_some_and(|errors| !errors.is_empty())
            })
        });
    let error = if failed {
        json!({"code":"conversation.subagent.cancel_incomplete","category":"state",
            "message":"One or more subagents could not be stopped","detail":{"outcomes":output["outcomes"]}})
    } else {
        Value::Null
    };
    let response = json!({"receipt":{"command_id":command_id,
        "state":if failed {"failed"} else {"completed"},"error":error,
        "aggregate":{"type":"thread","id":parent_id},"output":output}});
    s.store
        .put(
            "conversation_commands",
            &key,
            &json!({"request":body,"response":response}),
        )
        .map_err(storage)?;
    Ok(Json(response))
}

async fn send(
    s: &Arc<AppState>,
    parent_id: &str,
    command_id: &str,
    command_body: &Value,
    expected_version: Option<u64>,
) -> Result<Value, Failure> {
    let payload=&command_body["payload"];
    let child_id = text(payload, "subagent_id");
    let prompt = text(payload, "prompt");
    if child_id.is_empty() || prompt.is_empty() {
        return Err(fail(
            StatusCode::BAD_REQUEST,
            "conversation.subagent.argument_invalid",
            "subagent_id and prompt are required",
        ));
    }
    let (wait, timeout) = wait_options(payload)?;
    let guard=s.chat.active.lock().await;
    let mut child = thread(s, child_id)?;
    let child_before=child.clone();
    if text(lineage(&child), "parent_thread_id") != parent_id {
        return Err(fail(
            StatusCode::NOT_FOUND,
            "conversation.subagent.not_found",
            "subagent_id must identify a direct child",
        ));
    }
    let mut parent = thread(s, parent_id)?;
    let parent_before=parent.clone();
    ensure_parent_can_delegate(s, parent_id, &parent)?;
    let node = parent_node(&parent, child_id).ok_or_else(|| {
        fail(
            StatusCode::NOT_FOUND,
            "conversation.subagent.not_found",
            "Subagent node is missing",
        )
    })?;
    let (status, _, _, _) = live_status(&child, node);
    if node["cancel_requested"] == true
        || child["thread"]["archived"] == true
        || node["spawn_state"] != "dispatched"
        || child["thread"]["state"]["queue_paused"] == true
        || child["queue"]
            .as_array()
            .is_some_and(|rows| !rows.is_empty())
    {
        return Err(fail(
            StatusCode::CONFLICT,
            "conversation.subagent.not_continuable",
            "The child is cancelled, archived, or has an unresolved queue",
        ));
    }
    if !terminal(status) {
        return Err(fail(
            StatusCode::CONFLICT,
            "conversation.subagent.busy",
            "The child is still active",
        ));
    }
    let body=json!({"command_id":format!("subagent-continue:{command_id}"),"command_type":"conversation.turn.send","payload":{"text":prompt,"client_message_id":command_id}});
    let queued=chat::enqueue(s,child_id,&mut child,&body)?;
    crate::control_subagents::open(&mut child);
    let queue_id=queued["queue_id"].as_str().unwrap_or("").to_owned();
    if let Some(node)=parent_node_mut(&mut parent,child_id){node["status"]=json!("pending");node["continuation_command_id"]=json!(command_id);node["continuation_queue_id"]=json!(queue_id);}
    chat::emit(s,parent_id,&mut parent,"core.subagent.send_admitted",json!({"subagent_id":child_id,"command_id":command_id,"queue_id":queue_id}));
    let key=format!("thread:{parent_id}:{command_id}");
    let admitted=json!({"request":command_body,"response":{"receipt":{"command_id":command_id,"state":"completed","aggregate":{"type":"thread","id":parent_id},"output":{"subagent_id":child_id,"thread_id":child_id,"queue_id":queue_id,"status":"pending","command_id":command_id,"wait":{"requested":false}}}}});
    let Some((mut events,watermark))=s.store.chat_commit_many(&[(child_id,&child,Some(&child_before)),(parent_id,&parent,Some(&parent_before))],expected_version.map(|version|(parent_id,version)),&[("conversation_commands",&key,None)],&[("conversation_commands",&key,&admitted)]).map_err(storage)?else{return Err(fail(StatusCode::CONFLICT,"conversation.subagent.concurrent_update","Parent or child changed during continuation admission"));};
    chat::finish_save(s,parent_id,&mut parent,events.pop().unwrap(),watermark);
    chat::finish_save(s,child_id,&mut child,events.pop().unwrap(),watermark);
    drop(guard);
    dispatch_continuation(s,child_id,&queue_id).await?;
    sync_node(s,child_id)?;
    let mut result = if wait {
        let (result, finished, waited) = wait_for_terminal(s, child_id, timeout).await?;
        let mut result = result;
        result["wait"] = json!({"requested":true,"outcome":if finished {"finished"}else{"pending"},
            "timeout_seconds":timeout,"waited_seconds":waited});
        result
    } else {
        let mut result = snapshot(s, child_id, false)?;
        result["wait"] = json!({"requested":false});
        result
    };
    result["command_id"] = json!(command_id);
    Ok(result)
}

async fn dispatch_continuation(s:&Arc<AppState>,child_id:&str,queue_id:&str)->Result<(),Failure>{
    let guard=s.chat.active.lock().await;let mut child=thread(s,child_id)?;
    let parent_id=text(lineage(&child),"parent_thread_id");let parent=thread(s,parent_id)?;
    if parent_node(&parent,child_id).is_some_and(|node|node["cancel_requested"]==true)||parent["thread"]["archived"]==true{return Ok(());}
    if child["queue"].as_array().is_none_or(|rows|!rows.iter().any(|row|row["queue_id"]==queue_id)){return Ok(());}
    if child["thread"]["state"]["queue_pause_reason"]=="process_restarted"{child["thread"]["state"]["queue_paused"]=json!(false);child["thread"]["state"]["queue_pause_reason"]=json!("");}
    let next=chat::promote_queue(s,child_id,&mut child);chat::save(s,child_id,&mut child)?;drop(guard);
    if let Some((turn,dispatch))=next{chat::start(s.clone(),child_id.into(),turn,dispatch).await;}Ok(())
}

async fn cancel(
    s: &Arc<AppState>,
    caller: &str,
    command_id: &str,
    payload: &Value,
    command_body:Option<&Value>,
) -> Result<Value, Failure> {
    let target = text(payload, "subagent_id");
    if target.is_empty() {
        return Err(fail(
            StatusCode::BAD_REQUEST,
            "conversation.subagent.argument_invalid",
            "subagent_id is required",
        ));
    }
    if !is_descendant(s, target, caller) {
        return Err(fail(
            StatusCode::NOT_FOUND,
            "conversation.subagent.not_found",
            "Subagent does not belong to this Thread",
        ));
    }
    let reason = payload["reason"]
        .as_str()
        .filter(|value| !value.is_empty())
        .unwrap_or("requested");
    let mut targets = vec![target.to_owned()];
    for id in descendants(s, target)? {
        if id != target && !targets.iter().any(|existing| existing == &id) {
            targets.push(id);
        }
    }
    {
        let _guard=s.chat.active.lock().await;
        let mut child_parents = Vec::new();
        for child_id in &targets {
            let child = thread(s, child_id)?;
            let parent_id = text(lineage(&child), "parent_thread_id").to_owned();
            thread(s, &parent_id)?;
            child_parents.push((child_id, parent_id));
        }
        let mut parents=std::collections::BTreeMap::<String,(Value,Value)>::new();
        for (child_id, parent_id) in child_parents {
            if !parents.contains_key(&parent_id) {
                let parent = s.store.get(chat::COLLECTION, &parent_id).map_err(storage)?
                    .ok_or_else(|| fail(StatusCode::NOT_FOUND, "conversation.thread.not_found", "Parent Thread not found"))?;
                parents.insert(parent_id.clone(), (parent.clone(), parent));
            }
            let(_,parent)=parents.get_mut(&parent_id).unwrap();
            let node=parent_node_mut(parent,child_id).ok_or_else(||fail(StatusCode::NOT_FOUND,"conversation.subagent.not_found","Cancellation target has no parent node"))?;
            node["cancel_requested"]=json!(true);node["cancel_command_id"]=json!(command_id);node["cancel_reason"]=json!(reason);node["cancel_caller"]=json!(caller);node["cancel_target"]=json!(target);
            chat::emit(s,&parent_id,parent,"core.subagent.cancel_requested",json!({"subagent_id":child_id,"command_id":command_id,"reason":reason,"cascade_from":target}));
        }
        let key=format!("thread:{caller}:{command_id}");let old=if command_body.is_some(){s.store.get("conversation_commands",&key).map_err(storage)?}else{None};
        let record=command_body.map(|body|json!({"request":body,"response":{"receipt":{"command_id":command_id,"state":"pending","aggregate":{"type":"thread","id":caller},"output":{"subagent_id":target,"targets":targets}}}}));
        let records=record.as_ref().map(|row|vec![("conversation_commands",key.as_str(),row)]).unwrap_or_default();
        let expected=record.as_ref().map(|_|vec![("conversation_commands",key.as_str(),old.as_ref())]).unwrap_or_default();
        let documents=parents.iter().map(|(id,(before,after))|(id.as_str(),after,Some(before))).collect::<Vec<_>>();
        let Some((events,watermark))=s.store.chat_commit_many(&documents,None,&expected,&records).map_err(storage)?else{return Err(fail(StatusCode::CONFLICT,"conversation.subagent.concurrent_update","Cancellation tree changed during admission"));};
        for((id,(_,after)),events)in parents.iter_mut().zip(events){chat::finish_save(s,id,after,events,watermark);}
    }
    let mut outcomes = Vec::new();
    for child_id in targets {
        outcomes.push(cancel_one(s, caller, target, command_id, reason, &child_id).await);
    }
    let output=json!({"subagent_id":target,"reason":reason,"outcomes":outcomes});
    let receipt_key=format!("thread:{caller}:{command_id}");let previous=s.store.get("conversation_commands",&receipt_key).map_err(storage)?;
    let original=command_body.cloned().or_else(||previous.as_ref().and_then(|row|row.get("request")).cloned());
    if let Some(body)=original{
        let failed=output["outcomes"].as_array().into_iter().flatten().any(|outcome|outcome["errors"].as_array().is_some_and(|errors|!errors.is_empty()));
        let response=json!({"receipt":{"command_id":command_id,"state":if failed{"failed"}else{"completed"},"aggregate":{"type":"thread","id":caller},"output":output,"error":if failed{json!({"code":"conversation.subagent.cancel_incomplete","category":"state","message":"Some child stop actions failed","detail":output})}else{Value::Null}}});
        let saved=json!({"request":body,"response":response});
        if !s.store.put_many_if(&[("conversation_commands",receipt_key.as_str(),previous.as_ref())],&[("conversation_commands",receipt_key.as_str(),&saved)]).map_err(storage)?{return Err(fail(StatusCode::CONFLICT,"conversation.subagent.concurrent_update","Cancellation receipt changed during settlement"));}
    }
    Ok(output)
}

fn cancel_error(error: Failure) -> Value {
    let mut value = error.1.0["error"].clone();
    if !value.is_object() {
        value = json!({});
    }
    if !value["code"].is_string() {
        value["code"] = json!("conversation.subagent.cancel_action_failed");
    }
    value
}

async fn cancel_one(
    s: &Arc<AppState>,
    caller: &str,
    target: &str,
    command_id: &str,
    reason: &str,
    child_id: &str,
) -> Value {
    let mut actions = Vec::<&str>::new();
    let mut errors = Vec::<Value>::new();
    let child = match thread(s, child_id) {
        Ok(child) => child,
        Err(error) => {
            return json!({"subagent_id":child_id,"actions":actions,
            "errors":[cancel_error(error)],"status":null});
        }
    };
    {
        let _guard = s.chat.active.lock().await;
        match thread(s, child_id) {
            Ok(mut current) => {
                current["thread"]["state"]["subagent_report"]["state"] = json!("none");
                current["thread"]["state"]["subagent_report"]["cancelled"] = json!(true);
                current["thread"]["state"]["subagent_report"]["pending_delivery"] = Value::Null;
                current["thread"]["state"]["subagent_report"]["pending_reports"] = json!([]);
                if let Err(error) = chat::save(s, child_id, &mut current) { errors.push(cancel_error(error)); }
            }
            Err(error) => errors.push(cancel_error(error)),
        }
    }
    let parent_id = text(lineage(&child), "parent_thread_id");
    if let Err(error) = patch_node(
        s,
        parent_id,
        child_id,
        &json!({"cancel_requested":true,"report":"none","delivery":"not_delivered","activity":"正在停止","reason":reason,
            "requested_by_thread_id":caller,
            "cascade_from":if child_id == target { Value::Null } else { json!(target) }}),
        "core.subagent.cancel_requested",
    ) {
        errors.push(cancel_error(error));
    }
    if child["queue"]
        .as_array()
        .is_some_and(|rows| !rows.is_empty())
        && child["thread"]["state"]["queue_paused"] != true
    {
        let body = json!({"command_id":format!("subagent-cancel:{command_id}:{child_id}:queue"),
            "command_type":"conversation.queue.pause","payload":{"reason":"subagent_cancelled"}});
        match Box::pin(chat::command(
            State(s.clone()),
            Path(child_id.to_owned()),
            Json(body),
        ))
        .await
        {
            Ok(Json(receipt)) if receipt["receipt"]["error"].is_object() => {
                errors.push(receipt["receipt"]["error"].clone())
            }
            Ok(_) => {}
            Err(error) => errors.push(cancel_error(error)),
        }
        actions.push("queue_paused");
    }
    let mut interrupted_turn = Value::Null;
    match thread(s, child_id) {
        Ok(current)
            if current["thread"]["state"]["status"] == "running"
                || !text(&current["thread"]["state"], "running_turn_id").is_empty() =>
        {
            let turn_id = text(&current["thread"]["state"], "running_turn_id").to_owned();
            if !turn_id.is_empty() {
                interrupted_turn = json!(turn_id);
            }
            let body = json!({"command_id":format!("subagent-cancel:{command_id}:{child_id}:interrupt:{turn_id}"),
                "command_type":"conversation.turn.interrupt","payload":{"expected_turn_id":turn_id}});
            match Box::pin(chat::command(
                State(s.clone()),
                Path(child_id.to_owned()),
                Json(body),
            ))
            .await
            {
                Ok(Json(receipt)) if receipt["receipt"]["error"].is_object() => {
                    errors.push(receipt["receipt"]["error"].clone())
                }
                Ok(_) => actions.push("turn_interrupted"),
                Err(error)
                    if matches!(
                        error.1.0["error"]["code"].as_str(),
                        Some(
                            "conversation.turn.not_running" | "conversation.turn.expected_mismatch"
                        )
                    ) =>
                {
                    actions.push("turn_already_ended")
                }
                Err(error) => errors.push(cancel_error(error)),
            }
        }
        Ok(_) => {}
        Err(error) => errors.push(cancel_error(error)),
    }
    if let Err(error) = sync_node(s, child_id) {
        errors.push(cancel_error(error));
    }
    if child["thread"].get("src_delegation").is_some_and(Value::is_object)
        && let Err(error) = crate::runs::src_api::revoke_subthread_delegation(s, child_id, "subagent_cancelled")
    {
        errors.push(json!({"code":error.code,"message":error.message}));
    }
    let status = match snapshot(s, child_id, false) {
        Ok(row) => row["status"].clone(),
        Err(error) => {
            errors.push(cancel_error(error));
            Value::Null
        }
    };
    let mut outcome = json!({"subagent_id":child_id,"actions":actions,"status":status});
    if !interrupted_turn.is_null() {
        outcome["turn_id"] = interrupted_turn;
    }
    if !errors.is_empty() {
        outcome["errors"] = json!(errors);
    }
    outcome
}

pub async fn cascade(
    s: Arc<AppState>,
    parent_id: String,
    kind: String,
    turn_id: String,
) -> Result<(), Failure> {
    let parent = thread(&s, &parent_id)?;
    let nodes = parent["thread"]["state"]["subagents"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    for node in nodes {
        if kind == "conversation.turn.interrupt" && node["turn_id"] != turn_id {
            continue;
        }
        let child_id = text(&node, "agent_id");
        if child_id.is_empty() {
            continue;
        }
        let Ok(child) = thread(&s, child_id) else {
            tracing::warn!(child_id, "Could not load child conversation during cascade");
            continue;
        };
        let (status, _, _, _) = live_status(&child, &node);
        if matches!(status, "pending" | "running") {
            let reason = if kind == "conversation.thread.archive" {
                "parent_thread_archived"
            } else {
                "parent_turn_interrupted"
            };
            let command_id = format!("subagent-cascade:{parent_id}:{kind}:{turn_id}:{child_id}");
            match cancel(
                &s,
                &parent_id,
                &command_id,
                &json!({"subagent_id":child_id,"reason":reason}),
                None,
            )
            .await
            {
                Ok(output) => {
                    if output["outcomes"].as_array().is_some_and(|rows| {
                        rows.iter().any(|row| {
                            row["errors"]
                                .as_array()
                                .is_some_and(|errors| !errors.is_empty())
                        })
                    }) {
                        tracing::warn!(child_id, "Subagent cascade cancellation had child action errors");
                    }
                }
                Err((_, Json(error))) => {
                    tracing::warn!(child_id, error=%error, "Could not cancel child during cascade");
                }
            }
        }
        if kind == "conversation.thread.archive" && child["thread"]["archived"] != true {
            let body = json!({"command_id":format!("subagent-cascade-archive:{parent_id}:{child_id}"),
                "command_type":"conversation.thread.archive","payload":{"close_running":true}});
            if let Err((_, Json(error))) = Box::pin(chat::command(State(s.clone()), Path(child_id.to_owned()), Json(body))).await {
                tracing::warn!(child_id, error=%error, "Could not archive child during cascade");
            }
        }
        if kind == "conversation.thread.archive" {
            if let Err((_, Json(body))) = cleanup_worktree(&s, child_id).await {
                tracing::warn!(child_id, error=%body, "Could not clean archived child worktree");
            }
        }
    }
    Ok(())
}

pub async fn recover(s: Arc<AppState>)->Result<(),String> {
    let _ = configured_limits();
    let rows=s.store.list(chat::COLLECTION).map_err(|error|error.to_string())?;
    let mut failures=Vec::new();
        {let records=s.store.list("conversation_commands").map_err(|error|error.to_string())?;
            for record in records.into_iter().filter(|row|(row["request"]["command_type"]=="conversation.subagent.cancel" || row["request"]["type"]=="conversation.subagent.cancel") && row["response"]["receipt"]["state"]=="pending") {
                let body=&record["request"];let caller=record["response"]["receipt"]["aggregate"]["id"].as_str().unwrap_or("");
                if !caller.is_empty() && let Err((_,Json(error)))=cancel(&s,caller,text(body,"command_id"),&body["payload"],Some(body)).await {failures.push(format!("{caller}: {error}"));}
            }
        }
        for parent in rows {
            let parent_id = text(&parent["thread"], "thread_id").to_owned();
            if parent_id.is_empty() {
                continue;
            }
            if parent["thread"]["archived"] == true {
                if let Err((_, Json(body))) = cascade(
                    s.clone(),
                    parent_id.clone(),
                    "conversation.thread.archive".to_owned(),
                    String::new(),
                )
                .await
                {
                    failures.push(format!("{parent_id}: {body}"));
                }
            }
            for turn in parent["turns"].as_array().into_iter().flatten().filter(|row|matches!(row["status"].as_str(),Some("interrupted"|"cancelled"))){
                if let Err((_,Json(error)))=cascade(s.clone(),parent_id.clone(),"conversation.turn.interrupt".into(),text(turn,"turn_id").into()).await{failures.push(format!("{parent_id}: {error}"));}
            }
            for child_id in direct_children(&parent){
                if s.store.get(chat::COLLECTION,&child_id).map_err(|error|error.to_string())?.is_none(){
                    chat::forget_deleted_child(&s,&child_id).map_err(|(_,body)|body.to_string())?;
                    continue;
                }
                let current=thread(&s,&parent_id).unwrap_or_else(|_|parent.clone());
                if let Some(node)=parent_node(&current,&child_id).filter(|node|node["cancel_requested"]==true){
                    let command=text(node,"cancel_command_id");let command=if command.is_empty(){"restart-cancel"}else{command};
                    let outcome=cancel_one(&s,&parent_id,text(node,"cancel_target"),command,text(node,"cancel_reason"),&child_id).await;
                    if outcome["errors"].as_array().is_some_and(|errors|!errors.is_empty()){failures.push(format!("{child_id}: {}",outcome["errors"]));}
                }
                if let Err((_,Json(error)))=recover_child(&s,&parent_id,&child_id).await{tracing::warn!(parent_id,child_id,error=%error,"Could not recover child conversation");}
            }

        }
    if failures.is_empty(){Ok(())}else{Err(failures.join("; "))}
}

async fn recover_child(s: &Arc<AppState>, parent_id: &str, child_id: &str) -> Result<(), Failure> {
    let child = thread(s, child_id)?;
    let parent = thread(s, parent_id)?;
    let node = parent_node(&parent, child_id).ok_or_else(|| {
        fail(
            StatusCode::NOT_FOUND,
            "conversation.subagent.spawn_record_invalid",
            "Parent node is missing",
        )
    })?;
    let state = text(node, "spawn_state");
    if matches!(state, "planned" | "workspace_ready" | "activated") {
        if let Err(error) = dispatch_first(s, child_id).await {
            let command_id = text(node, "spawn_command_id");
            settle_spawn_failure(s, parent_id, child_id, command_id, &error)?;
            return Err(error);
        }
    }
    if node["cancel_requested"]!=true && let Some(queue_id)=node["continuation_queue_id"].as_str(){dispatch_continuation(s,child_id,queue_id).await?;}
    sync_node(s, child_id)?;
    let command_id = text(node, "spawn_command_id");
    let key = format!("thread:{parent_id}:{command_id}");
    if let Some(mut record) = s
        .store
        .get("conversation_commands", &key)
        .map_err(storage)?
        && record["response"]["receipt"]["state"] == "pending"
    {
        let (wait, timeout) = wait_options(&record["request"]["payload"])?;
        let mut output = if wait {
            let (row, finished, waited) = wait_for_terminal(s, child_id, timeout).await?;
            let mut row = row;
            row["wait"] = json!({"requested":true,"outcome":if finished {"finished"} else {"pending"},
                "timeout_seconds":timeout,"waited_seconds":waited});
            row
        } else {
            let mut row = snapshot(s, child_id, false)?;
            row["wait"] = json!({"requested":false});
            row
        };
        output["command_id"] = json!(command_id);
        let failed = state == "failed";
        let error = if failed {
            json!({"code":node["error_code"],"message":node["error"],
            "category":"state"})
        } else {
            Value::Null
        };
        record["response"] = json!({"receipt":{"command_id":command_id,
            "state":if failed {"failed"} else {"completed"},"aggregate":{"type":"thread","id":parent_id},
            "error":error,"output":output}});
        s.store
            .put("conversation_commands", &key, &record)
            .map_err(storage)?;
    }
    if child["thread"]["archived"] == true {
        cleanup_worktree(s, child_id).await?;
    }
    Ok(())
}

pub async fn cleanup_worktree(s: &Arc<AppState>, child_id: &str) -> Result<(), Failure> {
    let child = thread(s, child_id)?;
    if child["thread"]["archived"] != true || lineage(&child)["isolation"] != "worktree" {
        return Ok(());
    }
    let workspace_id = text(lineage(&child), "workspace_id");
    if workspace_id.is_empty()
        || s.store
            .get("workspaces", workspace_id)
            .map_err(storage)?
            .is_none()
    {
        return Ok(());
    }
    let body = json!({"command_id":format!("subagent-worktree-cleanup:{child_id}"),"workspace_id":workspace_id});
    match crate::workspaces::delete_worktree(State(s.clone()), Json(body)).await {
        Ok(_) => {
            patch_node(
                s,
                text(lineage(&child), "parent_thread_id"),
                child_id,
                &json!({"worktree_state":"removed"}),
                "core.subagent.updated",
            )?;
            Ok(())
        }
        Err((status, Json(body))) if body["error"]["code"] == "conversation.git.worktree_dirty" => {
            patch_node(
                s,
                text(lineage(&child), "parent_thread_id"),
                child_id,
                &json!({"worktree_state":"retained","error_code":"conversation.git.worktree_dirty",
                    "error":body["error"]["message"]}),
                "core.subagent.updated",
            )?;
            Err((status, Json(body)))
        }
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_state() -> Arc<AppState> {
        let store = crate::store::Store::open(":memory:").unwrap();
        Arc::new(AppState::new(store))
    }

    fn seed_subagent_tree(s: &AppState) {
        let epoch = s.chat.epoch().to_owned();
        let root = json!({
            "thread": {
                "thread_id": "root",
                "title": "Root Thread",
                "mode": "conversation",
                "archived": false,
                "state": {
                    "status": "running",
                    "running_turn_id": "root-turn-1",
                    "subagents": [
                        {
                            "agent_id": "sub-parent",
                            "origin": "app_owned",
                            "title": "Parent Subagent",
                            "turn_id": "root-turn-1",
                            "status": "running",
                            "spawn_state": "dispatched",
                            "cancel_requested": false
                        },
                        {
                            "agent_id": "sub-sibling",
                            "origin": "app_owned",
                            "title": "Sibling Subagent",
                            "turn_id": "root-turn-1",
                            "status": "running",
                            "spawn_state": "dispatched",
                            "cancel_requested": false
                        }
                    ]
                }
            },
            "runtime": {"adapter_id": "openai", "instance_id": "text", "access_mode": "supervised"},
            "runtime_epoch": epoch,
            "turns": [{"turn_id": "root-turn-1", "seq": 1, "status": "running", "text": "coord"}],
            "messages": [],
            "queue": [],
            "event_seq": 1
        });
        let sub_parent = json!({
            "thread": {
                "thread_id": "sub-parent",
                "title": "Parent Subagent",
                "mode": "conversation",
                "archived": false,
                "state": {
                    "status": "running",
                    "running_turn_id": "turn-parent",
                    "queue_paused": false,
                    "lineage": {
                        "origin": "app_owned",
                        "subagent_id": "sub-parent",
                        "parent_thread_id": "root",
                        "parent_turn_id": "root-turn-1",
                        "root_thread_id": "root",
                        "depth": 1,
                        "isolation": "shared"
                    },
                    "subagents": [
                        {
                            "agent_id": "sub-fail",
                            "origin": "app_owned",
                            "title": "Failing Child",
                            "turn_id": "turn-parent-0",
                            "status": "running",
                            "spawn_state": "dispatched",
                            "cancel_requested": false
                        },
                        {
                            "agent_id": "sub-ok",
                            "origin": "app_owned",
                            "title": "Healthy Child",
                            "turn_id": "turn-parent-0",
                            "status": "running",
                            "spawn_state": "dispatched",
                            "cancel_requested": false
                        }
                    ]
                }
            },
            "runtime": {"adapter_id": "openai", "instance_id": "text", "access_mode": "supervised"},
            "runtime_epoch": epoch,
            "turns": [{"turn_id": "turn-parent", "seq": 1, "status": "running", "text": "work"}],
            "messages": [],
            "queue": [{"queue_id": "q-parent", "status": "queued", "text": "queued"}],
            "queue_epoch": epoch,
            "event_seq": 1
        });
        let sub_fail = json!({
            "thread": {
                "thread_id": "sub-fail",
                "title": "Failing Child",
                "mode": "conversation",
                "archived": false,
                "state": {
                    "status": "running",
                    "running_turn_id": "turn-fail",
                    "queue_paused": false,
                    "history_mutation": {"epoch": epoch},
                    "lineage": {
                        "origin": "app_owned",
                        "subagent_id": "sub-fail",
                        "parent_thread_id": "sub-parent",
                        "parent_turn_id": "turn-parent",
                        "root_thread_id": "root",
                        "depth": 2,
                        "isolation": "shared"
                    }
                }
            },
            "runtime": {"adapter_id": "openai", "instance_id": "text", "access_mode": "supervised"},
            "runtime_epoch": epoch,
            "turns": [{"turn_id": "turn-fail", "seq": 1, "status": "running", "text": "fail work"}],
            "messages": [],
            "queue": [{"queue_id": "q-fail", "status": "queued", "text": "queued"}],
            "queue_epoch": epoch,
            "event_seq": 1
        });
        let sub_ok = json!({
            "thread": {
                "thread_id": "sub-ok",
                "title": "Healthy Child",
                "mode": "conversation",
                "archived": false,
                "state": {
                    "status": "running",
                    "running_turn_id": "turn-ok",
                    "queue_paused": false,
                    "lineage": {
                        "origin": "app_owned",
                        "subagent_id": "sub-ok",
                        "parent_thread_id": "sub-parent",
                        "parent_turn_id": "turn-parent",
                        "root_thread_id": "root",
                        "depth": 2,
                        "isolation": "shared"
                    }
                }
            },
            "runtime": {"adapter_id": "openai", "instance_id": "text", "access_mode": "supervised"},
            "runtime_epoch": epoch,
            "turns": [{"turn_id": "turn-ok", "seq": 1, "status": "running", "text": "ok work"}],
            "messages": [],
            "queue": [{"queue_id": "q-ok", "status": "queued", "text": "queued"}],
            "queue_epoch": epoch,
            "event_seq": 1
        });
        let sub_sibling = json!({
            "thread": {
                "thread_id": "sub-sibling",
                "title": "Sibling Subagent",
                "mode": "conversation",
                "archived": false,
                "state": {
                    "status": "running",
                    "running_turn_id": "turn-sibling",
                    "queue_paused": false,
                    "lineage": {
                        "origin": "app_owned",
                        "subagent_id": "sub-sibling",
                        "parent_thread_id": "root",
                        "parent_turn_id": "root-turn-1",
                        "root_thread_id": "root",
                        "depth": 1,
                        "isolation": "shared"
                    }
                }
            },
            "runtime": {"adapter_id": "openai", "instance_id": "text", "access_mode": "supervised"},
            "runtime_epoch": epoch,
            "turns": [{"turn_id": "turn-sibling", "seq": 1, "status": "running", "text": "sibling"}],
            "messages": [],
            "queue": [],
            "event_seq": 1
        });
        for (id, mut doc) in [
            ("root", root),
            ("sub-parent", sub_parent),
            ("sub-fail", sub_fail),
            ("sub-ok", sub_ok),
            ("sub-sibling", sub_sibling),
        ] {
            doc["thread"]["state"]["head_stream_seq"] = json!(1);
            s.store.put(chat::COLLECTION, id, &doc).unwrap();
        }
    }

    #[tokio::test]
    async fn subagent_cancel_isolates_stop_failure_and_processes_remaining_subagents() {
        let s = test_state();
        seed_subagent_tree(&s);

        let body = json!({
            "command_id": "cmd-cancel-tree",
            "command_type": "conversation.subagent.cancel",
            "payload": {
                "subagent_id": "sub-parent",
                "reason": "test_fault_isolation"
            }
        });
        let Json(res) = command(s.clone(), "root".into(), body, None).await.unwrap();
        let receipt = &res["receipt"];
        assert_eq!(receipt["state"], "failed");
        assert_eq!(receipt["error"]["code"], "conversation.subagent.cancel_incomplete");

        let outcomes = receipt["output"]["outcomes"].as_array().unwrap();
        assert_eq!(outcomes.len(), 3);

        let by_id = |id: &str| outcomes.iter().find(|o| o["subagent_id"] == id).unwrap();
        let out_parent = by_id("sub-parent");
        assert!(out_parent.get("errors").is_none(), "out_parent={out_parent}");
        assert_eq!(out_parent["status"], "cancelled");
        assert_eq!(out_parent["turn_id"], "turn-parent");

        let out_fail = by_id("sub-fail");
        let fail_errors = out_fail["errors"].as_array().unwrap();
        assert!(!fail_errors.is_empty());
        assert_eq!(fail_errors[0]["code"], "conversation.history.busy");

        let out_ok = by_id("sub-ok");
        assert!(out_ok.get("errors").is_none());
        assert_eq!(out_ok["status"], "cancelled");
        assert_eq!(out_ok["turn_id"], "turn-ok");
        assert_eq!(
            out_ok["actions"],
            json!(["queue_paused", "turn_interrupted"])
        );

        cascade(
            s.clone(),
            "root".into(),
            "conversation.thread.archive".into(),
            "root-turn-1".into(),
        )
        .await
        .unwrap();
        let sibling_after = thread(&s, "sub-sibling").unwrap();
        assert_eq!(sibling_after["thread"]["archived"], true);
        assert_eq!(sibling_after["turns"][0]["status"], "cancelled");
    }
}
