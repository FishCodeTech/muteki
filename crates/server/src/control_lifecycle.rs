//! Conversation organization, settlement, review triage, deletion and settings
//! mutation tools for `muteki-control`.

use crate::{
    AppState, chat,
    control::{self, ArmedSelfSettle, ReviewRecord},
};
use serde_json::{Value, json};
use std::sync::Arc;

fn check_expectation(doc: &Value, ctrl_doc: &Value, expect: &Value) -> Result<(), String> {
    if !expect.is_object() {
        return Ok(());
    }
    if let Some(exp_status) = expect["status"].as_str() {
        let actual = doc["thread"]["state"]["status"].as_str().unwrap_or("idle");
        if actual != exp_status {
            return Err(format!("expected status `{exp_status}`, got `{actual}`"));
        }
    }
    if let Some(exp_archived) = expect["archived"].as_bool() {
        let actual = doc["thread"]["archived"].as_bool().unwrap_or(false);
        if actual != exp_archived {
            return Err(format!("expected archived={exp_archived}, got {actual}"));
        }
    }
    if let Some(exp_settled) = expect["settled"].as_bool() {
        let actual = control::read_lifecycle(ctrl_doc).settled;
        if actual != exp_settled {
            return Err(format!("expected settled={exp_settled}, got {actual}"));
        }
    }
    if let Some(exp_turn) = expect["last_turn_id"].as_str() {
        let actual = doc["turns"]
            .as_array()
            .and_then(|turns| turns.iter().rev().find(|t| t["status"] != "superseded"))
            .and_then(|t| t["turn_id"].as_str())
            .unwrap_or("");
        if actual != exp_turn {
            return Err(format!(
                "expected last_turn_id `{exp_turn}`, got `{actual}`"
            ));
        }
    }
    Ok(())
}

/// Apply settlement side effects to a thread: close active PTY terminals, pause remaining queue,
/// mark `lifecycle.settled = true`, and mirror onto `conversation_threads`.
pub async fn settle_thread_now(
    s: &Arc<AppState>,
    actor: &str,
    target: &str,
    summary: Option<&str>,
) -> Result<Value, Value> {
    let guard = s.chat.active.lock().await;
    let mut doc = chat::load(s, target).map_err(|(_, b)| b.0["error"].clone())?;
    let blockers = crate::control_state::blockers(&doc);
    if crate::control_subagents::busy(&doc)
        || blockers.unsettled_children
        || blockers.pending_approval
        || blockers.pending_human_message
        || (blockers.pending_input && doc["thread"]["state"]["pending_user_input"]["native"]["background"] != true)
        || doc["thread"]["archived"] == true {
        return Err(json!({"code":"control.thread_busy","message":"The thread is busy or archived and cannot be settled."}));
    }
    let provider_detached = doc["native_session"]["open"] == true;
    let terminals_checked = s.terminals.settle_for_thread(target);
    let queued_dropped = doc["queue"].as_array().map_or(0, |rows| rows.iter().filter(|row| row["origin"] != "human").count());
    if let Some(queue) = doc["queue"].as_array_mut() { queue.retain(|row| row["origin"] == "human"); }
    let input_closed = blockers.pending_input;
    if input_closed { doc["thread"]["state"]["pending_user_input"] = Value::Null; }
    doc["thread"]["state"]["delivery_settled"] = json!(true);
    doc["native_session"]["open"] = json!(false);
    doc["native_session"]["close_reason"] = json!("settled");
    let mut ctrl = control::thread_doc(s, target).map_err(|(_, b)| b.0["error"].clone())?;
    let mut lifecycle = control::read_lifecycle(&ctrl);
    let unpinned = lifecycle.pinned;
    lifecycle.pinned = false;
    lifecycle.settled = true;
    if lifecycle.settled_at.is_none() { lifecycle.settled_at = Some(control::now()); }
    if let Some(summary) = summary { lifecycle.settled_summary = Some(summary.to_owned()); }
    let mut watches_ended = 0;
    for link in ctrl["pull_requests"].as_array_mut().into_iter().flatten() {
        if !link["watch"].is_null() { watches_ended += 1; link["watch"] = Value::Null; }
    }
    ctrl["lifecycle"] = serde_json::to_value(&lifecycle).map_err(|error|json!({"code":"control.serialization_failed","message":error.to_string()}))?;
    ctrl["manual_unsettle"] = json!(false);
    ctrl["self_settle"] = Value::Null;
    ctrl["canonical_self_settle"] = Value::Null;
    chat::save(s, target, &mut doc).map_err(|(_, b)| b.0["error"].clone())?;
    control::save_thread_doc(s, target, &ctrl).map_err(|(_, b)| b.0["error"].clone())?;
    crate::native_chat::close(s, target, "settled").await;
    drop(guard);
    control::sync_lifecycle_mirror(s, target, &lifecycle).map_err(|(_, b)| b.0["error"].clone())?;
    crate::chat_subagents::sync_node(s, target).map_err(|(_, b)| b.0["error"].clone())?;
    control::emit(s, target, "core.control.updated", json!({"thread_id":target,"kind":"settled","lifecycle":lifecycle})).map_err(|(_, b)| b.0["error"].clone())?;
    let effects = json!({"provider_detached":provider_detached,"pull_request_watches_ended":watches_ended,"queued_dropped":queued_dropped,"terminals_checked":terminals_checked,"unpinned":unpinned,"input_closed":input_closed});
    control::audit(s, actor, target, "settle_thread", effects.clone());
    Ok(json!({"thread_id":target,"settled":true,"side_effects":effects}))
}

