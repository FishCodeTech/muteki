use agent::{AgentEvent, ItemContent, ThreadItem};
use base64::Engine;
use serde_json::{Value, json};
use tcode_core::session::{EntryContent, Timeline};

/// Build provider content from local uploads. Stored messages retain opaque
/// references; file bytes and image data URLs are resolved for each request.
pub fn attachment_content(text: &str, files: Vec<(Value, Vec<u8>)>) -> Result<Value, String> {
    attachment_content_and_delivery(text, files).map(|(content, _)| content)
}

/// Delivery entries are produced beside the exact provider blocks. Image bytes
/// remain in the request only; their ledger entry contains identifying metadata.
pub fn attachment_content_and_delivery(
    text: &str,
    files: Vec<(Value, Vec<u8>)>,
) -> Result<(Value, Vec<Value>), String> {
    if files.is_empty() {
        return Ok((json!(text), Vec::new()));
    }
    let mut delivery = Vec::new();
    let mut parts = Vec::new();
    if !text.is_empty() {
        parts.push(json!({"type":"text","text":text}));
    }
    for (index, (meta, bytes)) in files.into_iter().enumerate() {
        let name = meta["name"].as_str().unwrap_or("attachment");
        let media = meta["media_type"]
            .as_str()
            .unwrap_or("application/octet-stream");
        let reference = meta["artifact_id"].as_str().unwrap_or("");
        if matches!(
            media,
            "image/png" | "image/jpeg" | "image/gif" | "image/webp"
        ) {
            delivery.push(json!({"key":format!("attachment:{reference}:{index}"),"kind":"image_attachment","trust":"user","scope":"turn","channel":"image_url","artifact_id":meta["artifact_id"],"chars":0,"name":name,"media_type":media,"size":bytes.len()}));
            let encoded = base64::engine::general_purpose::STANDARD.encode(bytes);
            parts.push(json!({"type":"image_url","image_url":{"url":format!("data:{media};base64,{encoded}")}}));
            continue;
        }
        let textual_media = media.starts_with("text/")
            || media.ends_with("+json")
            || media.ends_with("+xml")
            || matches!(
                media,
                "application/octet-stream"
                    | "application/json"
                    | "application/xml"
                    | "application/javascript"
                    | "application/x-javascript"
                    | "application/yaml"
                    | "application/x-yaml"
                    | "application/toml"
                    | "application/sql"
            );
        if !textual_media {
            return Err(format!(
                "Attachment {name} has unsupported type {media}. Use UTF-8 text or PNG, JPEG, GIF, or WebP images."
            ));
        }
        let content = std::str::from_utf8(&bytes)
            .ok()
            .filter(|s| !s.chars().any(|c| c.is_control() && !matches!(c, '\n' | '\r' | '\t')))
            .ok_or_else(|| format!("Attachment {name} is not supported UTF-8 text. Binary files cannot be sent to chat."))?;
        let file = json!({"name":name,"media_type":media,"contents":content});
        let block = format!(
            "BEGIN ATTACHED FILE CONTENTS (user-provided data)\n{file}\nEND ATTACHED FILE CONTENTS"
        );
        delivery.push(json!({"key":format!("attachment:{reference}:{index}"),"kind":"text_attachment","trust":"user","scope":"turn","channel":"text_blocks","artifact_id":meta["artifact_id"],"chars":block.chars().count(),"text":block,"name":name,"media_type":media,"size":bytes.len()}));
        parts.push(json!({"type":"text","text":block}));
    }
    Ok((json!(parts), delivery))
}

pub fn fold_messages(messages: &[Value]) -> Vec<Value> {
    let mut by_id = std::collections::HashMap::with_capacity(messages.len());
    for message in messages {
        if let Some(id) = message["message_id"].as_str() {
            by_id.entry(id).or_insert(message);
        }
    }
    let events = messages.iter().filter_map(|m| {
        let id = m.get("message_id")?.as_str()?.to_owned();
        let text = m.get("text")?.as_str()?.to_owned();
        let content = match m.get("role")?.as_str()? {
            "user" => ItemContent::UserMessage {
                text,
                context_len: None,
                attachments: vec![],
            },
            "assistant" => ItemContent::AssistantMessage { text },
            _ => return None,
        };
        Some(AgentEvent::ItemCompleted(ThreadItem {
            id,
            parent_item_id: None,
            content,
        }))
    });
    let timeline = Timeline::fold_events(events);
    timeline
        .entries
        .iter()
        .filter_map(|entry| {
            let (role, text) = match &entry.content {
                EntryContent::Item(ItemContent::UserMessage { text, .. }) => ("user", text),
                EntryContent::Steer { text, .. } => ("user", text),
                EntryContent::Item(ItemContent::AssistantMessage { text }) => ("assistant", text),
                _ => return None,
            };
            let mut row = by_id
                .get(entry.id.as_str())
                .map(|message| (*message).clone())
                .unwrap_or_else(|| json!({}));
            row["message_id"] = json!(entry.id);
            row["role"] = json!(role);
            row["text"] = json!(text);
            Some(row)
        })
        .collect()
}

fn str_field<'a>(v: &'a Value, keys: &[&str]) -> &'a str {
    for k in keys {
        if let Some(s) = v.get(*k).and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty()) {
            return s;
        }
    }
    ""
}

fn norm_task_status(v: Option<&Value>) -> &'static str {
    let raw = v.and_then(Value::as_str).unwrap_or("pending").trim().to_ascii_lowercase().replace('-', "_");
    match raw.as_str() {
        "pending" | "todo" | "open" => "pending",
        "in_progress" | "running" | "inprogress" | "active" => "in_progress",
        "completed" | "done" | "complete" | "finished" => "completed",
        "blocked" | "failed" | "error" => "blocked",
        "cancelled" | "canceled" | "interrupted" => "cancelled",
        "unknown" => "unknown",
        "waiting" => "waiting",
        "declined" => "declined",
        _ => "pending",
    }
}

