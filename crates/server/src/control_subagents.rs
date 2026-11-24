//! Report obligations and delivery for application-owned child threads.
use crate::{AppState, chat, control, control_content};
use serde_json::{Value, json};
use std::sync::Arc;
use tcode_control_core::notice;
use tcode_core::settlement::AgentDelivery;

const REMINDER: &str = "[Muteki] Your turn ended without report_result. Your parent sees only your report, not your transcript or background work. If finished, call muteki_report_result with your complete final report and end the turn. If still working, continue. You will be reminded once; a later quiet end without a report delivers your final message labelled unreported.";

pub fn open(doc: &mut Value) {
    doc["thread"]["state"]["delivery_settled"] = json!(false);
    doc["thread"]["control_lifecycle"]["settled"] = json!(false);
    let previous = doc["thread"]["state"]["subagent_report"]["last_report"]["id"].clone();
    let last = doc["thread"]["state"]["subagent_report"]["last_report"].clone();
    let pending = doc["thread"]["state"]["subagent_report"]["pending_reports"].clone();
    let legacy = doc["thread"]["state"]["subagent_report"]["pending_delivery"].clone();
    doc["thread"]["state"]["subagent_report"] = json!({
        "state": "expected", "opened_at": control::now(),
        "previous_report_id": previous, "last_report": last, "reminded": false,
        "pending_reports": pending, "pending_delivery": legacy,
    });
}

pub fn open_for_message(doc: &mut Value, body: &Value) {
    if body["payload"]["control_scheduled"] == true {
        return;
    }
    let parent = doc["thread"]["state"]["lineage"]["parent_thread_id"]
        .as_str()
        .unwrap_or("");
    let kind = body["command_type"].as_str().unwrap_or("");
    if !parent.is_empty()
        && body["actor"]["id"] == parent
        && matches!(kind, "conversation.turn.send" | "conversation.turn.steer")
    {
        open(doc);
    }
}

pub fn busy(doc: &Value) -> bool {
    doc["thread"]["state"]["status"] == "running"
        || doc["native_session"]["background_tasks"]
            .as_u64()
            .unwrap_or(0)
            > 0
        || doc["thread"]["state"]["subagents"]
            .as_array()
            .into_iter()
            .flatten()
            .any(|node| node["delivery"].as_str() == Some("running"))
}

/// What a child thread is blocked on, so the parent's panel can say which.
pub fn waiting_for(doc: &Value) -> Option<&'static str> {
    let state = &doc["thread"]["state"];
    if state["pending_approvals"]
        .as_object()
        .is_some_and(|rows| rows.values().any(|row| row["status"] == "pending"))
    {
        Some("approval")
    } else if state["pending_user_input"]["status"] == "pending" {
        Some("answer")
    } else {
        None
    }
}

pub fn execution(doc: &Value, status: &str) -> &'static str {
    if waiting_for(doc).is_some() {
        return "waiting";
    }
    match status {
        "running" | "pending" => "working",
        "failed" => "failed",
        "cancelled" => "cancelled",
        "interrupted" => "interrupted",
        _ if busy(doc) => "background",
        _ => "finished",
    }
}

pub fn delivery(doc: &Value, status: &str) -> AgentDelivery {
    delivery_with_cancel(doc, status, false)
}

pub fn delivery_with_cancel(doc: &Value, status: &str, cancel_requested: bool) -> AgentDelivery {
    if doc["thread"]["state"]["delivery_settled"] == true {
        return AgentDelivery::Settled;
    }
    let cancelled = cancel_requested
        || doc["thread"]["state"]["subagent_report"]["cancelled"] == true;
    if doc["thread"]["archived"] == true
        || status == "cancelled"
        || (cancelled && !matches!(status, "running" | "pending"))
    {
        return AgentDelivery::NotDelivered;
    }
    match doc["thread"]["state"]["subagent_report"]["state"].as_str() {
        Some("expected") if status == "interrupted" && !busy(doc) => AgentDelivery::NotDelivered,
        Some("expected") => AgentDelivery::Running,
        Some("reported" | "unreported" | "failed") => AgentDelivery::AwaitingSettle,
        Some("interrupted") => AgentDelivery::NotDelivered,
        Some("none") if cancelled || status == "interrupted" => AgentDelivery::NotDelivered,
        _ if matches!(status, "running" | "pending") || busy(doc) => AgentDelivery::Running,
        _ if status == "interrupted" => AgentDelivery::NotDelivered,
        _ => AgentDelivery::AwaitingSettle,
    }
}

