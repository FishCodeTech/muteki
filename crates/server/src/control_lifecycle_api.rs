//! User-driven thread lifecycle changes for the conversation sidebar, plus the
//! per-thread annotations (pull requests, auto-settle override) it lists.
use crate::{AppState, chat, control, control_lifecycle};
use axum::{
    Json, Router,
    extract::{Path, State},
    http::StatusCode,
    routing::{get, post},
};
use serde_json::{Value, json};
use std::{collections::BTreeMap, sync::Arc};

const ACTIONS: [&str; 9] = [
    "settle",
    "unsettle",
    "pin",
    "unpin",
    "mark_read",
    "mark_unread",
    "follow_up",
    "park",
    "keep",
];

fn refused(error: &Value) -> chat::Failure {
    let code = error["code"].as_str().unwrap_or("control.lifecycle.failed");
    let message = error["message"]
        .as_str()
        .map(str::to_owned)
        .unwrap_or_else(|| error.to_string());
    let status = if code == "control.thread_busy" {
        StatusCode::CONFLICT
    } else {
        StatusCode::BAD_REQUEST
    };
    control::failure(status, code, message)
}

fn lifecycle_json(s: &AppState, id: &str) -> Result<Value, chat::Failure> {
    let doc = control::thread_doc(s, id)?;
    serde_json::to_value(control::read_lifecycle(&doc)).map_err(control::storage)
}

/// `state.unread` is what listings and the inbox show; the lifecycle flag only
/// records the request, so both move together.
async fn sync_unread(s: &Arc<AppState>, id: &str, unread: bool) -> Result<(), chat::Failure> {
    let _guard = s.chat.active.lock().await;
    let mut doc = chat::load(s, id)?;
    let head = chat::stream_head(&doc);
    doc["thread"]["state"]["unread"] = json!(unread);
    if !unread {
        doc["thread"]["state"]["head_stream_seq"] = json!(head);
        doc["thread"]["state"]["read_stream_seq"] = json!(head);
    }
    chat::save(s, id, &mut doc)?;
    Ok(())
}

