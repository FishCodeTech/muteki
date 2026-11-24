//! Canonical control tools, event watches, scheduled messages and child reports.

use crate::control::{Category, Requirement, Tier};
use crate::{
    AppState, chat, control, control_content, control_lifecycle, control_observe, control_operate,
    control_subagents,
};
use serde_json::{Value, json};
use std::sync::{Arc, OnceLock};
use tcode_control_core::{control::action, notice::CONTROL_CALLBACK_PREFIX};
use tokio::sync::broadcast::error::RecvError;

const WATCHES: &str = "control_watches";

pub const REPORT_TOOL: &str = "muteki_report_result";
pub fn all_conversation_control_names() -> Vec<&'static str> {
    catalog().iter().filter_map(|row|row["name"].as_str()).collect()
}

pub fn requirement(name: &str) -> Option<Requirement> {
    if let Some(requirement)=tcode_control_mcp::tool_requirement(crate::control_api::canonical(name).trim_start_matches("muteki_")) {
        use tcode_control_mcp::ToolRequirement;
        use tcode_control_core::control::{ControlTier,ControlCategory};
        return Some(match requirement {
            ToolRequirement::Always=>Requirement::Always,ToolRequirement::Subagent=>Requirement::Subagent,
            ToolRequirement::Tier(tier)=>Requirement::Tier(match tier{ControlTier::Observe=>Tier::Observe,ControlTier::Organize=>Tier::Organize,ControlTier::Operate=>Tier::Operate}),
            ToolRequirement::Category(category)=>Requirement::Category(match category{ControlCategory::AnswerInput=>Category::AnswerInput,ControlCategory::Terminal=>Category::Terminal,ControlCategory::Delete=>Category::Delete,ControlCategory::GitRemote=>Category::GitRemote,ControlCategory::PrWrite=>Category::PrWrite,ControlCategory::HistoryEdit=>Category::HistoryEdit,ControlCategory::SettingsWrite=>Category::SettingsWrite}),
        });
    }
    match name {
        "muteki_request_grant" => Some(Requirement::Always),
        "muteki_report_result" => Some(Requirement::Subagent),
        "muteki_list_providers"
        | "muteki_get_subagents"
        | "muteki_notify_when"
        | "muteki_cancel_notify"
        | "muteki_outline_thread"
        | "muteki_read_anchors"
        | "muteki_search_in_item"
        | "muteki_file_history"
        | "muteki_reference_thread"
        | "muteki_get_git"
        | "muteki_get_pull_request"
        | "muteki_read_terminal"
        | "muteki_review_candidates"
        | "muteki_get_settings" => Some(Requirement::Tier(Tier::Observe)),
        "muteki_update_threads" | "muteki_record_review" | "muteki_settle_self" => {
            Some(Requirement::Tier(Tier::Organize))
        }
        "muteki_start_thread"
        | "muteki_send_message"
        | "muteki_cancel_thread"
        | "muteki_control_thread"
        | "muteki_respond_approval"
        | "muteki_move_thread"
        | "muteki_git"
        | "muteki_pull_request" => Some(Requirement::Tier(Tier::Operate)),
        "muteki_respond_input" => Some(Requirement::Category(Category::AnswerInput)),
        "muteki_delete_threads" => Some(Requirement::Category(Category::Delete)),
        "muteki_patch_settings" => Some(Requirement::Category(Category::SettingsWrite)),
        "muteki_rewind_thread" => Some(Requirement::Category(Category::HistoryEdit)),
        "muteki_terminal" => Some(Requirement::Category(Category::Terminal)),
        _ => None,
    }
}

/// Tools executed directly by `control_tools::invoke` (before legacy `chat_subagents` / `operations_api`).
pub fn handles(name: &str) -> bool {
    if tcode_control_mcp::tool_requirement(crate::control_api::canonical(name).trim_start_matches("muteki_")).is_some() { return true; }
    matches!(
        name,
        "muteki_request_grant"
            | "muteki_notify_when"
            | "muteki_cancel_notify"
            | "muteki_outline_thread"
            | "muteki_read_anchors"
            | "muteki_search_in_item"
            | "muteki_file_history"
            | "muteki_reference_thread"
            | "muteki_get_git"
            | "muteki_get_pull_request"
            | "muteki_read_terminal"
            | "muteki_review_candidates"
            | "muteki_get_settings"
            | "muteki_update_threads"
            | "muteki_record_review"
            | "muteki_settle_self"
            | "muteki_delete_threads"
            | "muteki_patch_settings"
            | "muteki_report_result"
            | "muteki_start_thread"
            | "muteki_send_message"
            | "muteki_control_thread"
            | "muteki_respond_input"
            | "muteki_respond_approval"
            | "muteki_rewind_thread"
            | "muteki_move_thread"
            | "muteki_git"
            | "muteki_pull_request"
            | "muteki_terminal"
    )
}