/// `muteki_update_threads`: batch 12 lifecycle operations with `expect` precondition & `dry_run`.
pub async fn update_threads(s: &Arc<AppState>, caller: &str, args: &Value) -> Result<Value, Value> {
    let action = args["action"].as_str().map(str::trim).unwrap_or("");
    const ACTIONS: [&str; 12] = [
        "settle",
        "unsettle",
        "archive",
        "unarchive",
        "pin",
        "unpin",
        "rename",
        "mark_read",
        "mark_unread",
        "follow_up",
        "park",
        "keep",
    ];
    if !ACTIONS.contains(&action) {
        return Err(json!({
            "code": "control.update_threads.invalid_action",
            "category": "validation",
            "message": format!("action must be one of: {}", ACTIONS.join(", "))
        }));
    }

    let dry_run = args["dry_run"].as_bool().unwrap_or(false);
    let expect = &args["expect"];
    let mut targets = Vec::<String>::new();
    if let Some(arr) = args["thread_ids"].as_array() {
        for v in arr.iter().filter_map(Value::as_str) {
            let resolved = control::resolve_thread_id(caller, v).to_owned();
            if !resolved.is_empty() && !targets.contains(&resolved) {
                targets.push(resolved);
            }
        }
    }
    if let Some(single) = args["thread_id"].as_str().filter(|v| !v.trim().is_empty()) {
        let resolved = control::resolve_thread_id(caller, single).to_owned();
        if !targets.contains(&resolved) {
            targets.push(resolved);
        }
    }
    if targets.is_empty() {
        return Err(json!({
            "code": "control.update_threads.targets_required",
            "category": "validation",
            "message": "specify `thread_id` or `thread_ids`"
        }));
    }
    if action == "rename" && targets.len() > 1 {
        return Err(json!({
            "code": "control.update_threads.rename_single_only",
            "category": "validation",
            "message": "`rename` only operates on a single thread at a time"
        }));
    }

    let mut outcomes = Vec::<Value>::new();
    for target in &targets {
        let Ok(doc) = chat::load(s, target) else {
            outcomes.push(json!({
                "thread_id": target,
                "ok": false,
                "error": "thread not found",
            }));
            continue;
        };
        let ctrl_doc = control::thread_doc(s, target).unwrap_or(Value::Null);
        if let Err(mismatch) = check_expectation(&doc, &ctrl_doc, expect) {
            outcomes.push(json!({
                "thread_id": target,
                "ok": false,
                "skipped_reason": "expectation_mismatch",
                "detail": mismatch,
            }));
            continue;
        }
        if action == "settle" && doc["thread"]["state"]["status"] == "running" {
            outcomes.push(json!({
                "thread_id": target,
                "ok": false,
                "error": if target == caller {
                    "cannot immediately settle own running turn; call muteki_settle_self(when=\"end_of_turn\", consent_quote=...) instead"
                } else {
                    "thread is currently running; wait for idle or interrupt first"
                },
            }));
            continue;
        }

        if dry_run {
            outcomes.push(json!({
                "thread_id": target,
                "ok": true,
                "dry_run": true,
                "would_apply": action,
            }));
            continue;
        }

        match action {
            "settle" => {
                let summary = args["summary"].as_str().or_else(|| args["note"].as_str());
                match settle_thread_now(s, caller, target, summary).await {
                    Ok(res) => {
                        outcomes.push(json!({"thread_id": target, "ok": true, "result": res}))
                    }
                    Err(err) => {
                        outcomes.push(json!({"thread_id": target, "ok": false, "error": err}))
                    }
                }
            }
            "archive" | "unarchive" => {
                let cmd_type = if action == "archive" {
                    "conversation.thread.archive"
                } else {
                    "conversation.thread.unarchive"
                };
                let cmd_id = format!("ctrl-{action}:{target}:{}", uuid::Uuid::new_v4().simple());
                let body = json!({
                    "command_id": cmd_id,
                    "command_type": cmd_type,
                    "payload": {},
                    "actor": {"kind": "agent", "id": caller},
                });
                match chat::command_from_platform(s.clone(), target.clone(), body, None).await {
                    Ok(_) => {
                        control::audit(s, caller, target, action, json!({}));
                        outcomes.push(json!({"thread_id": target, "ok": true, "action": action}));
                    }
                    Err((_, axum::Json(b))) => {
                        outcomes
                            .push(json!({"thread_id": target, "ok": false, "error": b["error"]}));
                    }
                }
            }
            "rename" => {
                let new_title = args["title"].as_str().map(str::trim).unwrap_or("");
                if new_title.is_empty() {
                    return Err(json!({
                        "code": "control.update_threads.title_required",
                        "category": "validation",
                        "message": "`title` is required for action=\"rename\""
                    }));
                }
                let cmd_id = format!("ctrl-rename:{target}:{}", uuid::Uuid::new_v4().simple());
                let body = json!({
                    "command_id": cmd_id,
                    "command_type": "conversation.thread.rename",
                    "payload": {"title": new_title},
                    "actor": {"kind": "agent", "id": caller},
                });
                match chat::command_from_platform(s.clone(), target.clone(), body, None).await {
                    Ok(_) => {
                        control::audit(
                            s,
                            caller,
                            target,
                            "rename_thread",
                            json!({"title": new_title}),
                        );
                        outcomes.push(json!({"thread_id": target, "ok": true, "title": new_title}));
                    }
                    Err((_, axum::Json(b))) => {
                        outcomes
                            .push(json!({"thread_id": target, "ok": false, "error": b["error"]}));
                    }
                }
            }
            _ => {
                let mut cdoc = control::thread_doc(s, target)
                    .map_err(|(_, axum::Json(b))| b["error"].clone())?;
                let mut lc = control::read_lifecycle(&cdoc);
                match action {
                    "unsettle" => {
                        lc.settled = false;
                        lc.settled_at = None;
                        let mut thread =
                            chat::load(s, target).map_err(|(_, b)| b.0["error"].clone())?;
                        thread["thread"]["state"]["delivery_settled"] = json!(false);
                        chat::save(s, target, &mut thread)
                            .map_err(|(_, b)| b.0["error"].clone())?;
                        crate::chat_subagents::sync_node(s, target)
                            .map_err(|(_, b)| b.0["error"].clone())?;
                    }
                    "pin" => {
                        lc.pinned = true;
                        lc.settled = false;
                        lc.settled_at = None;
                        let mut thread = chat::load(s, target).map_err(|(_, body)| body.0["error"].clone())?;
                        thread["thread"]["state"]["delivery_settled"] = json!(false);
                        chat::save(s, target, &mut thread).map_err(|(_, body)| body.0["error"].clone())?;
                        crate::chat_subagents::sync_node(s, target).map_err(|(_, body)| body.0["error"].clone())?;
                    },
                    "unpin" => lc.pinned = false,
                    "mark_read" => lc.unread = false,
                    "mark_unread" => lc.unread = true,
                    "follow_up" => {
                        lc.follow_up = args["on"].as_bool().unwrap_or(true);
                        lc.follow_up_note = if lc.follow_up { args["note"].as_str().map(str::to_owned) } else { None };
                    }
                    "park" => {
                        lc.parked = args["on"].as_bool().unwrap_or(true);
                        if let Some(hours) = args["snooze_hours"].as_u64() {
                            lc.snoozed_until = Some(
                                (chrono::Utc::now() + chrono::Duration::hours(hours as i64))
                                    .to_rfc3339(),
                            );
                        }
                    }
                    "keep" => {}
                    _ => {}
                }
                cdoc["lifecycle"] = serde_json::to_value(&lc).unwrap_or(Value::Null);
                control::save_thread_doc(s, target, &cdoc)
                    .map_err(|(_, axum::Json(b))| b["error"].clone())?;
                let _ = control::sync_lifecycle_mirror(s, target, &lc);
                let _ = control::emit(
                    s,
                    target,
                    "core.control.updated",
                    json!({"thread_id": target, "kind": action, "lifecycle": lc}),
                );
                control::audit(s, caller, target, action, json!({"lifecycle": lc}));
                outcomes.push(json!({"thread_id": target, "ok": true, "lifecycle": lc}));
            }
        }
    }

    Ok(json!({
        "action": action,
        "dry_run": dry_run,
        "outcomes": outcomes,
    }))
}

