use super::*;
use axum::{
    Json,
    body::to_bytes,
    extract::{Path, State},
};
use std::{fs::OpenOptions, sync::Arc};

const MONITOR_HEALTH: &str = "system";

pub(super) async fn lock_task(s: &AppState, task_id: &str) -> Api<std::fs::File> {
    let dir = crate::native_chat::state_dir(s).join("src_task_lifecycle_locks");
    let path = dir.join(format!("{}.lock", crate::runs::encode_run_id(task_id)));
    tokio::task::spawn_blocking(move || {
        std::fs::create_dir_all(dir)?;
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .open(path)?;
        fs2::FileExt::lock_exclusive(&file)?;
        Ok::<_, std::io::Error>(file)
    })
    .await
    .map_err(db)?
    .map_err(db)
}

fn operation_error(error: (StatusCode, Json<Value>)) -> Error {
    let message = error.1.0["detail"]
        .as_str()
        .map(str::to_owned)
        .unwrap_or_else(|| error.1.0.to_string());
    Error::new(
        if error.0 == StatusCode::CONFLICT {
            StatusCode::INTERNAL_SERVER_ERROR
        } else {
            error.0
        },
        "src.thread_lifecycle_failed",
        message,
    )
}

async fn thread_command(
    s: Arc<AppState>,
    thread_id: &str,
    command_id: &str,
    kind: &str,
    mut payload: Value,
) -> Api<()> {
    if let Some(map) = payload.as_object_mut() {
        map.insert("internal_src_lifecycle".to_owned(), json!(true));
    }
    let _ = crate::chat::command(
        State(s),
        Path(thread_id.to_owned()),
        Json(json!({"command_id":command_id,"command_type":kind,"payload":payload})),
    )
    .await
    .map_err(operation_error)?;
    Ok(())
}

async fn stop_bound_runs(s: &Arc<AppState>, task_id: &str, command_id: &str) -> Api<()> {
    for binding in records(s, task_id, "run_binding")? {
        if !binding["payload"]["destroyed_at"].is_null() {
            continue;
        }
        let Some(run_id) = binding["payload"]["run_id"].as_str() else {
            continue;
        };
        let run = s.store.get("runs", run_id).map_err(db)?.ok_or_else(|| {
            Error::new(
                StatusCode::NOT_FOUND,
                "src.run_not_found",
                format!("bound Run {run_id} is missing"),
            )
        })?;
        if matches!(
            run["status"].as_str(),
            Some("draft" | "finished" | "solved" | "stopped")
        ) {
            continue;
        }
        let response = crate::runs::control_run(
            State(s.clone()),
            Path(run_id.to_owned()),
            Json(json!({"action":"stop","command_id":format!("{command_id}:stop:{run_id}")})),
        )
        .await;
        let status = response.status();
        let body = to_bytes(response.into_body(), usize::MAX)
            .await
            .map_err(db)?;
        let result: Value = serde_json::from_slice(&body).map_err(db)?;
        if !status.is_success() || result["ok"] != true {
            return Err(Error::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "run_stop_failed",
                result["detail"]
                    .as_str()
                    .unwrap_or("bound Run did not stop"),
            ));
        }
    }
    Ok(())
}

fn committed(s: &AppState, command_id: &str) -> Api<Option<Value>> {
    Ok(s.store
        .get(TASK_LIFECYCLE_APPLIED, command_id)
        .map_err(db)?
        .map(|marker| marker["output"].clone()))
}