fn norm_plan_phase(v: Option<&Value>, default: &str) -> String {
    let raw = v.and_then(Value::as_str).unwrap_or(default).trim().to_ascii_lowercase().replace('-', "_");
    match raw.as_str() {
        "proposed" | "proposal" | "suggest" => "proposed".into(),
        "executing" | "running" | "in_progress" => "executing".into(),
        "awaiting_decision" | "waiting" | "awaiting" | "decision" => "awaiting_decision".into(),
        "completed" | "done" | "complete" => "completed".into(),
        "cleared" | "clear" => "cleared".into(),
        "unsupported" | "none" | "unknown" => "unsupported".into(),
        _ => default.to_owned(),
    }
}

fn normalize_plan_task(raw: &Value, index: usize, now: &str) -> Option<Value> {
    if let Some(text) = raw.as_str().map(str::trim).filter(|s| !s.is_empty()) {
        return Some(json!({
            "task_id": format!("step-{}", index + 1),
            "title": text,
            "status": "pending",
            "blocked_reason": Value::Null,
            "evidence": [],
            "updated_at": now,
        }));
    }
    if !raw.is_object() {
        return None;
    }
    let mut title = str_field(raw, &["title", "content", "description", "text", "step"]).to_owned();
    let mut task_id = str_field(raw, &["task_id", "id", "step_id", "item_id"]).to_owned();
    if task_id.is_empty() {
        if title.is_empty() {
            return None;
        }
        task_id = format!("step-{}", index + 1);
    }
    if title.is_empty() {
        title = task_id.clone();
    }
    let evidence_raw = raw.get("evidence").or_else(|| raw.get("evidence_refs"));
    let evidence: Vec<Value> = evidence_raw
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|item| {
            let id = str_field(item, &["id", "call_id"]);
            if id.is_empty() {
                return None;
            }
            let kind = str_field(item, &["kind"]);
            let kind = if kind.is_empty() { "tool" } else { kind };
            let turn_id = str_field(item, &["turn_id"]);
            Some(json!({
                "kind": kind,
                "id": id,
                "turn_id": if turn_id.is_empty() { Value::Null } else { json!(turn_id) },
            }))
        })
        .collect();
    let blocked = str_field(raw, &["blocked_reason"]);
    Some(json!({
        "task_id": task_id,
        "title": title,
        "status": norm_task_status(raw.get("status")),
        "blocked_reason": if blocked.is_empty() { Value::Null } else { json!(blocked) },
        "evidence": evidence,
        "updated_at": now,
    }))
}

fn extract_plan_tasks(payload: &Value) -> Vec<Value> {
    for key in ["tasks", "entries", "steps", "items"] {
        if let Some(arr) = payload.get(key).and_then(Value::as_array) {
            return arr.clone();
        }
    }
    if let Some(plan) = payload.get("plan") {
        if let Some(arr) = plan.as_array() {
            return arr.clone();
        }
        for key in ["tasks", "entries", "steps", "items"] {
            if let Some(arr) = plan.get(key).and_then(Value::as_array) {
                return arr.clone();
            }
        }
    }
    Vec::new()
}