pub async fn report(s: &Arc<AppState>, caller: &str, args: &Value) -> Result<Value, Value> {
    let text = args["text"].as_str().filter(|text| !text.trim().is_empty()).ok_or_else(||
        json!({"code":"control.report.text_required","category":"validation","message":"text must contain the complete final report"}))?;
    let guard = s.chat.active.lock().await;
    let mut doc = chat::load(s, caller).map_err(|(_, body)| body.0["error"].clone())?;
    let id = uuid::Uuid::new_v4().to_string();
    let turn = doc["thread"]["state"]["running_turn_id"].clone();
    doc["thread"]["state"]["subagent_report"]["last_report"] = json!({"id": id, "text": text, "turn_id": turn});
    chat::save(s, caller, &mut doc).map_err(|(_, body)| body.0["error"].clone())?;
    drop(guard);
    let mut ctrl = control::thread_doc(s, caller).map_err(|(_, body)| body.0["error"].clone())?;
    ctrl["structured_report"] =
        json!({"text":text,"summary":text,"status":"completed","reported_at":control::now()});
    control::save_thread_doc(s, caller, &ctrl).map_err(|(_, body)| body.0["error"].clone())?;
    control::audit(s, caller, caller, "report_result", json!({"report_id":id}));
    crate::runs::src_api::update_delegated_todo_on_report(s, caller, text);
    Ok(json!({"ok":true,"chars":text.chars().count(),"delivered_when":"turn_ends"}))
}

fn stage_delivery(doc: &mut Value, child_id: &str, state: &str, text: &str, report_id: &str) {
    let title = doc["thread"]["title"].as_str().unwrap_or(child_id);
    let updated = state == "reported" && doc["thread"]["state"]["subagent_report"]["delivered_report_id"].is_string();
    let word = if updated { "updated" } else { state };
    let body = match state {
        "reported" => format!("{}{text}", notice::ORCHESTRATE_REPORT_LABEL),
        "unreported" => format!("{}{}", notice::ORCHESTRATE_FALLBACK_LABEL, if text.is_empty() { "(no assistant output)" } else { text }),
        _ => text.to_owned(),
    };
    let message = format!("{} {word}.{}\n{body}", notice::callback_header(child_id, title), token_segment(&doc["thread"]["state"]["usage"]));
    let key = format!("child-report:{child_id}:{report_id}");
    let report = &mut doc["thread"]["state"]["subagent_report"];
    report["state"] = json!(state);
    report["delivered_report_id"] = json!(report_id);
    if !report["pending_reports"].is_array() { report["pending_reports"] = json!([]); }
    let rows = report["pending_reports"].as_array_mut().unwrap();
    if !rows.iter().any(|row|row["key"] == key) { rows.push(json!({"key":key,"text":message})); }
}