/// `muteki_record_review`: attach a structured triage / review conclusion with evidence anchors.
pub fn record_review(s: &Arc<AppState>, caller: &str, args: &Value) -> Result<Value, Value> {
    let target = control::resolve_thread_id(
        caller,
        args["thread_id"].as_str().unwrap_or(control::OWN_THREAD),
    );
    let summary = args["summary"].as_str().map(str::trim).unwrap_or("");
    if summary.is_empty() {
        return Err(json!({
            "code": "control.review.summary_required",
            "category": "validation",
            "message": "`summary` is required"
        }));
    }
    let disposition = args["disposition"]
        .as_str()
        .unwrap_or("keep")
        .trim()
        .to_owned();
    let evidence_anchors: Vec<String> = args["evidence_anchors"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_owned)
        .collect();

    let _ = chat::load(s, target).map_err(|(_, axum::Json(b))| b["error"].clone())?;
    let record = ReviewRecord {
        summary: summary.to_owned(),
        disposition,
        evidence_anchors,
        reviewer_thread_id: caller.to_owned(),
        updated_at: control::now(),
    };
    let mut ctrl_doc =
        control::thread_doc(s, target).map_err(|(_, axum::Json(b))| b["error"].clone())?;
    ctrl_doc["review"] = serde_json::to_value(&record).unwrap_or(Value::Null);
    control::save_thread_doc(s, target, &ctrl_doc)
        .map_err(|(_, axum::Json(b))| b["error"].clone())?;
    let _ = control::emit(
        s,
        target,
        "core.control.updated",
        json!({"thread_id": target, "kind": "review_recorded", "review": record}),
    );
    control::audit(
        s,
        caller,
        target,
        "record_review",
        json!({"disposition": record.disposition, "evidence_anchors": record.evidence_anchors}),
    );
    Ok(json!({"thread_id": target, "review": record}))
}