pub fn upsert_plan_snapshot(prev: &Value, payload: &Value, patch: bool, now: &str) -> Value {
    let prev_rev = prev["revision"].as_u64().unwrap_or(0);
    if let Some(inc) = payload.get("revision").and_then(Value::as_i64)
        && prev.is_object()
        && (inc.max(0) as u64) < prev_rev
    {
        return prev.clone();
    }
    let revision = payload
        .get("revision")
        .and_then(Value::as_i64)
        .map(|v| v.max(0) as u64)
        .unwrap_or_else(|| (prev_rev + 1).max(1));
    let normalized: Vec<Value> = extract_plan_tasks(payload)
        .iter()
        .enumerate()
        .filter_map(|(i, raw)| normalize_plan_task(raw, i, now))
        .collect();
    let prev_tasks = prev["tasks"].as_array().cloned().unwrap_or_default();
    let tasks = if patch && !prev_tasks.is_empty() {
        let mut merged = prev_tasks;
        for task in &normalized {
            if let Some(pos) = merged.iter().position(|t| t["task_id"] == task["task_id"]) {
                merged[pos] = task.clone();
            } else {
                merged.push(task.clone());
            }
        }
        merged
    } else if !normalized.is_empty() {
        normalized.clone()
    } else {
        prev_tasks
    };
    let default_phase = prev["phase"]
        .as_str()
        .filter(|s| !s.is_empty())
        .unwrap_or(if tasks.is_empty() { "cleared" } else { "proposed" });
    let mut phase = norm_plan_phase(payload.get("phase"), default_phase);
    if phase == "proposed" && tasks.iter().any(|t| t["status"] == "in_progress") {
        phase = "executing".into();
    }
    if matches!(phase.as_str(), "proposed" | "executing")
        && !tasks.is_empty()
        && tasks.iter().all(|t| matches!(t["status"].as_str(), Some("completed" | "cancelled")))
    {
        phase = "completed".into();
    }
    let awaiting = match payload.get("awaiting") {
        None => prev.get("awaiting").cloned().unwrap_or(Value::Null),
        Some(Value::Null) => Value::Null,
        Some(v) if v.is_object() => v.clone(),
        Some(v) => json!({"kind": "user_input", "summary": v.as_str().unwrap_or("")}),
    };
    let pending_amendment = match payload.get("pending_amendment") {
        None => prev.get("pending_amendment").cloned().unwrap_or(Value::Null),
        Some(Value::Null) => Value::Null,
        Some(v) if v.is_object() => v.clone(),
        Some(v) => json!({"text": v.as_str().unwrap_or(""), "status": "unconfirmed"}),
    };
    let title = str_field(payload, &["title"]);
    let title = if title.is_empty() { str_field(prev, &["title"]) } else { title };
    let source = str_field(payload, &["source"]);
    let source = if source.is_empty() {
        let ps = str_field(prev, &["source"]);
        if ps.is_empty() { "adapter" } else { ps }
    } else {
        source
    };
    let unsupported_reason = str_field(payload, &["unsupported_reason"]);
    let unsupported_reason = if unsupported_reason.is_empty() {
        str_field(prev, &["unsupported_reason"])
    } else {
        unsupported_reason
    };
    let mut last_change = str_field(payload, &["last_change_summary", "summary"]).to_owned();
    if last_change.is_empty() && !normalized.is_empty() {
        last_change = format!("更新 {} 个计划步骤", normalized.len());
    }
    let markdown = payload["native"]["markdown"]
        .as_str()
        .or_else(|| payload["explanation"].as_str())
        .or_else(|| prev["markdown"].as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let adapter_id = str_field(payload, &["adapter_id"]);
    let adapter_id = if adapter_id.is_empty() { str_field(prev, &["adapter_id"]) } else { adapter_id };
    let session_id = str_field(payload, &["agent_session_id"]);
    let session_id = if session_id.is_empty() { str_field(prev, &["agent_session_id"]) } else { session_id };
    let turn_id = str_field(payload, &["turn_id"]);
    let turn_id = if turn_id.is_empty() { str_field(prev, &["turn_id"]) } else { turn_id };
    json!({
        "revision": revision,
        "phase": phase,
        "source": source,
        "adapter_id": if adapter_id.is_empty() { Value::Null } else { json!(adapter_id) },
        "agent_session_id": if session_id.is_empty() { Value::Null } else { json!(session_id) },
        "turn_id": if turn_id.is_empty() { Value::Null } else { json!(turn_id) },
        "title": if title.is_empty() { Value::Null } else { json!(title) },
        "tasks": tasks,
        "awaiting": awaiting,
        "pending_amendment": pending_amendment,
        "last_change_summary": if last_change.is_empty() { Value::Null } else { json!(last_change) },
        "unsupported_reason": if unsupported_reason.is_empty() { Value::Null } else { json!(unsupported_reason) },
        "markdown": markdown,
        "updated_at": now,
    })
}

pub fn inject_plan(state: &mut Value, payload: &Value, now: &str) -> Result<(&'static str, Value), (&'static str, &'static str)> {
    if payload["unsupported"] == true || payload["phase"].as_str() == Some("unsupported") {
        let reason = str_field(payload, &["unsupported_reason", "reason"]);
        let reason = if reason.is_empty() { "当前 Runtime 不支持结构化计划事件" } else { reason };
        let prev_rev = state["plan"]["revision"].as_u64().unwrap_or(0);
        state["plan"] = json!({
            "revision": (prev_rev + 1).max(1),
            "phase": "unsupported",
            "source": "none",
            "adapter_id": Value::Null,
            "agent_session_id": Value::Null,
            "turn_id": Value::Null,
            "title": Value::Null,
            "tasks": [],
            "awaiting": Value::Null,
            "pending_amendment": Value::Null,
            "last_change_summary": "Runtime 不支持结构化计划事件",
            "unsupported_reason": reason,
            "markdown": Value::Null,
            "updated_at": now,
        });
        return Ok(("core.plan.cleared", json!({
            "unsupported": true,
            "unsupported_reason": reason,
            "phase": "unsupported",
        })));
    }
    let has_tasks = ["tasks", "entries", "steps"]
        .iter()
        .any(|k| payload.get(*k).and_then(Value::as_array).is_some_and(|a| !a.is_empty()));
    if !has_tasks {
        return Err(("conversation.plan.tasks_required", "plan.inject requires payload.tasks (or unsupported=true)"));
    }
    let patch = payload["patch"] == true || payload["delta"] == true;
    let source = str_field(payload, &["source"]);
    let source = if source.is_empty() { "fixture" } else { source };
    let mut body = payload.clone();
    body["source"] = json!(source);
    body["patch"] = json!(patch);
    state["plan"] = upsert_plan_snapshot(&state["plan"], &body, patch, now);
    Ok(("core.plan.updated", body))
}

pub fn amend_plan(
    state: &mut Value,
    payload: &Value,
    command_id: &str,
    now: &str,
) -> Result<(String, Value, Value), (&'static str, &'static str)> {
    let mut text = str_field(payload, &["text", "amendment"]).to_owned();
    if text.is_empty()
        && let Some(items) = payload["amendments"].as_array()
    {
        let parts: Vec<String> = items
            .iter()
            .filter_map(|item| {
                if let Some(s) = item.as_str().map(str::trim).filter(|s| !s.is_empty()) {
                    Some(s.to_owned())
                } else if item.is_object() {
                    let p = str_field(item, &["text", "title", "change"]);
                    (!p.is_empty()).then(|| p.to_owned())
                } else {
                    None
                }
            })
            .collect();
        text = parts.join("；");
    }
    if text.is_empty() {
        return Err(("conversation.plan.amend_required", "plan.amend requires payload.text or amendments[]"));
    }
    let plan = &state["plan"];
    let prev_rev = plan["revision"].as_u64().unwrap_or(0);
    let plan_revision = payload["plan_revision"].as_u64().unwrap_or(prev_rev);
    let steer_text = format!("请根据我对当前执行计划（revision={plan_revision}）的修改建议调整：{text}");
    let overlay = json!({
        "text": text,
        "status": "unconfirmed",
        "plan_revision": plan_revision,
        "requested_at": command_id,
    });
    let phase = if plan.is_object() {
        let p = plan["phase"].as_str().unwrap_or("proposed");
        if p == "proposed" { "awaiting_decision" } else { p }
    } else {
        "proposed"
    };
    let tasks = plan["tasks"].as_array().cloned().unwrap_or_default();
    let source = str_field(plan, &["source"]);
    let source = if source.is_empty() { "fixture" } else { source };
    let title = plan.get("title").cloned().unwrap_or(Value::Null);
    let event_body = json!({
        "revision": plan_revision.max(prev_rev) + 1,
        "phase": phase,
        "patch": true,
        "tasks": tasks,
        "pending_amendment": overlay,
        "awaiting": {
            "kind": "plan_accept",
            "summary": "用户已提出计划修改，等待 Runtime 确认",
        },
        "source": source,
        "last_change_summary": "用户提出计划修改（未获 Runtime 确认）",
        "title": title,
    });
    state["plan"] = upsert_plan_snapshot(plan, &event_body, true, now);
    Ok((steer_text, overlay, event_body))
}

fn looks_like_shell_tool(raw: &Value) -> bool {
    if raw["is_agent"] == true || raw["isAgent"] == true {
        return false;
    }
    if !str_field(raw, &["agent_id", "parent_id", "parent_agent_id"]).is_empty() {
        return false;
    }
    let name = str_field(raw, &["title", "name", "tool", "label"])
        .to_ascii_lowercase()
        .replace(['-', ' '], "_");
    if name.is_empty() {
        return false;
    }
    ["shell", "bash", "zsh", "terminal", "exec", "command", "run_terminal", "powershell"]
        .iter()
        .any(|m| name.contains(m))
}

fn norm_agent_status(v: Option<&Value>) -> &'static str {
    let raw = v.and_then(Value::as_str).unwrap_or("pending").trim().to_ascii_lowercase().replace('-', "_");
    match raw.as_str() {
        "pending" | "todo" | "open" => "pending",
        "waiting" => "waiting",
        "unknown" => "unknown",
        "declined" => "declined",
        "running" | "in_progress" | "inprogress" | "active" => "running",
        "completed" | "done" | "complete" | "finished" => "completed",
        "failed" | "error" | "blocked" => "failed",
        "cancelled" | "canceled" | "interrupted" => "cancelled",
        _ => "pending",
    }
}