/// Freeze a completed report before starting the next queued assignment.
pub(crate) fn stage_finished_report(doc: &mut Value, child_id: &str) {
    if doc["thread"]["state"]["status"] == "running" || doc["thread"]["archived"] == true
        || !doc["thread"]["state"]["lineage"]["parent_thread_id"].is_string() { return; }
    let report = &doc["thread"]["state"]["subagent_report"];
    if report["state"] == "none" { return; }
    let last = report["last_report"].clone();
    if let Some(id) = last["id"].as_str()
        && last["id"] != report["previous_report_id"] && last["id"] != report["delivered_report_id"]
    { stage_delivery(doc, child_id, "reported", last["text"].as_str().unwrap_or(""), id); }
}
fn has_pending(doc: &Value) -> bool {
    let report = &doc["thread"]["state"]["subagent_report"];
    report["pending_delivery"].is_object() || report["pending_reports"].as_array().is_some_and(|rows|!rows.is_empty())
}
pub(crate) fn pending_text(doc: &Value) -> Option<String> {
    let report = &doc["thread"]["state"]["subagent_report"];
    let texts = report["pending_delivery"]["text"].as_str().into_iter()
        .chain(report["pending_reports"].as_array().into_iter().flatten().filter_map(|row|row["text"].as_str())).collect::<Vec<_>>();
    (!texts.is_empty()).then(||texts.join("\n\n"))
}
async fn dispatch(s: &Arc<AppState>, child_id: &str, state: &str, text: &str, report_id: &str) -> Result<(), Value> {
    let guard = s.chat.active.lock().await;
    let mut doc = chat::load(s, child_id).map_err(|(_, body)|body.0["error"].clone())?;
    if !doc["thread"]["state"]["lineage"]["parent_thread_id"].is_string() { return Ok(()); }
    stage_delivery(&mut doc, child_id, state, text, report_id);
    chat::save(s, child_id, &mut doc).map_err(|(_, body)|body.0["error"].clone())?;
    drop(guard);
    deliver_pending(s, child_id).await
}
pub async fn deliver_pending(s: &Arc<AppState>, child_id: &str) -> Result<(), Value> {
    let doc = chat::load(s, child_id).map_err(|(_, body)|body.0["error"].clone())?;
    let parent = doc["thread"]["state"]["lineage"]["parent_thread_id"].as_str().unwrap_or("").to_owned();
    let report = &doc["thread"]["state"]["subagent_report"];
    let mut rows = Vec::new();
    if report["pending_delivery"].is_object() { rows.push(report["pending_delivery"].clone()); }
    rows.extend(report["pending_reports"].as_array().into_iter().flatten().cloned());
    for row in rows {
        let key = row["key"].as_str().ok_or_else(||json!({"code":"control.report_invalid","message":"Pending report has no delivery key"}))?;
        let text = row["text"].as_str().ok_or_else(||json!({"code":"control.report_invalid","message":"Pending report has no text"}))?;
        let current = chat::load(s, child_id).map_err(|(_, body)|body.0["error"].clone())?;
        let state = &current["thread"]["state"]["subagent_report"];
        if state["state"] == "none" { return Ok(()); }
        if state["pending_delivery"]["key"] != key && !state["pending_reports"].as_array().is_some_and(|rows|rows.iter().any(|row|row["key"] == key)) { continue; }
        control::deliver(s, &parent, key, text).await.map_err(|(_, body)|body.0["error"].clone())?;
        if control::message_accepted(s, &parent, key).map_err(|(_, body)|body.0["error"].clone())? {
            accepted(s, &parent, &format!("control:{key}")).await?;
        }
    }
    Ok(())
}
pub async fn accepted(s: &Arc<AppState>, parent: &str, command: &str) -> Result<(), Value> {
    let Some(key) = command.strip_prefix("control:") else { return Ok(()); };
    let guard = s.chat.active.lock().await;
    for mut doc in s.store.list(chat::COLLECTION).map_err(|error|json!({"message":error.to_string()}))? {
        if doc["thread"]["state"]["lineage"]["parent_thread_id"] != parent { continue; }
        let id = doc["thread"]["thread_id"].as_str().unwrap().to_owned();
        let report = &mut doc["thread"]["state"]["subagent_report"];
        let mut changed = false;
        if report["pending_delivery"]["key"] == key { report["pending_delivery"] = Value::Null; changed = true; }
        if let Some(rows) = report["pending_reports"].as_array_mut() {
            let before = rows.len(); rows.retain(|row|row["key"] != key); changed |= before != rows.len();
        }
        if changed { chat::save(s, &id, &mut doc).map_err(|(_, body)|body.0["error"].clone())?; }
    }
    drop(guard);
    crate::control_pr::accepted(s, parent, key)
}