pub(crate) fn catalog() -> &'static [Value] {
    static CATALOG: OnceLock<Vec<Value>> = OnceLock::new();
    CATALOG.get_or_init(crate::control_catalog::catalog)
}

pub fn engine_arguments(arguments: &Value) -> Value {
    let mut out = arguments.clone();
    if let Some(object) = out.as_object_mut()
        && let Some(id) = object.remove("thread_id")
    {
        object.insert("subagent_id".to_owned(), id);
    }
    out
}

fn invalid(code: &str, message: impl Into<String>) -> Value {
    json!({"code": code, "category": "validation", "message": message.into()})
}

pub async fn invoke(s:&Arc<AppState>,caller:&str,name:&str,arguments:&Value)->Result<Value,Value>{
    let canonical=crate::control_api::canonical(name);
    let key=arguments["request_id"].as_str().filter(|id|!id.is_empty())
        .filter(|_|matches!(canonical,"muteki_send_message"|"muteki_control_thread"|"muteki_move_thread"))
        .map(|id|format!("{caller}:{canonical}:{id}"));
    if let Some(key)=&key { if let Some(value)=s.control_receipts.lock().await.get(key).cloned() { return Ok(value); } }
    let result=match crate::control_api::implemented(s,caller,name,arguments).await {
        Some(result)=>result,None=>Err(invalid("control.unknown_tool", format!("Unknown control tool: {name}"))),
    };
    if let (Some(key),Ok(value))=(&key,&result){s.control_receipts.lock().await.insert(key.clone(),value.clone());}
    result
}