fn save_lifecycle(
    s: &AppState,
    task_id: &str,
    state: &str,
    ts_field: &str,
    actor_id: &str,
    command_id: &str,
) -> Api<Value> {
    let record_key = format!("task_lifecycle:{task_id}");
    loop {
        if let Some(output) = committed(s, command_id)? {
            return Ok(output);
        }
        let current_task = task(s, task_id)?;
        if current_task["lifecycle"]["state"] == "destroyed" {
            return Err(Error::conflict("destroyed SRC Task cannot be restored"));
        }
        let current_record = s.store.get(RECORDS, &record_key).map_err(db)?;
        let revision = current_record
            .as_ref()
            .and_then(|row| row["version"].as_u64())
            .unwrap_or(0)
            + 1;
        let stamp = now();
        let mut payload = json!({"state":state,"actor_id":actor_id});
        payload[ts_field] = json!(stamp);
        let output = json!({
            "record_id":task_id,"kind":"task_lifecycle","task_id":task_id,
            "revision":revision,"parent_id":"","natural_key":"","generation":0,
            "created_at":current_record.as_ref().and_then(|row| row["values"]["created_at"].as_str()).unwrap_or(&stamp),
            "updated_at":stamp,"payload":payload
        });
        let record = json!({"version":revision,"values":output});
        let mut next_task = current_task.clone();
        next_task["lifecycle"] = payload;
        let marker = json!({"command_id":command_id,"output":output});
        if s.store
            .put_many_if(
                &[
                    (TASKS, task_id, Some(&current_task)),
                    (RECORDS, &record_key, current_record.as_ref()),
                    (TASK_LIFECYCLE_APPLIED, command_id, None),
                ],
                &[
                    (TASKS, task_id, &next_task),
                    (RECORDS, &record_key, &record),
                    (TASK_LIFECYCLE_APPLIED, command_id, &marker),
                ],
            )
            .map_err(db)?
        {
            return Ok(output);
        }
    }
}