pub async fn on_turn_ended(
    s: &Arc<AppState>,
    child_id: &str,
    outcome: &str,
    error: &str,
) -> Result<(), Value> {
    let mut doc = chat::load(s, child_id).map_err(|(_, b)| b.0["error"].clone())?;
    let parent = doc["thread"]["state"]["lineage"]["parent_thread_id"]
        .as_str()
        .unwrap_or("");
    if parent.is_empty() || doc["thread"]["archived"] == true {
        return Ok(());
    }
    if crate::chat_subagents::snapshot(s, child_id, false).map_err(|(_, b)| b.0["error"].clone())?["cancel_requested"]
        == true
    {
        return Ok(());
    }
    deliver_pending(s, child_id).await?;
    doc = chat::load(s, child_id).map_err(|(_, body)|body.0["error"].clone())?;
    let state = doc["thread"]["state"]["subagent_report"].clone();
    let last = &state["last_report"];
    if doc["thread"]["state"]["status"] == "running" && last["turn_id"].is_string()
        && last["turn_id"] == doc["thread"]["state"]["running_turn_id"] { return Ok(()); }
    if let Some(id) = last["id"].as_str()
        && last["id"] != state["previous_report_id"]
        && last["id"] != state["delivered_report_id"]
    {
        return dispatch(
            s,
            child_id,
            "reported",
            last["text"].as_str().unwrap_or(""),
            id,
        )
        .await;
    }
    if state["state"] != "expected" {
        return Ok(());
    }
    if matches!(outcome, "interrupted" | "was interrupted") {
        if !busy(&doc)
            && doc["queue"]
                .as_array()
                .is_none_or(|queue| queue.is_empty())
        {
            doc["thread"]["state"]["subagent_report"]["state"] = json!("interrupted");
            chat::save(s, child_id, &mut doc).map_err(|(_, b)| b.0["error"].clone())?;
        }
        return Ok(());
    }
    if outcome == "failed" {
        let id = uuid::Uuid::new_v4().to_string();
        return dispatch(s, child_id, "failed", error, &id).await;
    }
    if busy(&doc)
        || doc["queue"]
            .as_array()
            .is_some_and(|queue| !queue.is_empty())
    {
        return Ok(());
    }
    let adapter = doc["runtime"]["adapter_id"].as_str().unwrap_or("");
    let has_mcp = crate::agent_runtime::descriptors::for_adapter(adapter)
        .is_some_and(|(_, a)| a.capabilities.mcp)
        && doc["native_session"]["mcp_attached"] != false;
    if state["reminded"] != true && has_mcp {
        doc["thread"]["state"]["subagent_report"]["reminded"] = json!(true);
        chat::save(s, child_id, &mut doc).map_err(|(_, b)| b.0["error"].clone())?;
        control::deliver(
            s,
            child_id,
            &format!("report-reminder:{child_id}:{}", state["opened_at"]),
            REMINDER,
        )
        .await
        .map_err(|(_, b)| b.0["error"].clone())?;
        return Ok(());
    }
    let (_, items) = control_content::index_thread(s, child_id)?;
    let text = items
        .iter()
        .rev()
        .find(|item| item.kind == "assistant_message")
        .map(|item| item.content.as_str())
        .unwrap_or("(no assistant output)");
    dispatch(
        s,
        child_id,
        "unreported",
        text,
        &uuid::Uuid::new_v4().to_string(),
    )
    .await
}

pub async fn resume_parent(s: &Arc<AppState>, parent: &str) -> Result<(), Value> {
    for doc in s
        .store
        .list(chat::COLLECTION)
        .map_err(|error| json!({"message":error.to_string()}))?
    {
        if doc["thread"]["state"]["lineage"]["parent_thread_id"] == parent
            && has_pending(&doc)
        {
            if let Some(id) = doc["thread"]["thread_id"].as_str() {
                deliver_pending(s, id).await?;
            }
        }
    }
    Ok(())
}