pub async fn invoke_legacy(
    s: &Arc<AppState>,
    caller: &str,
    name: &str,
    arguments: &Value,
) -> Result<Value, Value> {
    let receipt_key = arguments["request_id"]
        .as_str()
        .filter(|id| !id.is_empty())
        .filter(|_| matches!(name, "muteki_send_message" | "muteki_control_thread"))
        .map(|id| format!("{caller}:{name}:{id}"));
    if let Some(key) = &receipt_key {
        if let Some(result) = s
            .store
            .get("control_call_receipts", key)
            .map_err(|e| json!({"code":"storage.unavailable","message":e.to_string()}))?
        {
            return Ok(result);
        }
    }
    if matches!(name, "muteki_update_threads" | "muteki_control_thread") {
        if let Some(changes) = arguments["changes"].as_array() {
            let mut results = Vec::new();
            for change in changes {
                let mut args = change.clone();
                args["dry_run"] = arguments["dry_run"].clone();
                let op = &change["op"];
                if let Some(action) = op.as_str() {
                    args["action"] = json!(action);
                } else if let Some((action, fields)) =
                    op.as_object().and_then(|map| map.iter().next())
                {
                    if op.as_object().is_some_and(|map| map.len() != 1) {
                        results.push(json!({"target":change["thread_id"],"outcome":"failed","error":{"code":"control.operation_invalid","message":"op must name exactly one operation"}}));
                        continue;
                    }
                    args["action"] = json!(action);
                    if let Some(fields) = fields.as_object() {
                        for (key, value) in fields {
                            args[key] = value.clone();
                        }
                    }
                    if action == "set_option" {
                        args["option_id"] = fields["id"].clone();
                        args["option_value"] = fields["value"].clone();
                    }
                    if action == "retry" {
                        args["turn_id"] = fields["turn"].clone();
                    }
                }
                if matches!(
                    args["action"].as_str(),
                    Some("steer_queued" | "drop_queued")
                ) {
                    args["queue_id"] = args["id"]
                        .as_str()
                        .map(|id| json!(id))
                        .unwrap_or_else(|| json!(args["id"].to_string()));
                }
                if args["action"] == "fork_at" {
                    if let Err(error) = control::authorize_call(s, caller, name, &args) {
                        results.push(
                            json!({"target":change["thread_id"],"outcome":"failed","error":error}),
                        );
                        continue;
                    }
                    let target = control::resolve_thread_id(
                        caller,
                        args["thread_id"].as_str().unwrap_or(""),
                    );
                    let doc = match chat::load(s, target) {
                        Ok(doc) => doc,
                        Err((_, body)) => {
                            results.push(
                                json!({"target":target,"outcome":"failed","error":body.0["error"]}),
                            );
                            continue;
                        }
                    };
                    let turn = args["turn"]
                        .as_u64()
                        .and_then(|index| {
                            doc["turns"]
                                .as_array()
                                .and_then(|turns| turns.get(index as usize))
                        })
                        .and_then(|turn| turn["turn_id"].as_str());
                    let Some(turn) = turn else {
                        results.push(json!({"target":target,"outcome":"failed","error":{"code":"control.fork.turn_not_found","message":"fork_at turn is not present in this thread"}}));
                        continue;
                    };
                    args["turn_id"] = json!(turn);
                }
                let result = match control::authorize_call(s, caller, name, &args) {
                    Ok(()) => Box::pin(invoke_legacy(s, caller, name, &args)).await,
                    Err(error) => Err(error),
                };
                results.push(match result {
                    Ok(result) => {
                        let item = result["outcomes"].as_array().and_then(|rows| rows.first());
                        let outcome = if item.is_some_and(|row| row["ok"] == false) {
                            "skipped"
                        } else if arguments["dry_run"] == true {
                            "would_apply"
                        } else {
                            "applied"
                        };
                        json!({"target":change["thread_id"],"outcome":outcome,"result":result})
                    }
                    Err(error) => {
                        json!({"target":change["thread_id"],"outcome":"failed","error":error})
                    }
                });
            }
            let value = json!({"results":results});
            if let Some(key) = &receipt_key {
                s.store
                    .put("control_call_receipts", key, &value)
                    .map_err(|e| json!({"code":"storage.unavailable","message":e.to_string()}))?;
            }
            return Ok(value);
        }
    }
    let result = match name {
        "muteki_request_grant" => control::request_grant(s, caller, arguments).await,
        "muteki_notify_when" => notify_when(s, caller, arguments),
        "muteki_cancel_notify" => cancel_notify(s, caller, arguments),
        "muteki_outline_thread" => control_content::outline_thread(s, caller, arguments),
        "muteki_read_anchors" => control_content::read_anchors(s, caller, arguments),
        "muteki_search_in_item" => control_content::search_in_item(s, caller, arguments),
        "muteki_file_history" => control_content::file_history(s, caller, arguments),
        "muteki_reference_thread" => control_observe::reference_thread(s, caller, arguments),
        "muteki_get_git" => control_observe::get_git(s, caller, arguments).await,
        "muteki_get_pull_request" => control_observe::get_pull_request(s, caller, arguments).await,
        "muteki_read_terminal" => control_observe::read_terminal(s, caller, arguments),
        "muteki_review_candidates" => control_observe::review_candidates(s, caller, arguments),
        "muteki_get_settings" => control_observe::get_settings_tool(s, caller),
        "muteki_update_threads" => control_lifecycle::update_threads(s, caller, arguments).await,
        "muteki_record_review" => control_lifecycle::record_review(s, caller, arguments),
        "muteki_settle_self" => control_lifecycle::settle_self(s, caller, arguments),
        "muteki_delete_threads" => control_lifecycle::delete_threads(s, caller, arguments).await,
        "muteki_patch_settings" => control_lifecycle::patch_settings(s, caller, arguments).await,
        "muteki_report_result" => control_operate::report_result(s, caller, arguments).await,
        "muteki_start_thread" => control_operate::start_thread(s, caller, arguments).await,
        "muteki_send_message" => control_operate::send_message(s, caller, arguments).await,
        "muteki_control_thread" => control_operate::control_thread(s, caller, arguments).await,
        "muteki_respond_input" => control_operate::respond_input(s, caller, arguments).await,
        "muteki_respond_approval" => control_operate::respond_approval(s, caller, arguments).await,
        "muteki_rewind_thread" => control_operate::rewind_thread(s, caller, arguments).await,
        "muteki_move_thread" => control_operate::move_thread(s, caller, arguments),
        "muteki_git" => control_operate::git_tool(s, caller, arguments).await,
        "muteki_pull_request" => control_operate::pull_request_tool(s, caller, arguments).await,
        "muteki_terminal" => control_operate::terminal_tool(s, caller, arguments),
        _ => Err(invalid(
            "control.tool_unknown",
            format!("{name} is not a control tool"),
        )),
    };
    if let (Some(key), Ok(value)) = (&receipt_key, &result) {
        s.store
            .put("control_call_receipts", key, value)
            .map_err(|e| json!({"code":"storage.unavailable","message":e.to_string()}))?;
    }
    result
}

fn running_turn(s: &AppState, thread_id: &str) -> String {
    s.store
        .get(chat::COLLECTION, thread_id)
        .ok()
        .flatten()
        .and_then(|doc| {
            doc["thread"]["state"]["running_turn_id"]
                .as_str()
                .map(str::to_owned)
        })
        .unwrap_or_default()
}

fn storage(error: impl std::fmt::Display) -> Value {
    json!({"code": "control.storage_unavailable", "category": "internal", "message": error.to_string()})
}