fn normalize_agent_node(raw: &Value, index: usize, now: &str) -> Option<Value> {
    if !raw.is_object() || looks_like_shell_tool(raw) {
        return None;
    }
    let mut agent_id = str_field(raw, &["agent_id", "id", "call_id", "tool_call_id"]).to_owned();
    let mut title = str_field(raw, &["title", "name", "label", "description"]).to_owned();
    if agent_id.is_empty() {
        if title.is_empty() {
            return None;
        }
        agent_id = format!("agent-{}", index + 1);
    }
    if title.is_empty() {
        title = agent_id.clone();
    }
    let parent_id = str_field(raw, &["parent_id", "parent_agent_id", "parent"]);
    let status = norm_agent_status(raw.get("status"));
    let terminal = matches!(status, "completed" | "failed" | "cancelled" | "declined");
    let opt_str = |keys: &[&str]| {
        let s = str_field(raw, keys);
        if s.is_empty() { Value::Null } else { json!(s) }
    };
    let opt_u64 = |key: &str| raw.get(key).and_then(Value::as_u64).map_or(Value::Null, |n| json!(n));
    Some(json!({
        "agent_id": agent_id,
        "parent_id": if parent_id.is_empty() { Value::Null } else { json!(parent_id) },
        "origin": raw["origin"].as_str().unwrap_or("adapter"),
        "adapter_id": opt_str(&["adapter_id"]),
        "title": title,
        "nickname": opt_str(&["nickname"]),
        "role": opt_str(&["role"]),
        "model": opt_str(&["model"]),
        "effort": opt_str(&["effort"]),
        "cancel_supported": raw["cancel_supported"].as_bool().unwrap_or(false),
        "cancel_requested": raw["cancel_requested"].as_bool().unwrap_or(false),
        "cancel_reason": opt_str(&["cancel_reason"]),
        "turn_id": opt_str(&["turn_id"]),
        "message_id": opt_str(&["message_id"]),
        "call_id": opt_str(&["call_id", "tool_call_id"]),
        "session_ref": opt_str(&["session_ref"]),
        "thread_id": opt_str(&["thread_id"]),
        "status": status,
        "request": opt_str(&["request"]),
        "result": opt_str(&["result", "output", "summary"]),
        "error": opt_str(&["error"]),
        "activity": opt_str(&["activity"]),
        "tool_uses": opt_u64("tool_uses"),
        "total_tokens": opt_u64("total_tokens"),
        "duration_ms": opt_u64("duration_ms"),
        "started_at": raw.get("started_at").filter(|v| !v.is_null()).cloned().unwrap_or_else(|| json!(now)),
        "completed_at": if terminal {
            raw.get("completed_at").filter(|v| !v.is_null()).cloned().unwrap_or_else(|| json!(now))
        } else {
            Value::Null
        },
        "updated_at": now,
    }))
}

