//! Reuse the live project, conversation, and Worker Profile handlers behind
//! the shared Command/Query API. Authorization and idempotency are enforced by
//! the enclosing operations route before a handler is called.
use super::*;
use std::collections::HashMap;

pub(super) fn command_supported(kind: &str) -> bool {
    thread_command(kind)
        || crate::runtime_api::supports_command(kind)
        || super::workbench_dispatch::command_supported(kind)
        || matches!(
            kind,
            "conversation.project.create"
                | "conversation.project.update"
                | "conversation.workspace.bind"
                | "conversation.workspace.delete_worktree"
                | "conversation.thread.create"
                | "worker_profile.upsert"
                | "worker_profile.remove"
                | "worker_settings.update" | "worker_identity.update"
                | "credential_account.import_file" | "credential_account.import_host"
        )
}

fn thread_command(kind: &str) -> bool {
    matches!(
        kind,
        "conversation.thread.rename"
            | "conversation.thread.archive"
            | "conversation.thread.unarchive"
            | "conversation.thread.resume"
            | "conversation.thread.quota_resume"
            | "conversation.thread.fork"
            | "conversation.turn.send"
            | "conversation.turn.retry"
            | "conversation.turn.edit_resend"
            | "conversation.turn.native_rewind"
            | "conversation.turn.steer"
            | "conversation.turn.interrupt"
            | "conversation.turn.resume"
            | "conversation.subagent.spawn"
            | "conversation.subagent.send"
            | "conversation.subagent.cancel"
            | "conversation.queue.update"
            | "conversation.queue.delete"
            | "conversation.queue.reorder"
            | "conversation.queue.pause"
            | "conversation.queue.resume"
            | "conversation.queue.steer"
            | "conversation.artifact.attach"
            | "conversation.memory.record"
            | "conversation.memory.delete"
            | "conversation.plan.amend"
            | "conversation.plan.inject"
            | "conversation.agents.inject"
            | "conversation.approval.resolve"
            | "conversation.approval.inject"
            | "conversation.user_input.resolve"
            | "conversation.user_input.inject"
    )
}

pub(super) fn query_supported(kind: &str) -> bool {
    matches!(
        kind,
        "run.list"
            | "run.snapshot"
            | "project.list"
            | "conversation.project.directory_list"
            | "thread.list"
            | "conversation.thread.list"
            | "conversation.thread.view"
            | "conversation.thread.metadata"
            | "conversation.thread.messages"
            | "conversation.turn.process"
            | "conversation.thread.search"
            | "conversation.memory.list"
            | "conversation.subagent.status"
            | "conversation.subagent.runtimes"
            | "runtime.instance.list"
            | "runtime.descriptor.list"
            | "worker_profile.list"
            | "worker_settings.get"
            | "credential_account.list" | "graph.shared.read"
    ) || super::workbench_dispatch::query_supported(kind)
}

fn named<'a>(value: &'a Value, fields: &[&str]) -> &'a str {
    fields
        .iter()
        .find_map(|field| value[*field].as_str().filter(|v| !v.trim().is_empty()))
        .unwrap_or("")
        .trim()
}