/// `muteki_settle_self`: schedule auto-settlement of `caller` ONLY when `consent_quote` is verified
/// verbatim inside a human-authored user message of this thread.
pub fn settle_self(s: &Arc<AppState>, caller: &str, args: &Value) -> Result<Value, Value> {
    if args["cancel"].as_bool() == Some(true) {
        let mut ctrl_doc =
            control::thread_doc(s, caller).map_err(|(_, axum::Json(b))| b["error"].clone())?;
        let had = !ctrl_doc["self_settle"].is_null();
        ctrl_doc["self_settle"] = Value::Null;
        control::save_thread_doc(s, caller, &ctrl_doc)
            .map_err(|(_, axum::Json(b))| b["error"].clone())?;
        let _ = control::emit(
            s,
            caller,
            "core.control.updated",
            json!({"thread_id": caller, "kind": "self_settle_disarmed"}),
        );
        return Ok(json!({"armed": false, "disarmed": had}));
    }

    let raw_when = args["when"].as_str().unwrap_or("end_of_turn");
    let when = if raw_when == "turn_end" {
        "end_of_turn"
    } else {
        raw_when
    };
    if when != "end_of_turn" {
        return Err(json!({
            "code": "control.settle_self.invalid_when",
            "category": "validation",
            "message": "`when` must be \"end_of_turn\"; PR-triggered settlement is not implemented"
        }));
    }
    let quote = args["consent_quote"].as_str().map(str::trim).unwrap_or("");
    let doc = chat::load(s, caller).map_err(|(_, axum::Json(b))| b["error"].clone())?;

    let armed_turn_id = doc["thread"]["state"]["running_turn_id"]
        .as_str()
        .unwrap_or("")
        .to_owned();
    let armed = ArmedSelfSettle {
        when: when.to_owned(),
        summary: args["summary"].as_str().unwrap_or("").trim().to_owned(),
        consent_quote: quote.to_owned(),
        armed_at: control::now(),
        armed_turn_id,
    };

    let mut ctrl_doc =
        control::thread_doc(s, caller).map_err(|(_, axum::Json(b))| b["error"].clone())?;
    ctrl_doc["self_settle"] = serde_json::to_value(&armed).unwrap_or(Value::Null);
    control::save_thread_doc(s, caller, &ctrl_doc)
        .map_err(|(_, axum::Json(b))| b["error"].clone())?;
    let _ = control::emit(
        s,
        caller,
        "core.control.updated",
        json!({"thread_id": caller, "kind": "self_settle_armed", "self_settle": armed}),
    );
    control::audit(
        s,
        caller,
        caller,
        "settle_self_armed",
        json!({"when": armed.when, "consent_quote": armed.consent_quote}),
    );
    Ok(json!({
        "armed": true,
        "self_settle": armed,
    }))
}