pub fn inject_agents(
    doc: &mut Value,
    payload: &Value,
    now: &str,
) -> Result<Vec<(&'static str, Value)>, (&'static str, &'static str)> {
    let mut events = Vec::new();
    let prev = doc["thread"]["state"]["agents"].clone();
    let prev_rev = prev["revision"].as_u64().unwrap_or(0);
    if payload["unsupported"] == true || payload["phase"].as_str() == Some("unsupported") {
        let reason = str_field(payload, &["unsupported_reason", "reason"]);
        let reason = if reason.is_empty() { "当前 Runtime 未上报委派 Agent 事件" } else { reason };
        let summary = str_field(payload, &["tool_activity_summary", "activity_summary"]);
        let summary_val = if summary.is_empty() {
            prev.get("tool_activity_summary").cloned().unwrap_or(Value::Null)
        } else {
            json!(summary)
        };
        let body = json!({
            "unsupported": true,
            "unsupported_reason": reason,
            "tool_activity_summary": if summary.is_empty() { Value::Null } else { json!(summary) },
            "phase": "unsupported",
        });
        doc["thread"]["state"]["agents"] = json!({
            "revision": (prev_rev + 1).max(1),
            "source": "none",
            "adapter_id": Value::Null,
            "turn_id": Value::Null,
            "agents": [],
            "unsupported": true,
            "unsupported_reason": reason,
            "tool_activity_summary": summary_val,
            "updated_at": now,
        });
        events.push(("core.agent.cleared", body));
    } else {
        let raw_agents = payload
            .get("agents")
            .and_then(Value::as_array)
            .or_else(|| payload.get("nodes").and_then(Value::as_array))
            .filter(|a| !a.is_empty())
            .ok_or(("conversation.agents.agents_required", "agents.inject requires payload.agents (or unsupported=true)"))?;
        let patch = payload["patch"] == true || payload["delta"] == true;
        let source = str_field(payload, &["source"]);
        let source = if source.is_empty() { "fixture" } else { source };
        let mut body = payload.clone();
        if let Some(obj) = body.as_object_mut() {
            obj.remove("tools");
        }
        body["source"] = json!(source);
        body["patch"] = json!(patch);
        let prev_agents = prev["agents"].as_array().cloned().unwrap_or_default();
        let mut normalized = Vec::new();
        for (i, item) in raw_agents.iter().enumerate() {
            let mut merged_item = item.clone();
            if patch
                && let Some(obj) = item.as_object()
            {
                let id = str_field(item, &["agent_id", "id"]);
                if let Some(base) = prev_agents.iter().find(|a| a["agent_id"] == id)
                    && let Some(base_obj) = base.as_object()
                {
                    let mut m = base_obj.clone();
                    m.remove("updated_at");
                    for (k, v) in obj {
                        m.insert(k.clone(), v.clone());
                    }
                    if base["turn_id"].is_string() {
                        m.insert("turn_id".into(), base["turn_id"].clone());
                    }
                    merged_item = Value::Object(m);
                }
            }
            if payload["adapter_id"].is_string() {
                merged_item["adapter_id"] = payload["adapter_id"].clone();
            }
            if let Some(node) = normalize_agent_node(&merged_item, i, now) {
                normalized.push(node);
            }
        }
        let agents = if patch && !prev_agents.is_empty() {
            let mut merged = prev_agents;
            for node in normalized {
                if let Some(pos) = merged.iter().position(|a| a["agent_id"] == node["agent_id"]) {
                    merged[pos] = node;
                } else {
                    merged.push(node);
                }
            }
            merged
        } else {
            normalized
        };
        let revision = payload
            .get("revision")
            .and_then(Value::as_u64)
            .unwrap_or_else(|| (prev_rev + 1).max(1));
        let summary = str_field(payload, &["tool_activity_summary", "activity_summary"]);
        let summary_val = if summary.is_empty() {
            prev.get("tool_activity_summary").cloned().unwrap_or(Value::Null)
        } else {
            json!(summary)
        };
        let adapter_id = str_field(payload, &["adapter_id"]);
        let adapter_id = if adapter_id.is_empty() { str_field(&prev, &["adapter_id"]) } else { adapter_id };
        let turn_id = str_field(payload, &["turn_id"]);
        let turn_id = if turn_id.is_empty() { str_field(&prev, &["turn_id"]) } else { turn_id };
        doc["thread"]["state"]["agents"] = json!({
            "revision": revision,
            "source": source,
            "adapter_id": if adapter_id.is_empty() { Value::Null } else { json!(adapter_id) },
            "turn_id": if turn_id.is_empty() { Value::Null } else { json!(turn_id) },
            "agents": agents,
            "unsupported": false,
            "unsupported_reason": Value::Null,
            "tool_activity_summary": summary_val,
            "updated_at": now,
        });
        events.push(("core.agent.updated", body));
    }
    if let Some(tools) = payload.get("tools").and_then(Value::as_array) {
        let default_turn = str_field(payload, &["turn_id"]);
        let default_turn = if default_turn.is_empty() { "turn-c21-fixture" } else { default_turn };
        if !doc["tools"].is_array() {
            doc["tools"] = json!([]);
        }
        for (index, tool) in tools.iter().enumerate() {
            if !tool.is_object() {
                continue;
            }
            let call_id = str_field(tool, &["call_id", "tool_call_id", "id"]);
            let call_id = if call_id.is_empty() {
                format!("tool-c21-{}", index + 1)
            } else {
                call_id.to_owned()
            };
            let turn_id = str_field(tool, &["turn_id"]);
            let turn_id = if turn_id.is_empty() { default_turn } else { turn_id };
            let name = str_field(tool, &["tool", "name"]);
            let name = if name.is_empty() { "tool" } else { name };
            let input = tool.get("input").or_else(|| tool.get("arguments")).cloned().unwrap_or_else(|| json!({}));
            let mut started = json!({
                "turn_id": turn_id,
                "call_id": call_id,
                "tool": name,
                "input": input,
            });
            let parent = str_field(tool, &["parent_tool_use_id", "parent_id"]);
            if !parent.is_empty() {
                started["parent_tool_use_id"] = json!(parent);
            }
            if tool["is_agent"] == true {
                started["is_agent"] = json!(true);
            }
            events.push(("core.tool.started", started));
            let is_done = matches!(tool["status"].as_str(), Some("completed" | "failed"))
                || !str_field(tool, &["output", "result", "error"]).is_empty();
            let output_text = str_field(tool, &["output", "result"]).to_owned();
            if is_done {
                let is_error = tool["is_error"] == true
                    || tool["status"] == "failed"
                    || !tool["error"].is_null() && tool["error"] != "";
                let mut completed = json!({
                    "turn_id": turn_id,
                    "call_id": call_id,
                    "output": output_text,
                    "is_error": is_error,
                });
                if !tool["error"].is_null() && tool["error"] != "" {
                    completed["error"] = tool["error"].clone();
                }
                events.push(("core.tool.completed", completed));
            }
            let status = if is_done {
                if tool["status"] == "failed" || tool["is_error"] == true { "failed" } else { "completed" }
            } else {
                "running"
            };
            doc["tools"].as_array_mut().unwrap().push(json!({
                "tool_id": call_id,
                "turn_id": turn_id,
                "name": name,
                "status": status,
                "output_summary": output_text,
                "output_complete": is_done,
                "stream_seq": index + 1,
            }));
        }
    }
    Ok(events)
}