pub(super) fn command_scope(
    kind: &str,
    envelope: &Value,
) -> Result<(&'static str, String), Failure> {
    if super::workbench_dispatch::command_supported(kind) {
        return super::workbench_dispatch::command_scope(kind, envelope);
    }
    let payload = &envelope["payload"];
    let aggregate = envelope["aggregate_id"].as_str().unwrap_or("").trim();
    let (scope, payload_id, required) = match kind {
        "conversation.project.create" => ("project", named(payload, &["project_id"]), false),
        "conversation.project.update" => ("project", named(payload, &["project_id"]), true),
        "conversation.workspace.bind" => ("workspace", named(payload, &["workspace_id"]), false),
        "conversation.workspace.delete_worktree" => ("workspace", named(payload, &["workspace_id"]), true),
        "conversation.thread.create" => ("thread", named(payload, &["thread_id"]), false),
        kind if thread_command(kind) => ("thread", named(payload, &["thread_id"]), true),
        "worker_profile.upsert" => ("profile", named(payload, &["name", "id"]), true),
        "worker_profile.remove" => ("profile", named(payload, &["name"]), true),
        "worker_settings.update" | "worker_identity.update" => ("worker_settings","global",true),
        "credential_account.import_file" | "credential_account.import_host" => ("credential",named(payload,&["account_id"]),true),
        "runtime.instance.refresh" => (
            "runtime",
            named(payload, &["adapter_id", "transport_kind"]),
            false,
        ),
        kind if crate::runtime_api::supports_command(kind) => ("runtime", "", true),
        _ => {
            return Err(fail(
                StatusCode::NOT_IMPLEMENTED,
                "capability.unavailable",
                "Unknown Agent command",
            ));
        }
    };
    let derived_runtime = if scope == "runtime"
        && kind != "runtime.instance.refresh"
        && !named(payload, &["adapter_id"]).is_empty()
    {
        Some(format!(
            "{}:{}",
            named(payload, &["adapter_id"]),
            if named(payload, &["instance_id"]).is_empty() {
                "default"
            } else {
                named(payload, &["instance_id"])
            }
        ))
    } else {
        None
    };
    let id = derived_runtime.as_deref().unwrap_or_else(|| {
        if payload_id.is_empty() {
            aggregate
        } else {
            payload_id
        }
    });
    let id = if kind == "runtime.instance.refresh" && id.is_empty() {
        "all"
    } else {
        id
    };
    if required && id.is_empty() {
        let (code, msg) = match kind {
            "conversation.workspace.delete_worktree" => (
                "conversation.workspace.id_required",
                "delete_worktree requires workspace_id",
            ),
            "conversation.project.update" => (
                "conversation.project.id_required",
                "project.update requires payload.project_id",
            ),
            _ => ("platform.request.invalid", "Command target id is required"),
        };
        return Err(fail(StatusCode::BAD_REQUEST, code, msg));
    }
    Ok((scope, id.to_owned()))
}

pub(super) fn query_scope(kind: &str, envelope: &Value) -> Result<(&'static str, String), Failure> {
    if super::workbench_dispatch::query_supported(kind) {
        return super::workbench_dispatch::query_scope(kind, envelope);
    }
    let aggregate = envelope["aggregate_id"].as_str().unwrap_or("").trim();
    let params = &envelope["params"];
    let (scope, target, required) = match kind {
        "run.list" => ("run", "", false),
        "run.snapshot" | "graph.shared.read" => ("run", named(params, &["run_id"]), true),
        "project.list" | "conversation.project.directory_list" => ("project", "", false),
        "thread.list" | "conversation.thread.list" | "conversation.thread.search" => ("thread", "", false),
        "conversation.subagent.status" | "conversation.subagent.runtimes" =>
            ("thread", named(params, &["thread_id"]), true),
        "conversation.thread.view"
        | "conversation.thread.metadata"
        | "conversation.thread.messages"
        | "conversation.turn.process"
        | "conversation.memory.list" => ("thread", named(params, &["thread_id"]), true),
        "runtime.instance.list" | "runtime.descriptor.list" => ("runtime", "", false),
        "worker_profile.list" => ("profile", "", false),
        "worker_settings.get" => ("worker_settings", "", false),
        "credential_account.list" => ("credential","",false),
        _ => {
            return Err(fail(
                StatusCode::NOT_IMPLEMENTED,
                "capability.unavailable",
                "Unknown Agent query",
            ));
        }
    };
    let id = if !target.is_empty() {
        target
    } else if required {
        aggregate
    } else {
        ""
    };
    if required && id.is_empty() {
        return Err(fail(
            StatusCode::BAD_REQUEST,
            if kind == "run.snapshot" { "run.id_required" } else { "platform.request.invalid" },
            if kind == "run.snapshot" { "run.snapshot requires params.run_id" } else { "Query target id is required" },
        ));
    }
    Ok((scope, id.to_owned()))
}