async fn change(
    State(s): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> chat::Api {
    let action = body["action"].as_str().map(str::trim).unwrap_or("");
    if !ACTIONS.contains(&action) {
        return Err(control::failure(
            StatusCode::BAD_REQUEST,
            "control.lifecycle.invalid_action",
            format!("action must be one of: {}", ACTIONS.join(", ")),
        ));
    }
    let thread = chat::load(&s, &id)?;
    if thread["thread"]["archived"] == true {
        return Err(control::failure(
            StatusCode::CONFLICT,
            "control.lifecycle.archived",
            "Unarchive this thread before changing its lifecycle.",
        ));
    }
    let was = control::read_lifecycle(&control::thread_doc(&s, &id)?);
    if action == "settle" {
        control_lifecycle::settle_thread_now(&s, "user", &id, None)
            .await
            .map_err(|error| refused(&error))?;
    } else {
        let mut args = json!({"action": action, "thread_id": id});
        for key in ["on", "note", "snooze_hours"] {
            if !body[key].is_null() {
                args[key] = body[key].clone();
            }
        }
        let applied = control_lifecycle::update_threads(&s, "user", &args)
            .await
            .map_err(|error| refused(&error))?;
        let outcome = &applied["outcomes"][0];
        if outcome["ok"] != true {
            let detail = outcome["error"]
                .as_str()
                .or_else(|| outcome["detail"].as_str())
                .unwrap_or("The lifecycle change was refused.");
            return Err(control::failure(
                StatusCode::CONFLICT,
                "control.lifecycle.refused",
                detail,
            ));
        }
        let mut ctrl = control::thread_doc(&s, &id)?;
        let on = body["on"].as_bool().unwrap_or(true);
        match action {
            "unsettle" => ctrl["manual_unsettle"] = json!(true),
            "pin" if was.settled => ctrl["manual_unsettle"] = json!(true),
            "keep" => {
                ctrl["kept_at"] = if on {
                    json!(chrono::Utc::now().timestamp())
                } else {
                    Value::Null
                }
            }
            "follow_up" if on && !was.follow_up => ctrl["follow_up_since"] = json!(control::now()),
            "park" if on => ctrl["parked_at"] = json!(control::now()),
            _ => {}
        }
        control::save_thread_doc(&s, &id, &ctrl)?;
        match action {
            "mark_read" => sync_unread(&s, &id, false).await?,
            "mark_unread" => sync_unread(&s, &id, true).await?,
            _ => {}
        }
    }
    let ctrl = control::thread_doc(&s, &id)?;
    let current = chat::load(&s, &id)?;
    Ok(Json(json!({
        "thread_id": id,
        "action": action,
        "lifecycle": lifecycle_json(&s, &id)?,
        "unread": current["thread"]["state"]["unread"] == true,
        "auto_settle_disabled": !ctrl["kept_at"].is_null(),
    })))
}

/// `POST /api/threads/{id}/move`: the user reassigns a thread to another
/// project. Same rules as the control move: no worktree or provider-native
/// mirror, and an idle thread only; the provider session restarts there.
async fn move_to_project(
    State(s): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> chat::Api {
    let project_id = body["project_id"].as_str().map(str::trim).unwrap_or("");
    if project_id.is_empty() {
        return Err(control::failure(
            StatusCode::BAD_REQUEST,
            "control.move_thread.project_required",
            "project_id is required",
        ));
    }
    let project = s
        .store
        .get("projects", project_id)
        .map_err(control::storage)?
        .ok_or_else(|| {
            control::failure(
                StatusCode::NOT_FOUND,
                "control.move_thread.project_missing",
                "The destination project does not exist.",
            )
        })?;
    let guard = s.chat.active.lock().await;
    let mut doc = chat::load(&s, &id)?;
    if doc["thread"]["state"]["lineage"]["native_subagent"].is_string() {
        return Err(control::failure(
            StatusCode::CONFLICT,
            "control.move_thread.native_mirror",
            "A provider-native mirror thread is read-only.",
        ));
    }
    if doc["thread"]["state"]["lineage"]["isolation"] == "worktree" {
        return Err(control::failure(
            StatusCode::CONFLICT,
            "control.move_thread.worktree",
            "A thread with a dedicated worktree cannot be moved; merge it back first.",
        ));
    }
    let blockers = crate::control_state::blockers(&doc);
    if blockers.live_run || blockers.background_work || blockers.pending_approval || blockers.pending_input {
        return Err(control::failure(
            StatusCode::CONFLICT,
            "control.thread_busy",
            "The thread cannot be moved while it is working or waiting for input.",
        ));
    }
    if doc["thread"]["project_id"] == project_id {
        return Ok(Json(json!({"thread_id": id, "project_id": project_id, "changed": false})));
    }
    let workspace = s
        .store
        .list("workspaces")
        .map_err(control::storage)?
        .into_iter()
        .find(|workspace| {
            workspace["project_id"] == project_id
                && workspace["settings"]["mode"].as_str().is_none_or(str::is_empty)
        })
        .ok_or_else(|| {
            control::failure(
                StatusCode::CONFLICT,
                "control.move_thread.no_checkout",
                "The destination project has no checkout.",
            )
        })?;
    doc["thread"]["project_id"] = json!(project_id);
    doc["thread"]["workspace_id"] = workspace["workspace_id"].clone();
    chat::save(&s, &id, &mut doc)?;
    drop(guard);
    crate::native_chat::close(&s, &id, "project_moved").await;
    control::audit(&s, "user", &id, "move_thread", json!({"project_id": project_id}));
    Ok(Json(json!({
        "thread_id": id,
        "project_id": project_id,
        "project_name": project["name"],
        "changed": true,
    })))
}

/// One read for every row: linked pull requests and the per-thread auto-settle
/// override live in the control document, not in the thread listing.
async fn annotations(State(s): State<Arc<AppState>>) -> chat::Api {
    let mut threads = BTreeMap::<String, Value>::new();
    for (id, doc) in s.store.entries(control::THREADS).map_err(control::storage)? {
        let pull_requests = doc["pull_requests"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|link| !matches!(link["source"].as_str(), Some("dismissed" | "created")))
            .map(|link| {
                json!({
                    "host": link["key"]["host"],
                    "repository": link["key"]["repository"],
                    "number": link["key"]["number"],
                    "url": link["url"],
                    "state": link["snapshot"]["state"],
                    "title": link["snapshot"]["title"],
                    "is_draft": link["snapshot"]["is_draft"],
                })
            })
            .collect::<Vec<_>>();
        let auto_settle_disabled = !doc["kept_at"].is_null();
        if pull_requests.is_empty() && !auto_settle_disabled {
            continue;
        }
        threads.insert(
            id,
            json!({"pull_requests": pull_requests, "auto_settle_disabled": auto_settle_disabled}),
        );
    }
    Ok(Json(json!({"threads": threads})))
}

pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/control/threads/{id}/lifecycle", post(change))
        .route("/api/control/thread-annotations", get(annotations))
        .route("/api/threads/{id}/move", post(move_to_project))
}