pub(crate) fn notify_when(s: &Arc<AppState>, caller: &str, arguments: &Value) -> Result<Value, Value> {
    let targets = arguments["thread_ids"]
        .as_array()
        .ok_or_else(|| invalid("control.watch.targets_required", "thread_ids is required"))?;
    let events = arguments["events"]
        .as_array()
        .ok_or_else(|| invalid("control.watch.events_required", "events is required"))?;
    if targets.is_empty() || events.is_empty() {
        return Err(invalid(
            "control.watch.empty",
            "thread_ids and events must not be empty",
        ));
    }
    let request = arguments["request_id"].as_str().unwrap_or("");
    if !request.is_empty() {
        let stored: Vec<_> = s.store.entries(WATCHES).map_err(storage)?.into_iter()
            .map(|(_, row)| row)
            .filter(|row| row["watcher"] == caller && row["request_id"] == request).collect();
        if let Some(first) = stored.first() {
            let mut targets: Vec<_> = stored.iter().filter_map(|row| row["target"].as_str()).collect();
            targets.sort(); targets.dedup();
            return Ok(json!({"id":first["watch_id"],"thread_ids":targets,"events":first["events"],"once":first["once"]}));
        }
    }
    let id = if request.is_empty() {
        uuid::Uuid::new_v4().to_string()
    } else {
        request.to_owned()
    };
    let (grant, _) = control::effective(s, caller).map_err(|(_, b)| b.0["error"].clone())?;
    let mut skipped = Vec::new();
    let mut accepted = Vec::new();
    for raw in targets {
        let target = control::resolve_thread_id(caller, raw.as_str().unwrap_or(""));
        if accepted.iter().any(|id| id == target) { continue; }
        if let Err(error) = control::check_target(s, caller, target, &grant) {
            skipped.push(json!({"target":target,"outcome":"skipped","reason":error["message"].as_str().map(str::to_owned).unwrap_or_else(||error.to_string())}));
            continue;
        }
        let row = json!({"watch_id":id,"request_id":request,"watcher":caller,"target":target,"events":events,"once":arguments["once"].as_bool().unwrap_or(false),"armed_turn":running_turn(s,target),"created_at":control::now()});
        s.store
            .put(WATCHES, &format!("{caller}:{id}:{target}"), &row)
            .map_err(storage)?;
        accepted.push(target.to_owned());
        control::audit(s, caller, target, action::NOTIFY_WHEN, json!(id));
    }
    if accepted.is_empty() { return Ok(json!({"skipped":skipped})); }
    accepted.sort();
    Ok(
        json!({"id":id,"thread_ids":accepted,"events":events,"once":arguments["once"].as_bool().unwrap_or(false),"skipped":skipped}),
    )
}

pub(crate) fn cancel_notify(s: &Arc<AppState>, caller: &str, arguments: &Value) -> Result<Value, Value> {
    let ids = arguments["ids"]
        .as_array()
        .ok_or_else(|| invalid("control.watch.ids_required", "ids is required"))?;
    let mut removed = Vec::new();
    for (key, row) in s.store.entries(WATCHES).map_err(storage)? {
        if row["watcher"] == caller && ids.contains(&row["watch_id"]) {
            s.store.delete(WATCHES, &key).map_err(storage)?;
            if !removed.contains(&row["watch_id"]) {
                removed.push(row["watch_id"].clone());
            }
        }
    }
    let unknown: Vec<_> = ids
        .iter()
        .filter(|id| !removed.contains(id))
        .cloned()
        .collect();
    if !removed.is_empty() {
        let detail = removed.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(", ");
        control::audit(s, caller, caller, action::CANCEL_NOTIFY, json!(detail));
    }
    Ok(json!({"removed":removed,"unknown":unknown}))
}

async fn fire(
    s: &Arc<AppState>,
    key: &str,
    watch: &Value,
    condition: &str,
    event_ref: &str,
    payload: &Value,
) {
    let watcher = watch["watcher"].as_str().unwrap_or("");
    let target = watch["target"].as_str().unwrap_or("");
    let Ok((grant, _)) = control::effective(s, watcher) else { return; };
    if control::check_target(s, watcher, target, &grant).is_err() { return; }
    let event_name = if condition == "turn_finished" { "turn_completed" } else { condition };
    if watch["last_event_ref"] == event_ref && watch["last_outcome"] == event_name { return; }
    let mut fired = watch.clone();
    fired["last_event_ref"] = json!(event_ref);
    fired["last_outcome"] = json!(event_name);
    fired["armed_turn"] = json!("");
    if let Err(error) = s.store.put(WATCHES, key, &fired) {
        tracing::error!(%error,"could not record fired watch"); return;
    }
    let once = watch["once"].as_bool().unwrap_or(true);
    if once {
        match s.store.delete(WATCHES, key) {
            Ok(true) => {
                let entries = match s.store.entries(WATCHES) {
                    Ok(entries) => entries,
                    Err(error) => {
                        tracing::error!(%error,"could not read fired watch group");
                        return;
                    }
                };
                for (other_key, other) in entries {
                    if other["watcher"] == watch["watcher"]
                        && other["watch_id"] == watch["watch_id"]
                    {
                        if let Err(error) = s.store.delete(WATCHES, &other_key) {
                            tracing::error!(%error,"could not remove fired group watch");
                        }
                    }
                }
            }
            Ok(false) => return,
            Err(error) => {
                tracing::warn!(%error, watcher, target, "control watch could not be settled");
                return;
            }
        }
    }
    let doc = chat::load(s, target).ok();
    let title = doc.as_ref().and_then(|doc| doc["thread"]["title"].as_str()).unwrap_or(target);
    let detail = watch_event_detail(doc.as_ref(), event_name, payload);
    let text = format!(
        "{CONTROL_CALLBACK_PREFIX}thread {target} ({}) {event_name}{detail}",
        quoted(title)
    );
    let dkey = format!(
        "watch:{watcher}:{target}:{event_ref}:{}",
        uuid::Uuid::new_v4().simple()
    );
    if let Err((_, axum::Json(body))) = control::deliver(s, watcher, &dkey, &text).await {
        tracing::warn!(watcher, target, error = %body, "control watch could not wake its thread");
    }
}