/// `muteki_delete_threads` (`Category::Delete`): permanently delete non-running target threads.
pub async fn delete_threads(s: &Arc<AppState>, caller: &str, args: &Value) -> Result<Value, Value> {
    let dry_run = args["dry_run"].as_bool().unwrap_or(false);
    let mut targets = Vec::<String>::new();
    if let Some(arr) = args["thread_ids"].as_array() {
        for v in arr.iter().filter_map(Value::as_str) {
            let resolved = control::resolve_thread_id(caller, v).to_owned();
            if !resolved.is_empty() && !targets.contains(&resolved) {
                targets.push(resolved);
            }
        }
    }
    if let Some(single) = args["thread_id"].as_str().filter(|v| !v.trim().is_empty()) {
        let resolved = control::resolve_thread_id(caller, single).to_owned();
        if !targets.contains(&resolved) {
            targets.push(resolved);
        }
    }
    if targets.is_empty() {
        return Err(json!({
            "code": "control.delete.targets_required",
            "category": "validation",
            "message": "specify `thread_id` or `thread_ids`"
        }));
    }
    if targets.iter().any(|t| t == caller) {
        return Err(json!({
            "code": "control.delete.self_forbidden",
            "category": "validation",
            "message": "a thread cannot permanently delete itself"
        }));
    }

    let mut outcomes = Vec::<Value>::new();
    for target in &targets {
        let Ok(doc) = chat::load(s, target) else {
            outcomes.push(json!({"thread_id": target, "ok": false, "error": "thread not found"}));
            continue;
        };
        if doc["thread"]["state"]["status"] == "running" {
            outcomes.push(json!({
                "thread_id": target,
                "ok": false,
                "error": "thread is currently running; interrupt or cancel it before deleting",
            }));
            continue;
        }
        if dry_run {
            outcomes.push(json!({
                "thread_id": target,
                "title": doc["thread"]["title"],
                "ok": true,
                "dry_run": true,
            }));
            continue;
        }
        let cmd_id = format!("ctrl-delete:{target}:{}", uuid::Uuid::new_v4().simple());
        let body = json!({
            "command_id": cmd_id,
            "command_type": "conversation.thread.delete",
            "payload": {},
            "actor": {"kind": "agent", "id": caller},
        });
        match chat::command_from_platform(s.clone(), target.clone(), body, None).await {
            Ok(_) => {
                let _ = s.store.delete(control::THREADS, target);
                control::audit(
                    s,
                    caller,
                    target,
                    "delete_thread",
                    json!({"title": doc["thread"]["title"]}),
                );
                outcomes.push(json!({"thread_id": target, "ok": true, "deleted": true}));
            }
            Err((_, axum::Json(b))) => {
                outcomes.push(json!({"thread_id": target, "ok": false, "error": b["error"]}));
            }
        }
    }

    Ok(json!({
        "dry_run": dry_run,
        "outcomes": outcomes,
    }))
}

/// Uses the same preference schema and revision checks as the settings page.
pub async fn patch_settings(s: &Arc<AppState>, caller: &str, args: &Value) -> Result<Value, Value> {
    let patch = args["patch"].as_object().ok_or_else(
        || json!({"code":"control.patch_invalid","message":"patch must be an object"}),
    )?;
    let current = crate::compat::get_ui(axum::extract::State(s.clone()))
        .await
        .map_err(|(_, b)| b.0)?;
    let mut values = current.0["values"].as_object().cloned().unwrap_or_default();
    values.extend(patch.clone());
    let (status, result) = crate::compat::put_ui(
        axum::extract::State(s.clone()),
        axum::Json(json!({"version":current.0["version"],"values":values})),
    )
    .await
    .map_err(|(_, b)| b.0)?;
    if !status.is_success() {
        return Err(
            json!({"code":"control.patch_conflict","message":"UI settings changed concurrently","current":result.0}),
        );
    }
    control::audit(
        s,
        caller,
        caller,
        "patch_settings",
        json!({"keys":patch.keys().collect::<Vec<_>>()}),
    );
    Ok(result.0)
}