pub(super) async fn apply(
    s: Arc<AppState>,
    task_id: &str,
    kind: &str,
    command_id: &str,
    actor_id: &str,
) -> Api<Value> {
    let _lock = lock_task(&s, task_id).await?;
    if let Some(output) = committed(&s, command_id)? {
        return Ok(output);
    }
    let row = task(&s, task_id)?;
    if row["lifecycle"]["state"] == "destroyed" {
        return Err(Error::conflict("destroyed SRC Task cannot be restored"));
    }
    let thread_id = row["binding"]["thread_id"]
        .as_str()
        .ok_or_else(|| Error::invalid("SRC Task Thread binding is missing"))?;
    if kind == "src.task.archive" {
        stop_bound_runs(&s, task_id, command_id).await?;
        s.agents.close(thread_id).await;
        thread_command(
            s.clone(),
            thread_id,
            &format!("{command_id}:archive"),
            "conversation.thread.archive",
            json!({"queue_pause_reason":"task_archived","close_running":true}),
        )
        .await?;
        crate::capabilities::archive_src_thread_binding(s.clone(), thread_id)
            .await
            .map_err(operation_error)?;
        s.capability_gateway.release(thread_id);
        for row in records(&s, task_id, "src_thread_delegation")? {
            if !row["payload"]["revoked_at"].is_null() {
                continue;
            }
            let child_id = row["payload"]["child_thread_id"].as_str().unwrap_or("");
            if !child_id.is_empty() {
                crate::runs::src_api::revoke_subthread_delegation_for_task(
                    &s,
                    Some(task_id),
                    child_id,
                    "task_archived",
                )?;
                s.capability_gateway.release(child_id);
            }
        }
        domains_monitor_state(&s, task_id, "stopped")?;
        let output = save_lifecycle(&s, task_id, "archived", "archived_at", actor_id, command_id)?;
        if let Err(error) = quiesce_streams(s.clone(), task_id, "task_closed").await {
            tracing::warn!(task_id,code=error.code,%error.message,"SRC streams could not close after Task archive");
        }
        Ok(output)
    } else if kind == "src.task.restore" {
        thread_command(
            s.clone(),
            thread_id,
            &format!("{command_id}:unarchive"),
            "conversation.thread.unarchive",
            json!({}),
        )
        .await?;
        crate::capabilities::restore_src_thread_binding(s.clone(), thread_id)
            .await
            .map_err(operation_error)?;
        domains_monitor_state(&s, task_id, "idle")?;
        save_lifecycle(&s, task_id, "active", "restored_at", actor_id, command_id)
    } else if kind == "src.task.pause" {
        if row["lifecycle"]["state"] == "archived" {
            return Err(Error::conflict("archived SRC Task must be restored before pausing"));
        }
        stop_bound_runs(&s, task_id, command_id).await?;
        let children = active_delegated_children(&s, task_id)?;
        let main_already_paused = s
            .store
            .get(crate::chat::COLLECTION, thread_id)
            .map_err(db)?
            .is_some_and(|doc| doc["thread"]["state"]["queue_paused"] == true);
        thread_command(
            s.clone(),
            thread_id,
            &format!("{command_id}:queue_pause"),
            "conversation.queue.pause",
            json!({"reason": "src_task_paused"}),
        )
        .await?;
        for (idx, child_id) in children.iter().enumerate() {
            let Some(child_doc) = s.store.get(crate::chat::COLLECTION, child_id).map_err(db)? else {
                continue;
            };
            if child_doc["thread"]["archived"] == true {
                continue;
            }
            let child_already_paused = child_doc["thread"]["state"]["queue_paused"] == true;
            if !child_already_paused {
                if let Err(err) = thread_command(
                    s.clone(),
                    child_id,
                    &format!("{command_id}:child_queue_pause:{idx}"),
                    "conversation.queue.pause",
                    json!({"reason": "src_task_paused"}),
                )
                .await
                {
                    tracing::warn!(task_id, child_id, code = err.code, %err.message, "Failed to pause delegated subthread queue during SRC Task pause");
                }
            }
            if interrupt_if_running(&s, child_id, command_id, &format!("child:{idx}")).await
                && !child_already_paused
                && s.store
                    .get(crate::chat::COLLECTION, child_id)
                    .ok()
                    .flatten()
                    .is_some_and(|d| d["thread"]["state"]["queue_pause_reason"] == "turn_interrupted")
            {
                let _ = thread_command(
                    s.clone(),
                    child_id,
                    &format!("{command_id}:child_queue_pause_after:{idx}"),
                    "conversation.queue.pause",
                    json!({"reason": "src_task_paused"}),
                )
                .await;
            }
        }
        if interrupt_if_running(&s, thread_id, command_id, "main").await
            && !main_already_paused
            && s.store
                .get(crate::chat::COLLECTION, thread_id)
                .ok()
                .flatten()
                .is_some_and(|d| d["thread"]["state"]["queue_pause_reason"] == "turn_interrupted")
        {
            let _ = thread_command(
                s.clone(),
                thread_id,
                &format!("{command_id}:queue_pause_after"),
                "conversation.queue.pause",
                json!({"reason": "src_task_paused"}),
            )
            .await;
        }
        domains_monitor_state(&s, task_id, "paused")?;
        save_lifecycle(&s, task_id, "paused", "paused_at", actor_id, command_id)
    } else if kind == "src.task.resume" {
        if row["lifecycle"]["state"] == "archived" {
            return Err(Error::conflict("archived SRC Task must be restored via src.task.restore"));
        }
        domains_monitor_state(&s, task_id, "idle")?;
        let output = save_lifecycle(&s, task_id, "active", "resumed_at", actor_id, command_id)?;
        thread_command(
            s.clone(),
            thread_id,
            &format!("{command_id}:queue_resume"),
            "conversation.queue.resume",
            json!({"only_if_reason": "src_task_paused"}),
        )
        .await?;
        for (idx, child_id) in active_delegated_children(&s, task_id)?.into_iter().enumerate() {
            let Some(child_doc) = s.store.get(crate::chat::COLLECTION, &child_id).map_err(db)? else {
                continue;
            };
            if child_doc["thread"]["archived"] == true {
                continue;
            }
            if let Err(err) = thread_command(
                s.clone(),
                &child_id,
                &format!("{command_id}:child_queue_resume:{idx}"),
                "conversation.queue.resume",
                json!({"only_if_reason": "src_task_paused"}),
            )
            .await
            {
                tracing::warn!(task_id, child_id, code = err.code, %err.message, "Failed to resume delegated subthread queue during SRC Task resume");
            }
        }
        if let Some(current) = s.store.get(super::super::POLICIES, task_id).map_err(db)?
            && current["values"]["activation"]["state"] == "deferred_paused"
        {
            let version = current["values"]["version"].as_u64().unwrap_or(0);
            let mut revised = current["values"].clone();
            revised["activation"] = match super::ensure_worker_gateway(&s, task_id).await {
                Ok(environment) => json!({
                    "state": "ready",
                    "generation": version,
                    "network_enforced": environment["network_enforced"],
                    "content_enforced": environment["content_enforced"],
                    "gateway_container": environment["gateway_container"]
                }),
                Err(error) => {
                    tracing::warn!(task_id, code = error.code, %error.message, "Deferred worker gateway activation failed on SRC Task resume");
                    json!({"state": "failed", "generation": version, "error": error.message})
                }
            };
            let next_doc = json!({"version": current["version"], "values": revised});
            let _ = s.store.put_many_if(&[(super::super::POLICIES, task_id, Some(&current))], &[(super::super::POLICIES, task_id, &next_doc)]);
        }
        Ok(output)
    } else {
        Err(Error::invalid("unsupported SRC Task lifecycle command"))
    }
}