fn infer_approval_kind(item: &Value) -> String {
    let raw = str_field(item, &["approval_kind", "kind", "type", "permission_kind"])
        .to_ascii_lowercase()
        .replace('-', "_");
    match raw.as_str() {
        "command" | "commandexecution" | "command_execution" | "shell" | "exec" => "command_execution".into(),
        "file_change" | "filechange" | "apply_patch" | "applypatch" | "patch" => "file_change".into(),
        "permissions" | "permission" => "permissions".into(),
        "mcp_tool_call" | "mcp" => "mcp_tool_call".into(),
        "tool" => "tool".into(),
        "plan_exit" | "exit_plan_mode" | "exitplanmode" => "plan_exit".into(),
        "" => {
            if !str_field(item, &["diff", "patch", "unified_diff"]).is_empty() {
                "file_change".into()
            } else if !str_field(item, &["command", "cmd"]).is_empty() {
                "command_execution".into()
            } else {
                "unknown".into()
            }
        }
        other => other.into(),
    }
}

fn normalize_approval_item(item: &Value, now: &str) -> Result<Value, String> {
    if !item.is_object() {
        return Err("approval item must be an object".into());
    }
    let approval_id = str_field(item, &["approval_id", "request_id", "id"]);
    if approval_id.is_empty() {
        return Err("approval request requires approval_id".into());
    }
    let kind = infer_approval_kind(item);
    let command = str_field(item, &["command", "cmd"]);
    let cwd = str_field(item, &["cwd", "working_directory", "workspace", "workdir", "grantRoot", "grant_root"]);
    let path = str_field(item, &["path", "file", "filename", "file_path"]);
    let title = str_field(item, &["title", "action", "tool_name", "tool", "name", "requester"]);
    let title = if !title.is_empty() {
        title
    } else if kind == "command_execution" && !command.is_empty() {
        "命令执行"
    } else if kind == "file_change" {
        "文件变更"
    } else if kind == "mcp_tool_call" {
        "MCP 工具调用"
    } else if kind == "plan_exit" {
        "计划已就绪"
    } else {
        "Runtime 操作"
    };
    let status = match str_field(item, &["status"]) {
        "pending" | "expired" | "resolving" => str_field(item, &["status"]),
        _ => "pending",
    };
    let requested_at = str_field(item, &["requested_at"]);
    let requested_at = if requested_at.is_empty() { now } else { requested_at };
    let mut normalized = item.clone();
    normalized["approval_id"] = json!(approval_id);
    normalized["approval_kind"] = json!(kind);
    normalized["status"] = json!(status);
    normalized["title"] = json!(title);
    normalized["requested_at"] = json!(requested_at);
    normalized["fixture"] = json!(true);
    if !command.is_empty() {
        normalized["command"] = json!(command);
    }
    if !cwd.is_empty() {
        normalized["cwd"] = json!(cwd);
    }
    if !path.is_empty() {
        normalized["path"] = json!(path);
    }
    Ok(normalized)
}

pub fn inject_approvals(
    doc: &mut Value,
    payload: &Value,
    now: &str,
) -> Result<Vec<(&'static str, Value)>, (axum::http::StatusCode, &'static str, String)> {
    use axum::http::StatusCode;
    if std::env::var("MUTEKI_ALLOW_APPROVAL_FIXTURES").unwrap_or_default().trim() != "1" {
        return Err((
            StatusCode::FORBIDDEN,
            "conversation.approval.fixture_disabled",
            "Set MUTEKI_ALLOW_APPROVAL_FIXTURES=1 to inject fixtures".into(),
        ));
    }
    let raw_items: Vec<Value> = match payload.get("approvals") {
        None => vec![payload.clone()],
        Some(Value::Array(items)) if !items.is_empty() => items.clone(),
        _ => {
            return Err((
                StatusCode::BAD_REQUEST,
                "conversation.approval.invalid",
                "inject 需要 approvals[] 或单个 approval 载荷".into(),
            ));
        }
    };
    if !doc["thread"]["state"]["pending_approvals"].is_object() {
        doc["thread"]["state"]["pending_approvals"] = json!({});
    }
    let mut events = Vec::new();
    for item in &raw_items {
        let row = normalize_approval_item(item, now)
            .map_err(|msg| (StatusCode::BAD_REQUEST, "conversation.approval.invalid", msg))?;
        let id = row["approval_id"].as_str().unwrap().to_owned();
        if row["status"] == "expired" {
            let mut exp = doc["thread"]["state"]["pending_approvals"]
                .get(&id)
                .filter(|v| v.is_object())
                .cloned()
                .unwrap_or_else(|| row.clone());
            exp["status"] = json!("expired");
            exp["expired_reason"] = row.get("reason").cloned().unwrap_or_else(|| json!("请求已过期"));
            doc["thread"]["state"]["pending_approvals"][&id] = exp;
        } else {
            doc["thread"]["state"]["pending_approvals"][&id] = row.clone();
        }
        events.push(("core.approval.requested", row));
    }
    let expire_id = str_field(payload, &["expire_approval_id"]).to_owned();
    if !expire_id.is_empty() {
        let reason = str_field(payload, &["expire_reason"]);
        let reason = if reason.is_empty() { "请求已过期" } else { reason };
        let title = str_field(payload, &["expire_title"]);
        let title = if title.is_empty() { "已过期的审批" } else { title };
        let exp_event = json!({
            "approval_id": expire_id,
            "status": "expired",
            "reason": reason,
            "title": title,
            "approval_kind": "unknown",
        });
        let mut exp_row = doc["thread"]["state"]["pending_approvals"]
            .get(&expire_id)
            .filter(|v| v.is_object())
            .cloned()
            .unwrap_or_else(|| exp_event.clone());
        exp_row["status"] = json!("expired");
        exp_row["expired_reason"] = json!(reason);
        doc["thread"]["state"]["pending_approvals"][&expire_id] = exp_row;
        events.push(("core.approval.requested", exp_event));
    }
    let oldest = doc["thread"]["state"]["pending_approvals"]
        .as_object()
        .into_iter()
        .flat_map(|rows| rows.values())
        .filter(|row| matches!(row["status"].as_str(), Some("pending" | "resolving")))
        .min_by(|a, b| a["requested_at"].as_str().unwrap_or("").cmp(b["requested_at"].as_str().unwrap_or("")))
        .cloned();
    doc["thread"]["state"]["pending_approval"] = oldest.clone().unwrap_or(Value::Null);
    if let Some(primary) = oldest
        && doc["thread"]["state"]["plan"].is_object()
        && !matches!(doc["thread"]["state"]["plan"]["phase"].as_str(), Some("cleared" | "unsupported"))
    {
        let aid = primary["approval_id"].as_str().unwrap_or("").to_owned();
        let summary = str_field(&primary, &["summary", "title", "tool", "reason"]);
        let summary = if summary.is_empty() { "等待审批".to_owned() } else { summary.to_owned() };
        doc["thread"]["state"]["plan"]["phase"] = json!("awaiting_decision");
        doc["thread"]["state"]["plan"]["awaiting"] = json!({"kind": "approval", "request_id": aid, "summary": summary});
        doc["thread"]["state"]["plan"]["last_change_summary"] = json!("进入等待决策（审批）");
    }
    Ok(events)
}