/// A `[control]` notice is one line: notices that race into one wake are joined with a
/// blank line, which is where the watcher splits them again.
fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// `text` as one quoted string literal, so another thread's title or question cannot end
/// the quote and read as part of the notice.
fn quoted(text: &str) -> String {
    serde_json::to_string(text).unwrap_or_default()
}

/// `[<status>, ]turn <n> from "<opening message>"`: which turn ended, numbered as `outline`
/// numbers them, so a watcher can tell one turn of the thread from the next.
fn last_turn_detail(doc: &Value, event_name: &str) -> Option<String> {
    let turns = doc["turns"].as_array()?;
    let index = turns.len().checked_sub(1)?;
    let status = if event_name == "turn_completed" {
        turns[index]["status"].as_str().map(|status| format!("{status}, "))
    } else {
        None
    };
    let turn_id = turns[index]["turn_id"].as_str().unwrap_or("");
    let opening = doc["messages"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|message| message["turn_id"] == turn_id && message["role"] == "user")
        .and_then(|message| message["text"].as_str())
        .map(|text| {
            let (text, _) = tcode_control_services::thread_index::redact(text);
            format!(" from {}", quoted(&one_line(&text)))
        });
    Some(format!(
        "{}turn {index}{}",
        status.unwrap_or_default(),
        opening.unwrap_or_default()
    ))
}

/// `: <what happened>`, read from the target or the event as it stands when the watch fires.
fn watch_event_detail(doc: Option<&Value>, event_name: &str, payload: &Value) -> String {
    let detail = match event_name {
        "idle" | "turn_completed" => doc.and_then(|doc| last_turn_detail(doc, event_name)),
        "needs_input" => {
            let question = payload["questions"]
                .as_array()
                .and_then(|questions| questions.first())
                .map(|question| one_line(question["prompt"].as_str().unwrap_or("")))
                .unwrap_or_default();
            Some(format!(
                "{} (request_id: {})",
                quoted(&question),
                payload["request_id"].as_str().unwrap_or("")
            ))
        }
        "needs_approval" => Some(one_line(&control_subagents::approval_summary(payload))),
        "settled" => payload["lifecycle"]["settled_at"]
            .as_str()
            .map(|at| format!("at {at}")),
        _ => payload["detail"].as_str().map(str::to_owned),
    };
    detail.map_or_else(String::new, |detail| format!(": {detail}"))
}

/// Server instructions for the `muteki-control` surface: the upstream text with Muteki
/// tool names, so the model reads the names it can actually call.
pub fn instructions(subagent: bool) -> String {
    let text = tcode_control_mcp::instructions(subagent);
    let replacements = [
        ("tcode_control lets you observe and operate Tcode", "muteki-control lets you observe and operate Muteki"),
        ("which Tcode opens", "which Muteki opens"),
        (
            "start_thread, send_message, control_thread, move_thread, record_review and notify_when",
            "muteki_start_thread, muteki_send_message, muteki_control_thread, muteki_move_thread, muteki_record_review and muteki_notify_when",
        ),
        ("get_thread on it", "muteki_get_thread on it"),
        ("with report_result;", "with muteki_report_result;"),
    ];
    replacements
        .into_iter()
        .fold(text, |text, (from, to)| text.replace(from, to))
}