pub async fn recover(s: &Arc<AppState>) -> Result<(), Value> {
    for doc in s
        .store
        .list(chat::COLLECTION)
        .map_err(|e| json!({"code":"storage.unavailable","message":e.to_string()}))?
    {
        if has_pending(&doc) {
            if let Some(id) = doc["thread"]["thread_id"].as_str() {
                deliver_pending(s, id).await?;
            }
        }
    }
    Ok(())
}

pub async fn attention(
    s: &Arc<AppState>,
    child_id: &str,
    event: &Value,
    kind: &str,
) -> Result<(), Value> {
    let doc = chat::load(s, child_id).map_err(|(_, b)| b.0["error"].clone())?;
    let Some(parent) = doc["thread"]["state"]["lineage"]["parent_thread_id"].as_str() else {
        return Ok(());
    };
    let mode = control::settings(s)
        .map_err(|(_, b)| b.0["error"].clone())?
        .subagent_policy
        .approval_mode;
    let manual = mode == "manual";
    let payload = &event["payload"];
    let title = doc["thread"]["title"].as_str().unwrap_or(child_id);
    let header = notice::callback_header(child_id, title);
    let (id, message) = if kind == "approval" {
        let id = payload["approval_id"].as_str().unwrap_or("");
        if mode == "always_allow" && auto_allow(s, child_id, id, payload).await {
            return Ok(());
        }
        let options: Vec<_> = payload["options"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|option| json!({"id": option["option_id"], "label": option["label"], "kind": option["kind"]}))
            .collect();
        let instruction = if manual {
            "The user answers in the child thread."
        } else {
            "You are the approver: answer with muteki_respond_approval, passing this thread_id and approval_id with option: <exact id> or cancel: true; reject anything outside the brief's scope."
        };
        (id, format!(
            "{header} {}{} (approval_id: {id}). Native options: {}. {instruction}",
            notice::ORCHESTRATE_APPROVAL_PHRASE, approval_summary(payload), Value::Array(options)
        ))
    } else {
        let id = payload["request_id"].as_str().unwrap_or("");
        let delivery = if payload["native"]["background"] == true {
            "the child keeps working and reads the answer when it arrives; the question lapses when its turn ends"
        } else {
            "the child is blocked until it is answered"
        };
        let instruction = if manual {
            "The user answers in the child thread."
        } else {
            "You are the decider: answer with muteki_respond_input, passing this thread_id and question_id with answers keyed by question id, a string (an option label or free text) for a single-choice question and an array of strings for a multi_select one, deciding from the brief and the user's instructions."
        };
        (id, format!(
            "{header} {}question_id: {id}; {delivery}). Questions: {}. {instruction}",
            notice::ORCHESTRATE_QUESTION_PHRASE, payload["questions"]
        ))
    };
    control::deliver(
        s,
        parent,
        &format!("child-attention:{child_id}:{kind}:{id}"),
        &message,
    )
    .await
    .map_err(|(_, b)| b.0["error"].clone())
}

/// Answers a child's approval with its native allow option in `always_allow` mode, so the lead is not woken; false when no option exists or the answer failed.
async fn auto_allow(s: &Arc<AppState>, child_id: &str, approval_id: &str, row: &Value) -> bool {
    let options = row["options"].as_array().cloned().unwrap_or_default();
    let pick = |kind: &str| options.iter().find(|option| option["kind"] == kind);
    let Some(option) = pick("allow_always").or_else(|| pick("allow_once")) else {
        return false;
    };
    let body = json!({"command_id": format!("control-auto-approval:{child_id}:{approval_id}"), "command_type": "conversation.approval.resolve",
        "payload": {"approval_id": approval_id, "decision": "allow", "option_id": option["option_id"]}, "actor": {"kind": "system", "id": "muteki-control"}});
    match chat::command_from_platform(s.clone(), child_id.to_owned(), body, None).await {
        Ok(_) => true,
        Err((_, axum::Json(body))) => {
            tracing::warn!(child_id, approval_id, error = %body, "could not auto-answer child approval");
            false
        }
    }
}