pub(super) async fn command(state: Arc<AppState>, envelope: &Value) -> Result<Response, Failure> {
    let kind = envelope["command_type"].as_str().unwrap_or("");
    if super::settings_dispatch::supports(kind) { return super::settings_dispatch::command(state,envelope).await; }
    if super::workbench_dispatch::command_supported(kind) {
        return super::workbench_dispatch::command(state, envelope).await;
    }
    let (_, target) = command_scope(kind, envelope)?;
    let mut payload = envelope["payload"].clone();
    if !payload.is_object() {
        return Err(fail(
            StatusCode::BAD_REQUEST,
            "platform.request.invalid",
            "payload must be an object",
        ));
    }
    payload["command_id"] = envelope["command_id"].clone();
    if let Some(key) = envelope["idempotency_key"].as_str() {
        payload["idempotency_key"] = json!(key);
    }
    if envelope["actor"].is_object() {
        payload["actor"] = envelope["actor"].clone();
    }
    payload["_platform_envelope"] = envelope.clone();
    let result = match kind {
        "conversation.project.create" => {
            if !target.is_empty() { payload["project_id"] = json!(target); }
            crate::workspaces::create_project(State(state),Json(payload)).await.map(|Json(body)| Json(body).into_response())
        }
        "conversation.project.update" => {
            crate::workspaces::update_project(State(state),Path(target),Json(payload)).await.map(|Json(body)| Json(body).into_response())
        }
        "conversation.workspace.bind" => {
            if !target.is_empty() { payload["workspace_id"] = json!(target); }
            crate::workspaces::bind_workspace(State(state),Json(payload)).await.map(|Json(body)| Json(body).into_response())
        }
        "conversation.workspace.delete_worktree" => {
            if !target.is_empty() { payload["workspace_id"] = json!(target); }
            crate::workspaces::delete_worktree(State(state),Json(payload)).await.map(|Json(body)| Json(body).into_response())
        }
        "conversation.thread.create" => {
            if !target.is_empty() { payload["thread_id"] = json!(target); }
            crate::chat::create(State(state),Json(payload)).await.map(|Json(body)| Json(body).into_response())
        }
        kind if thread_command(kind) => {
            crate::chat::command_from_platform(state, target, json!({
                "command_id":envelope["command_id"],"command_type":kind,"payload":envelope["payload"],
                "actor":envelope["actor"],"idempotency_key":envelope["idempotency_key"]
            }), envelope["expected_version"].as_u64()).await.map(|Json(body)| Json(body).into_response())
        }
        "worker_profile.upsert" => {
            if named(&payload,&["name","id"]).is_empty() { payload["name"] = json!(target); }
            return Ok(crate::runtime_api::upsert_profile(State(state),Some(Json(payload))).await);
        }
        "worker_profile.remove" => {
            let mut query=HashMap::new();
            query.insert("command_id".to_owned(),envelope["command_id"].as_str().unwrap_or("").to_owned());
            if let Some(key)=envelope["idempotency_key"].as_str() { query.insert("idempotency_key".to_owned(),key.to_owned()); }
            if let Some(rev)=envelope["payload"]["expected_revision"].as_str() { query.insert("expected_revision".to_owned(),rev.to_owned()); }
            return Ok(crate::runtime_api::remove_profile(State(state),Path(target),Query(query)).await);
        }
        kind if crate::runtime_api::supports_command(kind) => {
            return Ok(crate::runtime_api::dispatch_command_envelope(state,envelope).await);
        }
        _ => return Err(fail(StatusCode::NOT_IMPLEMENTED,"capability.unavailable","Unknown Agent command")),
    };
    Ok(result.unwrap_or_else(|failure| failure.into_response()))
}