/// Format the user's configured role-to-agent/model roster for coordinator MCP instructions.
pub fn format_orchestration_instructions(resolved: &control::ResolvedOrchestration) -> String {
    let profile = &resolved.profile;
    let enabled: Vec<&control::OrchestrationRole> =
        profile.roles.iter().filter(|role| role.enabled).collect();
    if !profile.auto_inject_instructions || enabled.is_empty() {
        return String::new();
    }
    let mode_line = match (resolved.mode, resolved.source) {
        (control::ORCHESTRATION_MODE_SRC, control::OrchestrationSource::ModeOverride) => {
            "Active mode: src (independent override)."
        }
        (control::ORCHESTRATION_MODE_SRC, control::OrchestrationSource::Default) => {
            "Active mode: src (inheriting conversation default)."
        }
        _ => "Active mode: conversation (default).",
    };
    let mut out = format!(
        "\n\n## User Multi-Agent Orchestration Roster (Muteki Setup)\n\
         {mode_line}\n\
         Choose tasks and roles based on the user's goal. Discover the contract with `muteki_collab_discover(tool=\"muteki_start_thread\")`, then call `muteki_collab_call(tool=\"muteki_start_thread\", arguments={{...}})`. For a configured role, the target `muteki_start_thread` arguments include `subagent: true` and `role`; Muteki resolves its routing and appends its guidelines to the child message. A pinned role binds its configured provider/profile/model/reasoningEffort; conflicting arguments fail. For a distinct explicitly routed task, omit role. Native subagents run under their own harness: selecting a model there does not dispatch the configured provider or credential. Do not use native swarm or shell CLI processes as substitutes for configured cross-provider delegation. Dispatch only work the goal needs, and confirm real thread IDs before claiming agents started. Child reports are delivered automatically and wake this thread; when only child work remains, end the current turn to await reports instead of repeatedly polling or using native swarm waits. Continue independent work while children run, then integrate their reports and verify the requested outcome:\n"
    );
    let default_role = profile.default_role.trim();
    if !default_role.is_empty()
        && let Some(matched) = enabled
            .iter()
            .find(|role| role.id.eq_ignore_ascii_case(default_role))
    {
        out.push_str(&format!(
            "Default role when omitted (with no explicit provider/model): `{}`.\n",
            matched.id
        ));
    }
    for role in enabled {
        let routing = if role.mode == "pinned" && !role.provider.is_empty() {
            let mut parts = vec![format!("provider={}", role.provider)];
            if !role.profile.is_empty() {
                parts.push(format!("profile={}", role.profile));
            }
            if !role.model.is_empty() {
                parts.push(format!("model={}", role.model));
            }
            if !role.effort.is_empty() {
                parts.push(format!("reasoningEffort={}", role.effort));
            }
            parts.join(", ")
        } else if !role.effort.is_empty() {
            format!(
                "inherit current session agent/model (suggested reasoningEffort={}; for standalone threads set provider/model from current session or muteki_list_providers)",
                role.effort
            )
        } else {
            "inherit current session agent/model".to_owned()
        };
        out.push_str(&format!("- role `{}` ({}): {}\n", role.id, role.label, routing));
        if !role.description.is_empty() {
            out.push_str(&format!("  scope: {}\n", role.description));
        }
        if !role.guidelines.is_empty() {
            out.push_str(&format!("  guidelines: {}\n", role.guidelines));
        }
    }
    out.push_str("Inspect actual provider/profile/model availability and model reasoning metadata via `muteki_list_providers`; read this roster via `muteki_get_settings` (key `orchestration`). Native worker identities and completion are only what their harness reports; do not claim a configured provider ran without a managed thread and its runtime evidence.\n");
    out
}

pub fn instructions_for_thread(s: &AppState, thread_id: &str) -> String {
    let subagent = control::is_subagent(s, thread_id);
    let mut text = instructions(subagent);
    if !subagent && let Ok(resolved) = control::effective_orchestration(s, thread_id) {
        text.push_str(&format_orchestration_instructions(&resolved));
    }
    text
}

fn has_active_child_orchestration(s: &AppState, thread_id: &str) -> bool {
    if let Ok(doc) = crate::chat::load(s, thread_id)
        && let Some(subs) = doc["thread"]["state"]["subagents"].as_array()
        && subs.iter().any(|sub| {
            matches!(
                sub["delivery"].as_str(),
                Some("running" | "awaiting_settle")
            )
        })
    {
        return true;
    }
    false
}

async fn check_armed_self_settle_on_turn_end(s: &Arc<AppState>, thread_id: &str) {
    let Ok(ctrl_doc) = control::thread_doc(s, thread_id) else {
        return;
    };
    if ctrl_doc["self_settle"]["when"] == "end_of_turn" {
        if has_active_child_orchestration(s, thread_id) {
            return;
        }
        let summary = ctrl_doc["self_settle"]["summary"]
            .as_str()
            .filter(|v| !v.trim().is_empty())
            .map(str::to_owned);
        let _ =
            control_lifecycle::settle_thread_now(s, thread_id, thread_id, summary.as_deref()).await;
    }
}

fn disarm_self_settle_on_human_turn(s: &AppState, thread_id: &str, event: &Value) {
    let new_turn = event["payload"]["turn_id"].as_str().unwrap_or("");
    if new_turn.is_empty() {
        return;
    }
    if let Ok(doc) = crate::chat::load(s, thread_id) {
        let human = doc["messages"].as_array().into_iter().flatten().any(|message| {
            message["turn_id"].as_str()==Some(new_turn) && message["role"]=="user"
                && message["origin"].as_str().map_or(!control::is_subagent(s,thread_id),|origin|origin=="human")
        });
        if !human { return; }
    }
    let Ok(mut ctrl_doc) = control::thread_doc(s, thread_id) else {
        return;
    };
    let mut changed = false;
    if !ctrl_doc["canonical_self_settle"].is_null() { ctrl_doc["canonical_self_settle"] = Value::Null; changed=true; }
    if !ctrl_doc["self_settle"].is_null() {
        let armed_turn = ctrl_doc["self_settle"]["armed_turn_id"]
            .as_str()
            .unwrap_or("");
        if new_turn != armed_turn {
            ctrl_doc["self_settle"] = Value::Null;
            changed = true;
        }
    }
    if ctrl_doc["lifecycle"]["settled"].as_bool() == Some(true) {
        ctrl_doc["lifecycle"]["settled"] = json!(false);
        changed = true;
    }
    if changed {
        let _ = control::save_thread_doc(s, thread_id, &ctrl_doc);
        let lifecycle = control::read_lifecycle(&ctrl_doc);
        let _ = control::sync_lifecycle_mirror(s, thread_id, &lifecycle);
    }
}