/// One-line approval summary as the lead and a watcher read it.
pub(crate) fn approval_summary(row: &Value) -> String {
    let text = |key: &str| row[key].as_str().unwrap_or("");
    match row["approval_kind"].as_str().unwrap_or("") {
        "command_execution" => format!("command `{}`", text("command")),
        "file_change" => match row["paths"].as_array().map(Vec::as_slice) {
            Some([path]) => format!("file change `{}`", path.as_str().unwrap_or("")),
            Some(paths) => format!("{} file changes", paths.len()),
            None => format!("file change `{}`", text("path")),
        },
        "plan_exit" => format!("plan: {}", text("plan_markdown")),
        _ if row["input"].is_null() && text("tool_name") == "read" => format!("file read `{}`", text("reason")),
        _ => format!("tool `{}`", text("tool_name")),
    }
}

/// ` tokens: input N (+C cached), output N, total N.` or empty when the thread reported no usage.
fn token_segment(usage: &Value) -> String {
    let count = |key: &str| usage[key].as_u64();
    let mut parts = Vec::new();
    if let Some(input) = count("input_tokens") {
        let cached = count("cache_read_tokens").filter(|cached| *cached > 0).map(|cached| format!(" (+{cached} cached)")).unwrap_or_default();
        parts.push(format!("input {input}{cached}"));
    }
    if let Some(output) = count("output_tokens") {
        parts.push(format!("output {output}"));
    }
    if let Some(total) = count("total_processed_tokens").or_else(|| count("context_used")) {
        parts.push(format!("total {total}"));
    }
    if parts.is_empty() { String::new() } else { format!(" tokens: {}.", parts.join(", ")) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn busy_and_delivery_with_cancel_distinguish_running_awaiting_and_cancelled() {
        let awaiting_doc = json!({
            "thread": {
                "archived": false,
                "state": {
                    "status": "completed",
                    "delivery_settled": false,
                    "subagent_report": {"state": "reported", "cancelled": false},
                    "subagents": [
                        {"agent_id": "c1", "delivery": "awaiting_settle"}
                    ]
                }
            },
            "native_session": {"background_tasks": 0}
        });
        assert!(!busy(&awaiting_doc));
        assert_eq!(
            delivery_with_cancel(&awaiting_doc, "completed", false),
            AgentDelivery::AwaitingSettle
        );

        let mut running_child = awaiting_doc.clone();
        running_child["thread"]["state"]["subagents"] =
            json!([{"agent_id": "c1", "delivery": "running"}]);
        assert!(busy(&running_child));

        let mut settled_doc = awaiting_doc.clone();
        settled_doc["thread"]["state"]["delivery_settled"] = json!(true);
        assert_eq!(
            delivery_with_cancel(&settled_doc, "completed", false),
            AgentDelivery::Settled
        );

        let mut failed_doc = awaiting_doc.clone();
        failed_doc["thread"]["state"]["status"] = json!("failed");
        failed_doc["thread"]["state"]["subagent_report"]["state"] = json!("failed");
        assert_eq!(
            delivery_with_cancel(&failed_doc, "failed", true),
            AgentDelivery::NotDelivered
        );

        let interrupted_expected = json!({
            "thread": {
                "archived": false,
                "state": {
                    "status": "interrupted",
                    "delivery_settled": false,
                    "subagent_report": {"state": "expected"}
                }
            },
            "native_session": {"background_tasks": 0}
        });
        assert_eq!(
            delivery_with_cancel(&interrupted_expected, "interrupted", false),
            AgentDelivery::NotDelivered
        );
    }
}