pub(super) async fn query(state: &Arc<AppState>, envelope: &Value, principal: &str) -> Result<Value, Failure> {
    let kind = envelope["query_type"].as_str().unwrap_or("");
    if super::workbench_dispatch::query_supported(kind) {
        return super::workbench_dispatch::query(state, envelope).await;
    }
    let (_, target) = query_scope(kind, envelope)?;
    let mut params = envelope["params"].clone();
    if !params.is_object() {
        return Err(fail(
            StatusCode::BAD_REQUEST,
            "platform.request.invalid",
            "params must be an object",
        ));
    }
    if !target.is_empty() && kind.starts_with("conversation.") {
        params["thread_id"] = json!(target);
    }
    if crate::chat_subagents::supports_query(kind) {
        return crate::chat_subagents::query(state,&target,kind,&params).await;
    }
    if kind == "run.list" {
        return super::run_queries::list(state, &params, principal, &envelope["actor"]);
    }
    if kind == "run.snapshot" {
        return super::run_queries::snapshot(state, &target);
    }
    if kind == "graph.shared.read" { return super::run_queries::shared_graph(state,&target,&params); }
    if kind == "credential_account.list" {
        let Json(mut value) = crate::compat::credential_accounts(State(state.clone())).await?;
        value["count"] = json!(value["accounts"].as_array().map_or(0,Vec::len));
        return Ok(value);
    }
    let values: HashMap<String, String> = params
        .as_object()
        .into_iter()
        .flatten()
        .map(|(key, value)| {
            (
                key.clone(),
                value
                    .as_str()
                    .map(str::to_owned)
                    .unwrap_or_else(|| value.to_string()),
            )
        })
        .collect();
    let direct = match kind {
        "project.list" | "conversation.project.directory_list" => {
            Some(crate::workspaces::list_projects(State(state.clone()), Query(Default::default())).await)
        }
        "conversation.thread.list" => {
            Some(crate::chat::list(State(state.clone()), Query(values.clone())).await)
        }
        "conversation.turn.process" => {
            return crate::chat::turn_process(state, &target, &params);
        }
        "conversation.memory.list" => {
            return crate::memory_api::query_list(state, &target, &params);
        }
        "conversation.thread.view" | "conversation.thread.metadata" => {
            let mut query = values.clone();
            if kind == "conversation.thread.metadata" {
                query.insert("mark_read".to_owned(), "false".to_owned());
            } else {
                query
                    .entry("mark_read".to_owned())
                    .or_insert_with(|| "false".to_owned());
            }
            let metadata_target = target.clone();
            let result =
                crate::chat::get_thread(State(state.clone()), Path(target), Query(query)).await;
            if kind == "conversation.thread.metadata" {
                return result.and_then(|Json(view)| {
                    let thread = view["thread"].clone();
                    let status = &view["state"];
                    let raw_doc = state
                        .store
                        .get(crate::chat::COLLECTION, &metadata_target)
                        .map_err(storage)?;
                    let message_count = raw_doc
                        .as_ref()
                        .map(|doc| {
                            crate::conversation_view::snapshot(doc)["messages"]
                                .as_array()
                                .map_or(0, Vec::len)
                        })
                        .unwrap_or(0);
                    let queue_count = raw_doc
                        .as_ref()
                        .and_then(|doc| doc["queue"].as_array())
                        .map(|q| {
                            q.iter()
                                .filter(|row| {
                                    matches!(
                                        row["status"].as_str().unwrap_or("queued"),
                                        "queued" | "scheduled"
                                    )
                                })
                                .count()
                        })
                        .unwrap_or(0);
                    let ctrl = state
                        .store
                        .get("control_threads", &metadata_target)
                        .ok()
                        .flatten()
                        .unwrap_or(Value::Null);
                    Ok(json!({
                        "thread": thread,
                        "state": {
                            "status": status["status"],
                            "running_turn_id": status["running_turn_id"],
                            "unread": status["unread"],
                            "queue_paused": status["queue_paused"],
                            "message_count": message_count,
                            "queue_count": queue_count,
                            "pending_approvals": status["pending_approvals"],
                            "pending_user_input": status["pending_user_input"],
                            "lineage": status["lineage"],
                            "control_lifecycle": status["control_lifecycle"],
                        },
                        "runtime": view["runtime"],
                        "review": ctrl["review"],
                        "references": ctrl["references"].as_array().cloned().unwrap_or_default(),
                        "self_settle": ctrl["self_settle"],
                        "messages_query": "muteki_read_thread_messages",
                        "outline_query": "muteki_outline_thread",
                    }))
                });
            }
            Some(result)
        }
        "conversation.thread.messages" => {
            Some(crate::chat::messages(State(state.clone()), Path(target), Query(values.clone())).await)
        }
        "runtime.instance.list" => {
            return crate::runtime_api::list(State(state.clone()), Query(values))
                .await
                .map(|Json(v)| v);
        }
        "runtime.descriptor.list" => {
            return crate::runtime_api::descriptors_view(State(state.clone()), Query(values))
                .await
                .map(|Json(v)| v);
        }
        "worker_profile.list" => {
            return crate::runtime_api::profiles(State(state.clone()))
                .await
                .map(|Json(v)| v);
        }
        "worker_settings.get" => {
            return crate::settings_metadata::configuration(state)
                .map(|config| json!({"config":config}))
                .map_err(|(_, Json(body))| {
                    fail(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "storage.unavailable",
                        body["detail"]
                            .as_str()
                            .unwrap_or("Worker settings unavailable"),
                    )
                });
        }
        _ => None,
    };
    if let Some(result) = direct {
        return result.map(|Json(v)| v);
    }
    match kind {
        "thread.list" => list_threads(state,&params),
        "conversation.thread.search" => crate::chat_history_api::search(State(state.clone()),Query(values)).await.map(|Json(value)|value),
        _ => Err(fail(StatusCode::NOT_IMPLEMENTED,"capability.unavailable","Unknown Agent query")),
    }
}