/// Background orchestration loop running for the life of the process.
pub async fn watch_loop(s: Arc<AppState>) {
    let mut events = s.chat.subscribe();
    tokio::spawn(crate::control_pr::run_watches(s.clone()));
    let mut ticker = tokio::time::interval(std::time::Duration::from_secs(1));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut settlement_cache = crate::control_automation::SweepCache::default();
    let mut reports_recovered = false;
    if crate::business::require(&s,crate::business::Module::Chat).is_ok() {
        match control_subagents::recover(&s).await {
            Ok(()) => reports_recovered = true,
            Err(error) => tracing::error!(%error, "could not restore child reports"),
        }
    }
    if let Err(error)=crate::control_native_agents::recover(&s).await { tracing::error!(%error,"native agent pages could not be recovered"); }
    sweep(&s).await;

    loop {
        tokio::select! {
            _ = ticker.tick() => {
                if !reports_recovered && crate::business::require(&s,crate::business::Module::Chat).is_ok() {
                    match control_subagents::recover(&s).await {
                        Ok(()) => reports_recovered = true,
                        Err(error) => tracing::error!(%error, "could not restore child reports"),
                    }
                }
                control_operate::sweep_scheduled_messages(&s).await;
                if let Err(error)=crate::control_automation::sweep(&s, &mut settlement_cache).await { tracing::error!(%error,"settlement sweep failed"); }
            }
            recv = events.recv() => {
                match recv {
                    Ok((thread, kind, event)) if kind == "event" => {
                        let etype = event["event_type"].as_str().unwrap_or("");
                        match etype {
                            "core.agent.event" => { if let Err(error)=crate::control_native_agents::mirror(&s,&thread,&event).await { tracing::error!(%error,"native transcript mirror failed"); } }
                            "core.control.message_accepted" | "core.turn.steered" => {
                                if let Some(command) = event["payload"]["command_id"].as_str().or_else(||event["payload"]["client_message_id"].as_str()) {
                                    if let Err(error) = control_subagents::accepted(&s, &thread, command).await { tracing::error!(%error,"callback acknowledgement failed"); }
                                }
                            }
                            "core.approval.requested" => {
                                if let Err(error) = control_subagents::attention(&s, &thread, &event, "approval").await { tracing::error!(%error, "child approval callback failed"); }
                                fire_matching(&s, &thread, &["needs_approval"], &event["stream_seq"].to_string(), &event["payload"]).await;
                            }
                            "core.user_input.requested" => {
                                if let Err(error) = control_subagents::attention(&s, &thread, &event, "input").await { tracing::error!(%error, "child input callback failed"); }
                                fire_matching(&s, &thread, &["needs_input"], &event["stream_seq"].to_string(), &event["payload"]).await;
                            }
                            "core.control.updated" if event["payload"]["kind"] == "settled" => {
                                fire_matching(&s, &thread, &["settled"], &event["stream_seq"].to_string(), &event["payload"]).await;
                            }
                            "core.turn.started" => {
                                disarm_self_settle_on_human_turn(&s, &thread, &event);
                            }
                            "core.session.background.updated" if event["payload"]["count"] == 0 => {
                                if let Err(error) = control_subagents::on_turn_ended(&s, &thread, "completed", "").await { tracing::error!(%error, "child background report delivery failed"); }
                                check_armed_self_settle_on_turn_end(&s, &thread).await;
                                if let Err(error) = crate::control_actions::self_due(&s, &thread, true).await { tracing::error!(%error, "background completion self-settlement failed"); }
                            }
                            "core.turn.completed" | "core.turn.failed" | "core.turn.interrupted" => {
                                let outcome = match etype {
                                    "core.turn.completed" => "completed",
                                    "core.turn.failed" => "failed",
                                    _ => "interrupted",
                                };
                                let turn_id = event["payload"]["turn_id"].as_str().unwrap_or("").to_owned();
                                let raw_error = &event["payload"]["error"];
                                let raw_err = if raw_error.is_null() { String::new() } else if let Some(text) = raw_error.as_str() { text.to_owned() } else { raw_error.to_string() };

                                let doc = chat::load(&s, &thread).ok();
                                let idle = doc.as_ref().is_some_and(|doc| !control_subagents::busy(doc) && doc["queue"].as_array().is_none_or(Vec::is_empty));
                                let conditions: &[&str] = if idle { &["turn_finished", "turn_completed", "idle"] } else { &["turn_finished", "turn_completed"] };
                                fire_matching(
                                    &s,
                                    &thread,
                                    conditions,
                                    &turn_id,
                                    &json!({}),
                                ).await;

                                if let Err(error) = control_subagents::on_turn_ended(&s, &thread, outcome, &raw_err).await {
                                    tracing::error!(%error, thread, "could not deliver child result");
                                }
                                if let Err(error)=control_subagents::resume_parent(&s,&thread).await { tracing::error!(%error,"pending child report delivery failed"); }
                                if etype == "core.turn.completed" {
                                    check_armed_self_settle_on_turn_end(&s, &thread).await;
                                    if let Err(error)=crate::control_actions::self_due(&s,&thread,true).await { tracing::error!(%error,"self-settlement failed"); }
                                }
                            }
                            _ => {}
                        }
                    }
                    Ok(_) => {}
                    Err(RecvError::Lagged(_)) => sweep(&s).await,
                    Err(RecvError::Closed) => return,
                }
            }
        }
    }
}