fn c22_fixture_questions() -> Vec<Value> {
    vec![
        json!({
            "question_id": "q_header",
            "kind": "header",
            "prompt": "部署确认",
            "required": false,
            "options": [],
            "allow_free_text": false,
            "placeholder": "",
        }),
        json!({
            "question_id": "q_env",
            "kind": "single_select",
            "prompt": "选择部署环境",
            "required": true,
            "options": [
                {"value": "staging", "label": "staging", "recommended": false},
                {"value": "prod", "label": "prod", "recommended": true},
            ],
            "allow_free_text": false,
            "placeholder": "",
        }),
        json!({
            "question_id": "q_flags",
            "kind": "multi_select",
            "prompt": "可选特性开关",
            "required": false,
            "options": [
                {"value": "a", "label": "a", "recommended": false},
                {"value": "b", "label": "b", "recommended": false},
                {"value": "c", "label": "c", "recommended": false},
            ],
            "allow_free_text": false,
            "placeholder": "",
        }),
        json!({
            "question_id": "q_note",
            "kind": "free_text",
            "prompt": "备注",
            "required": true,
            "options": [],
            "allow_free_text": true,
            "placeholder": "填写备注…",
        }),
    ]
}

fn normalize_question_item(raw: &Value, index: usize) -> Option<Value> {
    if let Some(text) = raw.as_str().map(str::trim).filter(|s| !s.is_empty()) {
        return Some(json!({
            "question_id": format!("q{index}"),
            "kind": "free_text",
            "prompt": text,
            "required": true,
            "options": [],
            "allow_free_text": true,
            "placeholder": "",
        }));
    }
    if !raw.is_object() {
        return None;
    }
    let header_text = str_field(raw, &["header"]);
    let mut prompt = str_field(raw, &["prompt", "question", "message", "label", "title", "header"]).to_owned();
    let qid = str_field(raw, &["question_id", "id", "name", "key"]);
    let question_id = if qid.is_empty() { format!("q{index}") } else { qid.to_owned() };
    let options: Vec<Value> = raw
        .get("options")
        .or_else(|| raw.get("choices"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|opt| {
            if let Some(s) = opt.as_str().map(str::trim).filter(|s| !s.is_empty()) {
                Some(json!({"value": s, "label": s, "recommended": false}))
            } else if opt.is_object() {
                let val = str_field(opt, &["value", "id", "label", "title", "name"]);
                if val.is_empty() {
                    return None;
                }
                let label = str_field(opt, &["label", "title", "name", "value"]);
                let rec = opt["recommended"] == true || opt["isRecommended"] == true || opt["is_recommended"] == true;
                Some(json!({"value": val, "label": if label.is_empty() { val } else { label }, "recommended": rec}))
            } else {
                None
            }
        })
        .collect();
    let explicit_kind = str_field(raw, &["kind", "type"]).to_ascii_lowercase();
    let is_header = explicit_kind == "header"
        || (!header_text.is_empty() && str_field(raw, &["question", "prompt"]).is_empty())
        || raw["isHeader"] == true
        || raw["is_header"] == true;
    let multi = raw["multi"] == true
        || raw["multiple"] == true
        || raw["multiSelect"] == true
        || raw["multi_select"] == true
        || raw["allow_multiple"] == true;
    let allow_free_text = raw
        .get("allow_free_text")
        .or_else(|| raw.get("isOther"))
        .or_else(|| raw.get("allowOther"))
        .and_then(Value::as_bool)
        .unwrap_or(options.is_empty() && !is_header);
    let kind = if matches!(explicit_kind.as_str(), "header" | "single_select" | "multi_select" | "free_text" | "boolean" | "number") {
        explicit_kind
    } else if is_header {
        "header".into()
    } else if multi && !options.is_empty() {
        "multi_select".into()
    } else if !options.is_empty() {
        "single_select".into()
    } else {
        "free_text".into()
    };
    if prompt.is_empty() {
        prompt = question_id.clone();
    }
    let required = if kind == "header" {
        false
    } else {
        raw.get("required").and_then(Value::as_bool).unwrap_or(true)
    };
    Some(json!({
        "question_id": question_id,
        "kind": kind,
        "prompt": prompt,
        "required": required,
        "options": options,
        "allow_free_text": if kind == "header" { false } else { allow_free_text },
        "placeholder": str_field(raw, &["placeholder"]),
    }))
}

pub fn inject_user_input(
    doc: &mut Value,
    payload: &Value,
    epoch: &str,
    now: &str,
) -> Result<(Vec<(&'static str, Value)>, Value), (axum::http::StatusCode, &'static str, String)> {
    use axum::http::StatusCode;
    if std::env::var("MUTEKI_ALLOW_USER_INPUT_FIXTURES").unwrap_or_default().trim() != "1" {
        return Err((
            StatusCode::FORBIDDEN,
            "conversation.user_input.fixture_disabled",
            "Set MUTEKI_ALLOW_USER_INPUT_FIXTURES=1 to inject fixtures".into(),
        ));
    }
    let req_id = str_field(payload, &["request_id"]);
    let request_id = if req_id.is_empty() {
        format!("req-{}", uuid::Uuid::new_v4().simple())
    } else {
        req_id.to_owned()
    };
    let questions: Vec<Value> = match payload.get("questions").and_then(Value::as_array) {
        Some(items) if !items.is_empty() => items
            .iter()
            .enumerate()
            .filter_map(|(i, q)| normalize_question_item(q, i))
            .collect(),
        _ => c22_fixture_questions(),
    };
    let mut events = Vec::new();
    let mut turn_id = doc["thread"]["state"]["running_turn_id"]
        .as_str()
        .unwrap_or("")
        .to_owned();
    if turn_id.is_empty() {
        turn_id = format!("turn-{}", uuid::Uuid::new_v4().simple());
        if !doc["turns"].is_array() {
            doc["turns"] = json!([]);
        }
        let seq = doc["turns"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|t| t["seq"].as_u64())
            .max()
            .unwrap_or(0)
            + 1;
        let turn_text = str_field(payload, &["turn_text"]);
        let turn_text = if turn_text.is_empty() { "[c22 fixture]" } else { turn_text };
        doc["turns"].as_array_mut().unwrap().push(json!({
            "turn_id": turn_id,
            "seq": seq,
            "kind": "message",
            "text": turn_text,
            "status": "running",
            "process_recorded": true,
            "created_at": now,
        }));
        doc["thread"]["state"]["running_turn_id"] = json!(turn_id);
        doc["thread"]["state"]["status"] = json!("running");
        doc["runtime_epoch"] = json!(epoch);
        events.push(("core.turn.started", json!({"turn_id": turn_id, "fixture": true})));
    }
    let first_prompt = questions
        .iter()
        .find(|q| q["kind"] != "header")
        .or_else(|| questions.first())
        .and_then(|q| q["prompt"].as_str())
        .unwrap_or("")
        .to_owned();
    let title = str_field(payload, &["title", "message"]);
    let title = if title.is_empty() { first_prompt.as_str() } else { title };
    let message = str_field(payload, &["message", "title"]);
    let message = if message.is_empty() { first_prompt.as_str() } else { message };
    let kind = str_field(payload, &["user_input_kind"]);
    let kind = if kind.is_empty() { "fixture" } else { kind };
    let pending = json!({
        "request_id": request_id,
        "turn_id": turn_id,
        "user_input_kind": kind,
        "title": title,
        "message": message,
        "question": first_prompt,
        "questions": questions,
        "fixture": true,
        "status": "pending",
        "requested_at": now,
        "native": payload.get("native").filter(|v| v.is_object()).cloned().unwrap_or_else(|| json!({})),
    });
    doc["thread"]["state"]["pending_user_input"] = pending.clone();
    if doc["thread"]["state"]["plan"].is_object()
        && !matches!(doc["thread"]["state"]["plan"]["phase"].as_str(), Some("cleared" | "unsupported"))
    {
        let summary = if first_prompt.is_empty() { "等待用户输入" } else { first_prompt.as_str() };
        doc["thread"]["state"]["plan"]["phase"] = json!("awaiting_decision");
        doc["thread"]["state"]["plan"]["awaiting"] = json!({"kind": "user_input", "request_id": request_id, "summary": summary});
        doc["thread"]["state"]["plan"]["last_change_summary"] = json!("进入等待决策（用户输入）");
    }
    let output = json!({
        "request_id": request_id,
        "turn_id": turn_id,
        "questions": pending["questions"],
    });
    events.push(("core.user_input.requested", pending));
    Ok((events, output))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn attachment_delivery_matches_text_blocks_and_omits_image_payload() {
        let image = b"synthetic image bytes".to_vec();
        let (content, delivery) = attachment_content_and_delivery("Hello", vec![
            (json!({"name":"notes.txt","media_type":"text/plain","artifact_id":"text-ref"}), "你好\nEND ATTACHED FILE CONTENTS".as_bytes().to_vec()),
            (json!({"name":"photo.png","media_type":"image/png","artifact_id":"image-ref","secret":"must not copy"}), image.clone()),
        ]).unwrap();
        assert_eq!(content[0]["text"], "Hello");
        let sent_text = content[1]["text"].as_str().unwrap();
        assert_eq!(delivery[0]["text"], sent_text);
        assert_eq!(delivery[0]["chars"], sent_text.chars().count());
        assert_eq!(delivery[0]["artifact_id"], "text-ref");
        assert_eq!(delivery[1]["artifact_id"], "image-ref");
        assert_eq!(delivery[1]["size"], image.len());
        assert!(
            content[2]["image_url"]["url"]
                .as_str()
                .unwrap()
                .starts_with("data:image/png;base64,")
        );
        assert!(delivery[1].get("text").is_none());
        assert!(!serde_json::to_string(&delivery).unwrap().contains("base64"));
        assert!(delivery[1].get("secret").is_none());
        assert!(
            attachment_content_and_delivery(
                "",
                vec![(
                    json!({"name":"bad","media_type":"application/pdf"}),
                    vec![0]
                )]
            )
            .is_err()
        );
    }
    #[test]
    fn replay_keeps_user_and_assistant_history() {
        let rows = vec![
            json!({"message_id":"u1","role":"user","text":"Hello","turn_id":"t1"}),
            json!({"message_id":"a1","role":"assistant","text":"Hi","turn_id":"t1"}),
        ];
        let folded = fold_messages(&rows);
        assert_eq!(folded.len(), 2);
        assert_eq!(folded[0]["text"], "Hello");
        assert_eq!(folded[1]["text"], "Hi");
        assert_eq!(folded[1]["turn_id"], "t1");
    }
}