fn list_threads(s: &AppState, arguments: &Value) -> Result<Value, Failure> {
    let limit = arguments["limit"].as_u64().unwrap_or(50).clamp(1, 200) as usize;
    let project = arguments["project_id"].as_str().unwrap_or("").trim().to_owned();
    let mode_filter = arguments["mode"].as_str().unwrap_or("").trim().to_owned();
    let status_filter = arguments["status"].as_str().unwrap_or("").trim().to_owned();
    let archived_filter = arguments["archived"].as_bool();
    let settled_filter = arguments["settled"].as_bool();
    let pinned_filter = arguments["pinned"].as_bool();
    let follow_up_filter = arguments["follow_up"].as_bool();
    let subagent_filter = arguments["subagent"].as_bool();
    let needle = arguments["query"].as_str().unwrap_or("").trim().to_lowercase();
    let cursor = arguments["cursor"].as_str().unwrap_or("").trim().to_owned();
    let mut rows: Vec<Value> = s
        .store
        .conversation_summaries()
        .map_err(storage)?
        .into_iter()
        .map(|row| row["thread"].clone())
        .filter(|thread| {
            let is_subagent = thread["state"]["lineage"]["origin"] == "app_owned";
            let lifecycle = &thread["state"]["control_lifecycle"];
            !crate::chat::is_side_chat(thread)
                && !crate::chat::is_ctf_thread(thread)
                && (project.is_empty() || thread["project_id"] == project.as_str())
                && (mode_filter.is_empty() || thread["mode"] == mode_filter.as_str())
                && (status_filter.is_empty() || thread["state"]["status"] == status_filter.as_str())
                && archived_filter.is_none_or(|want| (thread["archived"] == true) == want)
                && settled_filter.is_none_or(|want| (lifecycle["settled"] == true) == want)
                && pinned_filter.is_none_or(|want| (lifecycle["pinned"] == true) == want)
                && follow_up_filter.is_none_or(|want| (lifecycle["follow_up"] == true) == want)
                && subagent_filter.is_none_or(|want| is_subagent == want)
                && (needle.is_empty()
                    || thread["title"].as_str().unwrap_or("").to_lowercase().contains(&needle)
                    || thread["thread_id"].as_str().unwrap_or("").to_lowercase().contains(&needle))
        })
        .collect();
    rows.sort_by(|a, b| {
        (b["state"]["control_lifecycle"]["pinned"] == true)
            .cmp(&(a["state"]["control_lifecycle"]["pinned"] == true))
            .then_with(|| b["updated_at"].as_str().cmp(&a["updated_at"].as_str()))
            .then_with(|| b["thread_id"].as_str().cmp(&a["thread_id"].as_str()))
    });
    let start = match cursor.is_empty() {
        true => 0,
        false => match rows.iter().position(|thread| thread["thread_id"] == cursor.as_str()) {
            Some(index) => index + 1,
            None => {
                return Err(fail(StatusCode::BAD_REQUEST,"capability.cursor_invalid","游标指向的会话已不存在，请重新从第一页读取。"));
            }
        },
    };
    let page: Vec<Value> = rows.iter().skip(start).take(limit).cloned().collect();
    let next = (start + page.len() < rows.len()).then(|| page.last().map(|thread| thread["thread_id"].clone())).flatten();
    Ok(json!({"count": page.len(), "threads": page, "total": rows.len(), "next_cursor": next}))
}