fn active_delegated_children(s: &AppState, task_id: &str) -> Api<Vec<String>> {
    let mut children = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for row in records(s, task_id, "src_thread_delegation")? {
        if !row["payload"]["revoked_at"].is_null() {
            continue;
        }
        let child_id = row["payload"]["child_thread_id"].as_str().unwrap_or("").trim();
        if !child_id.is_empty() && seen.insert(child_id.to_owned()) {
            children.push(child_id.to_owned());
        }
    }
    Ok(children)
}

async fn interrupt_if_running(
    s: &Arc<AppState>,
    tid: &str,
    command_id: &str,
    suffix: &str,
) -> bool {
    let Ok(Some(doc)) = s.store.get(crate::chat::COLLECTION, tid) else {
        return false;
    };
    if doc["thread"]["archived"] == true {
        return false;
    }
    let state = &doc["thread"]["state"];
    let turn_id = state["running_turn_id"].as_str().unwrap_or("").trim().to_owned();
    if state["status"] != "running" && turn_id.is_empty() {
        return false;
    }
    let mut payload = json!({"internal_src_lifecycle": true});
    if !turn_id.is_empty() {
        payload["expected_turn_id"] = json!(turn_id);
    }
    match crate::chat::command(
        State(s.clone()),
        Path(tid.to_owned()),
        Json(json!({
            "command_id": format!("{command_id}:interrupt:{suffix}"),
            "command_type": "conversation.turn.interrupt",
            "payload": payload,
        })),
    )
    .await
    {
        Ok(_) => true,
        Err((_, Json(err_body)))
            if matches!(
                err_body["error"]["code"].as_str(),
                Some("conversation.turn.not_running" | "conversation.turn.expected_mismatch")
            ) =>
        {
            false
        }
        Err((status, Json(err_body))) => {
            tracing::warn!(
                tid,
                %status,
                error = %err_body,
                "Failed to interrupt running thread turn during SRC Task pause"
            );
            false
        }
    }
}

fn domains_monitor_state(s: &AppState, task_id: &str, state: &str) -> Api<()> {
    let key = format!("src-monitor-scheduler:{task_id}");
    let mut health = s
        .store
        .get(MONITOR_HEALTH, &key)
        .map_err(db)?
        .unwrap_or_else(|| json!({"task_id":task_id}));
    health["state"] = json!(state);
    if state == "stopped" {
        health["stopped_at"] = json!(now());
    } else if state == "paused" {
        health["paused_at"] = json!(now());
    } else {
        health["resumed_at"] = json!(now());
    }
    s.store.put(MONITOR_HEALTH, &key, &health).map_err(db)
}