pub(crate) async fn external_event(s:&Arc<AppState>,target:&str,event:&str,detail:&str) {
    fire_matching(s,target,&[event],&format!("{event}:{detail}"),&json!({"detail":detail})).await;
}

async fn fire_matching(
    s: &Arc<AppState>,
    target: &str,
    conditions: &[&str],
    event_ref: &str,
    payload: &Value,
) {
    let Ok(entries) = s.store.entries(WATCHES) else {
        return;
    };
    let mut grouped = std::collections::BTreeMap::<String, Vec<(String, String, Value)>>::new();
    for (key, watch) in entries {
        let cond = watch["condition"].as_str().unwrap_or("turn_finished");
        let matched = if conditions.contains(&cond) {
            Some(cond.to_owned())
        } else {
            watch["events"].as_array().into_iter().flatten()
                .filter_map(Value::as_str)
                .find(|event| conditions.contains(event))
                .map(str::to_owned)
        };
        if watch["target"] == target && let Some(matched) = matched {
            grouped.entry(watch["watcher"].as_str().unwrap_or("").to_owned()).or_default().push((key, matched, watch));
        }
    }
    for (watcher, matches) in grouped {
        let Ok((grant, _)) = control::effective(s, &watcher) else { continue; };
        if control::check_target(s, &watcher, target, &grant).is_err() { continue; }
        let mut delivered = false;
        for (key, matched, watch) in matches {
            let event_name = if matched == "turn_finished" { "turn_completed" } else { matched.as_str() };
            if watch["last_event_ref"] == event_ref && watch["last_outcome"] == event_name { continue; }
            if !delivered {
                fire(s, &key, &watch, &matched, event_ref, payload).await;
                delivered = true;
            } else if watch["once"] == true {
                let others = match s.store.entries(WATCHES) { Ok(rows) => rows, Err(error) => { tracing::error!(%error,"could not read fired watch group"); continue; } };
                for (other_key, other) in others {
                    if other["watcher"] == watch["watcher"] && other["watch_id"] == watch["watch_id"] {
                        if let Err(error) = s.store.delete(WATCHES, &other_key) { tracing::error!(%error,"could not remove fired watch"); }
                    }
                }
            } else {
                let mut fired = watch;
                fired["last_event_ref"] = json!(event_ref);
                fired["last_outcome"] = json!(event_name);
                fired["armed_turn"] = json!("");
                if let Err(error) = s.store.put(WATCHES, &key, &fired) { tracing::error!(%error,"could not record fired watch"); }
            }
        }
    }
}

/// Fire watches whose armed turn ended while no event reached the loop, and sweep scheduled messages.
async fn sweep(s: &Arc<AppState>) {
    control_operate::sweep_scheduled_messages(s).await;
    let Ok(entries) = s.store.entries(WATCHES) else {
        return;
    };
    let mut ended = std::collections::BTreeSet::new();
    for (_, watch) in entries {
        let armed = watch["armed_turn"].as_str().unwrap_or("");
        let target = watch["target"].as_str().unwrap_or("");
        if !armed.is_empty() && running_turn(s, target) != armed { ended.insert((target.to_owned(), armed.to_owned())); }
    }
    for (target, turn_id) in ended {
        let Ok(doc) = chat::load(s, &target) else { continue; };
        let Some(turn) = doc["turns"].as_array().into_iter().flatten().find(|turn| turn["turn_id"] == turn_id) else { continue; };
        let Some("completed" | "failed" | "interrupted" | "cancelled") = turn["status"].as_str() else { continue; };
        let idle = !control_subagents::busy(&doc) && doc["queue"].as_array().is_none_or(Vec::is_empty);
        let conditions: &[&str] = if idle { &["turn_finished", "turn_completed", "idle"] } else { &["turn_finished", "turn_completed"] };
        fire_matching(s, &target, conditions, &turn_id, &json!({})).await;
    }
}
