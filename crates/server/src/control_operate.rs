//! Operational `muteki-control` tools: subagent structured result reporting,
//! cross-thread & scheduled messaging (`queue` / `steer` / `schedule`),
//! standalone or subagent thread creation, unified `control_thread`,
//! `respond_input`, `respond_approval`, `rewind_thread`, `move_thread`,
//! `git`, `pull_request`, and `terminal`.

use crate::{
    AppState, chat, checkpoints, control, git_api, github_pr, workspace_read_api::bound_root,
};
use serde_json::{Value, json};
use std::sync::Arc;
use tcode_control_core::control::action;

pub const SCHEDULED_MESSAGES: &str = "control_scheduled_messages";

pub const SUBAGENT_REPORT_FOOTER: &str = "\n\n[muteki-control subagent protocol]\nThe parent cannot see your transcript, turns, waits or background work. When finished, call `muteki_report_result` once with `text` containing your complete final report, then end the turn. Include reasoning, recommendations, disagreements and open questions for discussion; changed files, commands actually run and outcomes for execution. Only if the tool call fails, put the same complete report in your final reply.";

fn cmd_error((_, axum::Json(body)): chat::Failure) -> Value {
    body["error"].clone()
}

/// The grant/scope error as a one-line batch skip reason.
fn skip_reason(error: &Value) -> String {
    error["message"]
        .as_str()
        .map(str::to_owned)
        .unwrap_or_else(|| error.to_string())
}

/// Why a target refuses a cross-thread write, worded as the upstream batch skip reasons.
fn write_target_refusal(s: &AppState, doc: &Value) -> Option<String> {
    if doc["thread"]["archived"] == true {
        return Some("archived: unarchive the thread first".to_owned());
    }
    if control::is_read_only_subagent(s, doc["thread"]["thread_id"].as_str().unwrap_or("")) {
        return Some("read-only: a native subagent's mirror takes no writes".to_owned());
    }
    None
}

/// `muteki_report_result` (`Requirement::Subagent`): record structured child verdict.
pub async fn report_result(s: &Arc<AppState>, caller: &str, args: &Value) -> Result<Value, Value> {
    crate::control_subagents::report(s, caller, args).await
}

fn started_thread(s: &AppState, id: &str, created: bool) -> Result<Value, Value> {
    let doc = chat::load(s, id).map_err(cmd_error)?;
    let line = &doc["thread"]["state"]["lineage"];
    let subagent = line["origin"] == "app_owned";
    let cwd = crate::native_chat::session_cwd(s, &doc, id)
        .map_err(|error| json!({"code":error.code,"message":error.message}))?;
    let workspace = s
        .store
        .get(
            "workspaces",
            doc["thread"]["workspace_id"].as_str().unwrap_or(""),
        )
        .map_err(|error| json!({"code":"storage.unavailable","message":error.to_string()}))?;
    let branch = if line["isolation"] == "worktree" {
        workspace
            .as_ref()
            .map(|workspace| workspace["settings"]["branch"].clone())
            .unwrap_or(Value::Null)
    } else {
        Value::Null
    };
    serde_json::to_value(crate::control_contract::StartThreadResponse {
        thread_id: id.to_owned(), created,
        title: doc["thread"]["title"].as_str().unwrap_or("").to_owned(),
        cwd: cwd.to_string_lossy().into_owned(), subagent,
        worktree_branch: branch.as_str().map(str::to_owned),
    }).map_err(|error|json!({"code":"control.response_invalid","message":error.to_string()}))
}

/// Return a previously created thread for `(caller, request_id)` so retries recover existing
/// threads even if mutable orchestration roles were later changed, disabled, or removed.
pub fn find_existing_started_thread(
    s: &AppState,
    caller: &str,
    request_id: &str,
) -> Result<Option<Value>, Value> {
    for thread in s
        .store
        .list(chat::COLLECTION)
        .map_err(|error| json!({"code":"storage.unavailable","message":error.to_string()}))?
    {
        if thread["thread"]["started_by"]["thread_id"] == caller
            && thread["thread"]["started_by"]["request_id"] == request_id
        {
            return started_thread(s, thread["thread"]["thread_id"].as_str().unwrap(), false)
                .map(Some);
        }
    }
    let old_parent = chat::load(s, caller).map_err(cmd_error)?;
    if let Some(child) = old_parent["thread"]["state"]["subagents"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|child| child["spawn_command_id"] == request_id)
        .and_then(|child| child["thread_id"].as_str())
    {
        return started_thread(s, child, false).map(Some);
    }
    Ok(None)
}

/// `muteki_start_thread`: start either a bound subagent (`subagent: true`) or a standalone
/// conversation thread (`subagent: false`), honoring `subagent_policy`.
pub async fn start_thread(s: &Arc<AppState>, caller: &str, args: &Value) -> Result<Value, Value> {
    let mut normalized = args.clone();
    if let Some(value) = args.get("message") {
        normalized["prompt"] = value.clone();
    }
    if let Some(value) = args.get("request_id") {
        normalized["idempotency_key"] = value.clone();
    }
    if let Some(value) = args.get("provider") {
        normalized["engine"] = value.clone();
    }
    if let Some(value) = args.get("profile") {
        normalized["credential_id"] = value.clone();
    }
    if let Some(workspace) = args["workspace"].as_str() {
        normalized["isolation"] = json!(if workspace == "local" {
            "shared"
        } else {
            workspace
        });
    }
    if let Some(effort) = args["options"]["reasoningEffort"].as_str() {
        normalized["effort"] = json!(effort);
    }
    if let Some(tier) = args["options"]["serviceTier"].as_str() {
        normalized["service_tier"] = json!(tier);
    }
    let args = &normalized;
    let request_id = args["request_id"].as_str().filter(|id|!id.trim().is_empty()).ok_or_else(||json!({"code":"control.request_id_required","message":"request_id must not be empty"}))?;
    if let Some(existing) = find_existing_started_thread(s, caller, request_id)? {
        return Ok(existing);
    }
    let has_attachments=args["attachments"].as_array().is_some_and(|attachments|!attachments.is_empty());
    if args["prompt"].as_str().is_none_or(|text|text.trim().is_empty()) && !has_attachments {return Err(json!({"code":"control.message_empty","message":"message is empty"}));}
    let caller_doc = chat::load(s, caller).map_err(cmd_error)?;
    let caller_is_src = caller_doc["thread"]["mode"] == "src";
    let has_src_delegation_args = ["delegation_profile", "todo_id", "allowed_refs", "allowed_tools"]
        .iter()
        .any(|k| args.get(*k).is_some_and(|v| !v.is_null()));
    let subagent = args["subagent"]
        .as_bool()
        .unwrap_or(caller_is_src || has_src_delegation_args);
    if !subagent && args["workspace"] == "worktree" {
        return Err(json!({"code":"control.workspace_invalid","message":"workspace worktree is only for a subagent"}));
    }
    let target_project = args["project_id"]
        .as_str()
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .or_else(|| caller_doc["thread"]["project_id"].as_str())
        .unwrap_or("")
        .to_owned();
    if target_project.is_empty() && !(subagent && caller_is_src) {
        return Err(json!({"code":"control.project_required","message":"project_id is required: this thread has no project"}));
    }
    let cfg = control::settings(s).map_err(cmd_error)?;
    let caller_mode = control::thread_orchestration_mode(s, caller);
    let resolved_orchestration = cfg.orchestration_for_mode(caller_mode);
    if let Some(role_id) = normalized["role"].as_str().map(str::trim).filter(|id| !id.is_empty()).map(str::to_owned) {
        let role = resolved_orchestration.profile.roles.iter().find(|role| role.enabled && role.id.eq_ignore_ascii_case(&role_id))
            .ok_or_else(|| json!({"code":"control.role_unknown","category":"validation","message":format!("unknown or disabled orchestration role: {role_id}")}))?;
        let canonical_engine = |value: &str| -> String {
            crate::agent_runtime::descriptors::find(value).map(|descriptor| descriptor.engine().to_owned())
                .or_else(|| crate::agent_runtime::descriptors::for_adapter(value).map(|(descriptor, _)| descriptor.engine().to_owned()))
                .unwrap_or_else(|| value.trim().to_ascii_lowercase())
        };
        if role.mode == "pinned" {
            for (field, configured) in [("engine", &role.provider), ("credential_id", &role.profile), ("model", &role.model), ("effort", &role.effort)] {
                if configured.is_empty() { continue; }
                if let Some(requested) = normalized[field].as_str().filter(|value| !value.trim().is_empty()) {
                    let matches = if field == "engine" { canonical_engine(requested) == canonical_engine(configured) } else { requested.trim() == configured.as_str() };
                    if !matches {
                        return Err(json!({"code":"control.role_routing_conflict","category":"validation",
                            "message":format!("role {} pins {field}={configured}; requested {requested}. Use the configured role routing, or omit role for a distinct explicit task.", role.id),
                            "role":role.id,"field":field,"configured":configured,"requested":requested}));
                    }
                }
                normalized[field] = json!(configured);
            }
            normalized["provider"] = normalized["engine"].clone();
            if !role.profile.is_empty() { normalized["profile"] = json!(role.profile); }
        }
        if normalized["effort"].as_str().is_none_or(str::is_empty) && !role.effort.is_empty() {
            normalized["effort"] = json!(role.effort);
        }
        if let Some(effort) = normalized["effort"].as_str().filter(|value| !value.is_empty()).map(str::to_owned) {
            if !normalized["options"].is_object() { normalized["options"] = json!({}); }
            normalized["options"]["reasoningEffort"] = json!(effort);
        }
        let prompt = normalized["prompt"].as_str().unwrap_or("");
        normalized["prompt"] = json!(format!("{prompt}\n\n[Muteki configured role: {} ({})]\n{}\n{}", role.id, role.label, role.description, role.guidelines));
        normalized["role"] = json!(role.id);
        let intent_source = normalized["routing_intent"]["source"]
            .as_str()
            .filter(|v| !v.is_empty())
            .unwrap_or("orchestration_role");
        normalized["routing"] = json!({
            "source": intent_source,
            "workbench_mode": resolved_orchestration.mode,
            "orchestration_scope": resolved_orchestration.source,
            "role": role.id,
            "mode": role.mode,
            "provider": normalized["engine"],
            "profile": normalized["credential_id"],
            "model": normalized["model"],
            "effort": normalized["effort"],
            "guidelines": role.guidelines,
            "requested_fields": normalized["routing_intent"]["requested_fields"],
            "inherited_fields": normalized["routing_intent"]["inherited_fields"],
        });
    } else {
        let intent_source = normalized["routing_intent"]["source"]
            .as_str()
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| {
                let has_explicit = ["engine", "credential_id", "model", "effort"]
                    .iter()
                    .any(|k| normalized[k].as_str().is_some_and(|v| !v.trim().is_empty()));
                if has_explicit { "explicit_request" } else { "inherited_parent" }
            });
        normalized["routing"] = json!({
            "source": intent_source,
            "workbench_mode": resolved_orchestration.mode,
            "orchestration_scope": resolved_orchestration.source,
            "provider": normalized["engine"],
            "profile": normalized["credential_id"],
            "model": normalized["model"],
            "effort": normalized["effort"],
            "requested_fields": normalized["routing_intent"]["requested_fields"],
            "inherited_fields": normalized["routing_intent"]["inherited_fields"],
        });
    }
    let args = &normalized;

    if !subagent {
        let creation_guard = s.chat.active.lock().await;
        let caller_doc = chat::load(s, caller).map_err(cmd_error)?;
        let project_id = args["project_id"]
            .as_str()
            .map(str::trim)
            .filter(|p| !p.is_empty())
            .or_else(|| caller_doc["thread"]["project_id"].as_str())
            .unwrap_or("")
            .to_owned();
        let engine = args["engine"].as_str().unwrap_or("");
        let descriptor = crate::agent_runtime::descriptors::find(engine).ok_or_else(|| json!({"code":"control.provider_unknown","message":format!("unknown provider {engine}")}))?;
        let preferences = crate::compat::get_ui(axum::extract::State(s.clone())).await.map_err(cmd_error)?.0;
        let mode = args["access_mode"].as_str()
            .or_else(|| preferences["values"]["defaultAccessMode"].as_str().filter(|mode| !mode.is_empty()))
            .unwrap_or("supervised");
        let mut runtime = json!({
            "adapter_id":descriptor.identity.default_adapter_id,
            "instance_id":"default", "credential_id": args["credential_id"].as_str().map(str::to_owned).unwrap_or_else(||format!("system:{engine}")),
            "model":args["model"].as_str().unwrap_or(""), "effort":args["effort"].as_str().unwrap_or(""),
            "access_mode":mode, "options":args["options"].as_object().cloned().unwrap_or_default(),
        });
        let workspace = s
            .store
            .list("workspaces")
            .map_err(|e| json!({"code":"storage.unavailable","message":e.to_string()}))?
            .into_iter()
            .find(|workspace| {
                workspace["project_id"] == project_id
                    && workspace["settings"]["mode"]
                        .as_str()
                        .is_none_or(str::is_empty)
            });
        let workspace_id = workspace
            .as_ref()
            .map(|workspace| workspace["workspace_id"].clone())
            .unwrap_or(Value::Null);
        if let Some(cwd) = args["cwd"].as_str() {
            let root = workspace.as_ref().and_then(|workspace|workspace["root_path"].as_str()).ok_or_else(||json!({"code":"control.cwd_unavailable","message":"This project has no workspace"}))?;
            let root = std::path::Path::new(root)
                .canonicalize()
                .map_err(|e| json!({"code":"control.cwd_invalid","message":e.to_string()}))?;
            let relative = std::path::Path::new(cwd);
            if relative.is_absolute()
                || relative
                    .components()
                    .any(|part| matches!(part, std::path::Component::ParentDir))
            {
                return Err(
                    json!({"code":"control.cwd_invalid","message":"cwd must be relative to the project root"}),
                );
            }
            let selected = root
                .join(relative)
                .canonicalize()
                .map_err(|e| json!({"code":"control.cwd_invalid","message":e.to_string()}))?;
            if !selected.starts_with(&root) || !selected.is_dir() {
                return Err(
                    json!({"code":"control.cwd_invalid","message":"cwd must remain in the project"}),
                );
            }
            runtime["cwd"] = json!(selected);
        }
        chat::resolve_runtime(s, &runtime).map_err(cmd_error)?;

        let request_id = args["idempotency_key"].as_str().ok_or_else(
            || json!({"code":"control.request_id_required","message":"request_id is required"}),
        )?;
        for existing in s
            .store
            .list(chat::COLLECTION)
            .map_err(|e| json!({"code":"storage.unavailable","message":e.to_string()}))?
        {
            let started = &existing["thread"]["started_by"];
            if started["thread_id"] == caller && started["request_id"] == request_id {
                return started_thread(
                    s,
                    existing["thread"]["thread_id"].as_str().unwrap_or(""),
                    false,
                );
            }
        }
        let new_tid = uuid::Uuid::new_v4().to_string();
        let title = args["title"].as_str().map(str::trim).unwrap_or("");
        let create_body = json!({
            "command_id": format!("ctrl-create:{new_tid}"),
            "thread_id": new_tid,
            "project_id": project_id,
            "workspace_id": workspace_id,
            "title": title,
            "title_source": if title.is_empty() { "fallback" } else { "user" },
            "runtime": runtime,
        });
        let (_, mut created, _) =
            chat::prepare_created_thread(s, &create_body, "conversation").map_err(cmd_error)?;
        let mut attachments = Vec::new();
        for reference in args["attachments"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
        {
            let (meta, _) = crate::files::read_control_attachment(s, caller, reference).map_err(
                |message| json!({"code":"control.attachment_invalid","message":message}),
            )?;
            attachments.push(meta);
        }
        created["artifacts"] = json!(attachments);
        created["thread"]["started_by"] = json!({"thread_id":caller,"request_id":request_id});
        created["thread"]["routing"] = args["routing"].clone();
        created["thread"]["routing"]["resolved"] = json!({
            "adapter_id":created["runtime"]["adapter_id"],"instance_id":created["runtime"]["instance_id"],
            "profile":created["runtime"]["credential_id"],"model":created["runtime"]["model"],
            "effort":created["runtime"]["effort"]});
        created["thread"]["control_ancestor_restrictions"] = json!(control::ancestor_restrictions(s, caller)?);
        chat::save(s, &new_tid, &mut created).map_err(cmd_error)?;
        drop(creation_guard);

        let prompt = args["prompt"].as_str().unwrap_or("");
        if !prompt.is_empty() || has_attachments {
            let send_body = json!({
                "command_id": format!("ctrl-start-send:{new_tid}"),
                "command_type": "conversation.turn.send",
                "payload": {"text": prompt, "attachments": args["attachments"].as_array().cloned().unwrap_or_default(), "client_message_id": format!("ctrl-init:{new_tid}")},
                "actor": {"kind": "agent", "id": caller},
            });
            let _ = chat::command_from_platform(s.clone(), new_tid.clone(), send_body, None)
                .await
                .map_err(cmd_error)?;
        }

        control::audit(
            s,
            caller,
            &new_tid,
            action::START_THREAD,
            json!({"project_id": project_id, "title": title, "routing":args["routing"]}),
        );
        return started_thread(s, &new_tid, true);
    }

    let mut payload = crate::control_tools::engine_arguments(args);
    if payload["isolation"].as_str().is_none() {
        let has_local_root = !target_project.is_empty()
            && s.store
                .list("workspaces")
                .map_err(|e| json!({"code":"storage.unavailable","message":e.to_string()}))?
                .into_iter()
                .any(|workspace| {
                    workspace["project_id"] == target_project
                        && workspace["settings"]["mode"]
                            .as_str()
                            .is_none_or(str::is_empty)
                        && workspace["root_path"]
                            .as_str()
                            .is_some_and(|path| !path.trim().is_empty())
                });
        payload["isolation"] = json!(if cfg.subagent_policy.child_worktrees && has_local_root {
            "worktree"
        } else {
            "shared"
        });
    }
    let provider = args["provider"].as_str().or_else(|| args["engine"].as_str());
    let report_tools = provider.and_then(crate::agent_runtime::descriptors::find).map(|engine|engine.identity.default_adapter_id.clone())
        .and_then(|id|crate::agent_runtime::descriptors::for_adapter(&id).map(|(_,adapter)|adapter.capabilities.mcp)).unwrap_or(true);
    if report_tools && let Some(prompt_str) = payload["prompt"].as_str() {
        payload["prompt"] = json!(format!("{prompt_str}{SUBAGENT_REPORT_FOOTER}"));
    }

    let command_id = args["idempotency_key"]
        .as_str()
        .filter(|v| !v.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| format!("cmd-{}", uuid::Uuid::new_v4()));
    let prior_key = format!("thread:{caller}:{command_id}");
    if let Some(prior) = s
        .store
        .get("conversation_commands", &prior_key)
        .map_err(|e| json!({"code":"storage.unavailable","message":e.to_string()}))?
    {
        if prior["request"]["command_type"] == "conversation.subagent.spawn" {
            let id = prior["response"]["receipt"]["output"]["thread_id"].as_str().ok_or_else(||json!({"code":"control.start_record_invalid","message":"Persisted start receipt has no thread id"}))?;
            return started_thread(s, id, false);
        }
    }
    let body = json!({
        "command_id": command_id,
        "command_type": "conversation.subagent.spawn",
        "payload": payload,
        "actor": {"kind": "agent", "id": caller},
    });
    let axum::Json(val) = crate::chat_subagents::command(s.clone(), caller.to_owned(), body, None)
        .await
        .map_err(cmd_error)?;
    let child_tid = val["receipt"]["output"]["thread_id"]
        .as_str()
        .unwrap_or(caller);
    control::audit(
        s,
        caller,
        child_tid,
        action::START_THREAD,
        json!({"command_id": command_id, "state": val["receipt"]["state"], "routing":args["routing"]}),
    );
    if val["receipt"]["state"] == "failed" {
        return Err(val["receipt"]["error"].clone());
    }
    started_thread(s, child_tid, true)
}

async fn message_attachments(
    s: &Arc<AppState>,
    caller: &str,
    target: &str,
    args: &Value,
) -> Result<Vec<Value>, Value> {
    let references = args["attachments"].as_array().cloned().unwrap_or_default();
    if references.is_empty() {
        return Ok(references);
    }
    let _guard = s.chat.active.lock().await;
    let mut destination = chat::load(s, target).map_err(cmd_error)?;
    if !destination["artifacts"].is_array() {
        destination["artifacts"] = json!([]);
    }
    for reference in &references {
        let reference = reference.as_str().ok_or_else(||json!({"code":"control.attachment_invalid","message":"attachments must name uploaded artifacts"}))?;
        let (meta, _) = crate::files::read_control_attachment(s, caller, reference)
            .map_err(|message| json!({"code":"control.attachment_invalid","message":message}))?;
        let rows = destination["artifacts"].as_array_mut().unwrap();
        if !rows
            .iter()
            .any(|row| row["artifact_id"] == meta["artifact_id"])
        {
            rows.push(meta);
        }
    }
    chat::save(s, target, &mut destination).map_err(cmd_error)?;
    Ok(references)
}

/// `muteki_send_message`: send or steer or schedule messages to `"self"`, subagents, or reachable threads.
pub async fn send_message(s: &Arc<AppState>, caller: &str, args: &Value) -> Result<Value, Value> {
    let mut normalized = args.clone();
    if let Some(raw) = args["mode"]["schedule"].get("at") {
        let at = raw.as_u64().and_then(|at|i64::try_from(at).ok()).ok_or_else(||json!({"code":"control.schedule.invalid_time","message":"schedule.at is out of range"}))?;
        let time = chrono::DateTime::from_timestamp(at, 0).ok_or_else(|| json!({"code":"control.schedule.invalid_time","message":"schedule.at is out of range"}))?;
        normalized["mode"] = json!("schedule");
        normalized["deliver_at"] = json!(time.to_rfc3339());
    }
    let args = &normalized;

    let text = args["prompt"]
        .as_str()
        .or_else(|| args["text"].as_str())
        .map(str::trim)
        .unwrap_or("");
    if text.is_empty() {
        return Err(json!({
            "code": "control.send_message.text_required",
            "category": "validation",
            "message": "`prompt` (or `text`) must be a non-empty string"
        }));
    }

    let mode = args["mode"].as_str().unwrap_or("queue");
    if !matches!(mode, "queue" | "steer" | "schedule") {
        return Err(json!({
            "code": "control.send_message.invalid_mode",
            "category": "validation",
            "message": "`mode` must be \"queue\", \"steer\", or \"schedule\""
        }));
    }

    let mut targets = Vec::<String>::new();
    if let Some(arr) = args["thread_ids"].as_array() {
        for item in arr.iter().filter_map(Value::as_str) {
            let resolved = control::resolve_thread_id(caller, item).to_owned();
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
            "code": "control.send_message.target_required",
            "category": "validation",
            "message": "specify `thread_id` or `thread_ids`"
        }));
    }

    if targets.len() == 1
        && mode == "queue"
        && args["wait"].as_bool() == Some(true)
        && targets[0] != caller
        && let Ok(cdoc) = chat::load(s, &targets[0])
        && cdoc["thread"]["state"]["lineage"]["parent_thread_id"].as_str() == Some(caller)
        && cdoc["thread"]["state"]["status"] != "running"
    {
        let mut payload = crate::control_tools::engine_arguments(args);
        payload["subagent_id"] = json!(targets[0]);
        payload["prompt"] = json!(text);
        let command_id = args["idempotency_key"]
            .as_str()
            .filter(|v| !v.is_empty())
            .map(str::to_owned)
            .unwrap_or_else(|| format!("cmd-{}", uuid::Uuid::new_v4()));
        let body = json!({
            "command_id": command_id,
            "command_type": "conversation.subagent.send",
            "payload": payload,
            "actor": {"kind": "agent", "id": caller},
        });
        let axum::Json(val) =
            crate::chat_subagents::command(s.clone(), caller.to_owned(), body, None)
                .await
                .map_err(cmd_error)?;
        control::audit(
            s,
            caller,
            &targets[0],
            action::SEND_QUEUED,
            json!({"mode": "queue", "wait": true}),
        );
        return Ok(val["receipt"].clone());
    }

    let (grant, _) = control::effective(s, caller).map_err(cmd_error)?;
    if mode == "schedule" {
        let deliver_at = if let Some(delay) = args["delay_seconds"].as_u64() {
            (chrono::Utc::now() + chrono::Duration::seconds(delay.min(86_400 * 7) as i64))
                .to_rfc3339()
        } else if let Some(raw_at) = args["deliver_at"]
            .as_str()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            chrono::DateTime::parse_from_rfc3339(raw_at)
                .map(|dt| dt.with_timezone(&chrono::Utc).to_rfc3339())
                .map_err(|e| {
                    json!({
                        "code": "control.schedule.invalid_time",
                        "category": "validation",
                        "message": format!("`deliver_at` must be RFC3339 timestamp: {e}")
                    })
                })?
        } else {
            return Err(json!({
                "code": "control.schedule.time_required",
                "category": "validation",
                "message": "mode=\"schedule\" requires `delay_seconds` or `deliver_at`"
            }));
        };

        let mut scheduled = Vec::<Value>::new();
        for target in &targets {
            if let Err(error) = control::check_target(s, caller, target, &grant) {
                scheduled.push(json!({"target_thread_id":target,"skipped":true,"reason":skip_reason(&error)}));
                continue;
            }
            let doc = match chat::load(s, target) {
                Ok(doc) => doc,
                Err(_) => {
                    scheduled.push(json!({"target_thread_id":target,"skipped":true,"reason":"unknown thread"}));
                    continue;
                }
            };
            if let Some(reason) = write_target_refusal(s, &doc) {
                scheduled.push(json!({"target_thread_id":target,"skipped":true,"reason":reason}));
                continue;
            }
            let mut side_effects = Vec::new();
            if doc["thread"]["control_lifecycle"]["settled"] == true {
                side_effects.push(json!("unsettled: a message reopens a settled thread"));
            }
            let attachments = match message_attachments(s, caller, target, args).await {
                Ok(attachments) => attachments,
                Err(error) => {
                    scheduled.push(json!({"target_thread_id":target,"error":error}));
                    continue;
                }
            };
            let at=chrono::DateTime::parse_from_rfc3339(&deliver_at).map_err(|error|json!({"code":"control.schedule.invalid_time","message":error.to_string()}))?.timestamp();
            let command_id=format!("scheduled:{}",uuid::Uuid::new_v4());
            let body=json!({"command_id":command_id,"command_type":"conversation.turn.send","payload":{"text":text,"attachments":attachments,"not_before":at,"control_scheduled":true,"client_message_id":command_id},"actor":{"kind":"agent","id":caller}});
            match chat::command_from_platform(s.clone(),target.clone(),body,None).await {
                Ok(_) => {control::audit(s,caller,target,action::SEND_SCHEDULED,json!({"deliver_at":deliver_at}));scheduled.push(json!({"target_thread_id":target,"side_effects":side_effects}));},
                Err((_,body)) => scheduled.push(json!({"target_thread_id":target,"error":body.0["error"]})),
            }
        }
        let results: Vec<Value> = scheduled.into_iter().map(|row| {
            if row["skipped"] == true {
                json!({"target":row["target_thread_id"],"outcome":"skipped","reason":row["reason"]})
            } else {
                let mut out = json!({"target":row["target_thread_id"],"outcome":if row["error"].is_null(){"applied"}else{"failed"},"reason":if row["error"].is_null(){json!("scheduled")}else{row["error"].clone()}});
                if row["error"].is_null() && row["side_effects"].as_array().is_some_and(|effects| !effects.is_empty()) {
                    out["side_effects"] = row["side_effects"].clone();
                }
                out
            }
        }).collect();
        return Ok(json!({"results":results}));
    }

    let mut deliveries = Vec::<Value>::new();
    for target in &targets {
        if let Err(error) = control::check_target(s, caller, target, &grant) {
            deliveries.push(json!({"thread_id":target,"skipped":true,"reason":skip_reason(&error)}));
            continue;
        }
        let doc = match chat::load(s, target) {
            Ok(doc) => doc,
            Err(_) => {
                deliveries.push(json!({"thread_id":target,"skipped":true,"reason":"unknown thread"}));
                continue;
            }
        };
        if let Some(reason) = write_target_refusal(s, &doc) {
            deliveries.push(json!({"thread_id":target,"skipped":true,"reason":reason}));
            continue;
        }
        let attachments = match message_attachments(s, caller, target, args).await {
            Ok(attachments) => attachments,
            Err(error) => {
                deliveries.push(json!({"thread_id":target,"ok":false,"error":error}));
                continue;
            }
        };
        let mut side_effects = Vec::new();
        if doc["thread"]["control_lifecycle"]["settled"] == true {
            side_effects.push(json!("unsettled: a message reopens a settled thread"));
        }
        if doc["thread"]["state"]["lineage"]["parent_thread_id"].as_str() == Some(caller) {
            let superseded =
                doc["thread"]["state"]["subagent_report"]["state"].as_str() == Some("expected");
            side_effects.push(json!(if superseded {
                "the report it owed for your previous message is replaced by one for this"
            } else {
                "it owes you a report for this message"
            }));
        }
        let is_running = doc["thread"]["state"]["status"] == "running";
        let adapter = doc["runtime"]["adapter_id"].as_str().unwrap_or("");
        let can_steer = crate::agent_runtime::descriptors::for_adapter(adapter)
            .is_some_and(|(_, a)| a.capabilities.steer);
        let cmd_type = if mode == "steer" && is_running && can_steer && !crate::native_chat::turn_starting(s,target,&doc) {
            "conversation.turn.steer"
        } else {
            "conversation.turn.send"
        };
        if mode == "steer" && cmd_type == "conversation.turn.send" {
            side_effects.push(json!(if is_running {
                "the provider cannot steer this running turn; queued instead"
            } else {
                "no turn was running; queued instead"
            }));
        }
        let cmd_id = format!(
            "ctrl-msg:{target}:{}",
            uuid::Uuid::new_v4()
        );
        let body = json!({
            "command_id": cmd_id,
            "command_type": cmd_type,
            "payload": {
                "text": text,
                "attachments": attachments,
                "expected_turn_id": if cmd_type == "conversation.turn.steer" { doc["thread"]["state"]["running_turn_id"].clone() } else { Value::Null },
                "client_message_id": cmd_id,
            },
            "actor": {"kind": "agent", "id": caller},
        });
        match chat::command_from_platform(s.clone(), target.clone(), body, None).await {
            Ok(axum::Json(res)) => {
                let delivery = if cmd_type == "conversation.turn.steer" { "steered" } else { "queued" };
                control::audit(
                    s,
                    caller,
                    target,
                    if delivery == "steered" { action::SEND_STEERED } else { action::SEND_QUEUED },
                    Value::Null,
                );
                deliveries.push(json!({
                    "thread_id": target,
                    "ok": true,
                    "delivery": delivery,
                    "side_effects": side_effects,
                    "receipt": res["receipt"],
                }));
            }
            Err(err) => {
                deliveries.push(json!({
                    "thread_id": target,
                    "ok": false,
                    "error": cmd_error(err),
                }));
            }
        }
    }

    let results: Vec<Value> = deliveries.into_iter().map(|row| {
        if row["skipped"] == true {
            json!({"target":row["thread_id"],"outcome":"skipped","reason":row["reason"]})
        } else if row["ok"] == true {
            let mut out = json!({"target":row["thread_id"],"outcome":"applied","reason":row["delivery"]});
            if row["side_effects"].as_array().is_some_and(|effects| !effects.is_empty()) {
                out["side_effects"] = row["side_effects"].clone();
            }
            out
        } else {
            json!({"target":row["thread_id"],"outcome":"failed","reason":row["error"]})
        }
    }).collect();
    Ok(json!({"results":results}))
}

pub async fn sweep_scheduled_messages(s: &Arc<AppState>) {
    match s.store.scheduled_conversation_threads() {
        Ok(ids) => {
            for id in ids {
                let guard = s.chat.active.lock().await;
                let Ok(mut doc) = chat::load(s, &id) else { continue; };
                let before = doc.clone();
                let next = chat::promote_queue(s, &id, &mut doc);
                if doc != before {
                    if let Err((_, body)) = chat::save(s, &id, &mut doc) {
                        tracing::error!(error=%body.0, "scheduled queue could not be saved");
                        continue;
                    }
                }
                drop(guard);
                if let Some((turn, dispatch)) = next {
                    chat::start(s.clone(), id, turn, dispatch).await;
                }
            }
        }
        Err(error) => tracing::error!(%error, "scheduled conversation queues could not be read"),
    }
    let Ok(rows) = s.store.list(SCHEDULED_MESSAGES) else {
        return;
    };
    let now = chrono::Utc::now();
    for row in rows {
        let Some(sid) = row["schedule_id"].as_str() else {
            continue;
        };
        let due = row["deliver_at"]
            .as_str()
            .and_then(|ts| chrono::DateTime::parse_from_rfc3339(ts).ok())
            .is_some_and(|dt| dt.with_timezone(&chrono::Utc) <= now);
        if !due {
            continue;
        }
        if row["status"] == "failed" {
            continue;
        }
        let target = row["target_thread_id"].as_str().unwrap_or("");
        let body = json!({"command_id":format!("scheduled:{sid}"),"command_type":"conversation.turn.send",
            "payload":{"text":row["text"],"attachments":row["attachments"].as_array().cloned().unwrap_or_default(),"client_message_id":format!("scheduled:{sid}")},
            "actor":{"kind":"agent","id":row["actor_thread_id"]}});
        match chat::command_from_platform(s.clone(), target.to_owned(), body, None).await {
            Ok(_) => {
                if let Err(error) = s.store.delete(SCHEDULED_MESSAGES, sid) {
                    tracing::error!(%error,sid,"could not remove delivered schedule");
                }
            }
            Err((_, error)) => {
                let mut failed = row.clone();
                failed["status"] = json!("failed");
                failed["error"] = error.0["error"].clone();
                if let Err(error) = s.store.put(SCHEDULED_MESSAGES, sid, &failed) {
                    tracing::error!(%error,sid,"could not persist failed schedule");
                }
                control::audit(
                    s,
                    row["actor_thread_id"].as_str().unwrap_or(""),
                    target,
                    "scheduled_message_failed",
                    error.0,
                );
            }
        }
    }
}

/// `muteki_control_thread`: operational control (`interrupt`, `retry`, `compact`, `set_model`,
/// `set_option`, `fork`, `fork_at`, `steer_queued`, `drop_queued`, `pause_queue`, `resume_queue`, `cancel`).
pub async fn control_thread(s: &Arc<AppState>, caller: &str, args: &Value) -> Result<Value, Value> {
    let target = control::resolve_thread_id(
        caller,
        args["thread_id"].as_str().unwrap_or(control::OWN_THREAD),
    )
    .to_owned();
    let action = args["action"].as_str().map(str::trim).unwrap_or("");
    let cmd_id = format!("ctrl-{action}:{target}:{}", uuid::Uuid::new_v4().simple());

    let result = match action {
        "interrupt" => {
            let body = json!({
                "command_id": cmd_id,
                "command_type": "conversation.turn.interrupt",
                "payload": {},
                "actor": {"kind": "agent", "id": caller},
            });
            chat::command_from_platform(s.clone(), target.clone(), body, None)
                .await
                .map(|axum::Json(v)| v["receipt"].clone())
                .map_err(cmd_error)?
        }
        "retry" => {
            let mut payload = json!({});
            if let Some(turn_id) = args["turn_id"].as_str().filter(|s| !s.is_empty()) {
                payload["turn_id"] = json!(turn_id);
            }
            let body = json!({
                "command_id": cmd_id,
                "command_type": "conversation.turn.retry",
                "payload": payload,
                "actor": {"kind": "agent", "id": caller},
            });
            chat::command_from_platform(s.clone(), target.clone(), body, None)
                .await
                .map(|axum::Json(v)| v["receipt"].clone())
                .map_err(cmd_error)?
        }
        "compact" => {
            let doc = chat::load(s, &target).map_err(cmd_error)?;
            let adapter = doc["runtime"]["adapter_id"].as_str().unwrap_or("");
            if !crate::agent_runtime::pool::manual_compaction(adapter) {
                let provider = crate::agent_runtime::descriptors::for_adapter(adapter)
                    .map(|(engine, _)| engine.identity.display_name.clone())
                    .unwrap_or_else(|| adapter.to_owned());
                return Err(json!({
                    "code": "control.compact.unsupported",
                    "category": "capability",
                    "message": format!("{provider} has no manual compaction")
                }));
            }
            let body = json!({
                "command_id": cmd_id,
                "command_type": "conversation.turn.send",
                "payload": {"text": "/compact", "client_message_id": cmd_id},
                "actor": {"kind": "agent", "id": caller},
            });
            chat::command_from_platform(s.clone(), target.clone(), body, None)
                .await
                .map(|axum::Json(v)| v["receipt"].clone())
                .map_err(cmd_error)?
        }
        "fork" | "fork_at" => {
            let mut payload = json!({});
            if let Some(turn_id) = args["turn_id"].as_str().filter(|s| !s.is_empty()) {
                payload["turn_id"] = json!(turn_id);
            }
            if let Some(title) = args["title"].as_str().filter(|s| !s.is_empty()) {
                payload["title"] = json!(title);
            }
            let body = json!({
                "command_id": cmd_id,
                "command_type": "conversation.thread.fork",
                "payload": payload,
                "actor": {"kind": "agent", "id": caller},
            });
            chat::command_from_platform(s.clone(), target.clone(), body, None)
                .await
                .map(|axum::Json(v)| v["receipt"].clone())
                .map_err(cmd_error)?
        }
        "set_model" | "set_option" | "set_runtime" => {
            let mut updated_runtime = Value::Null;
            {
                let mut doc = chat::load(s, &target).map_err(cmd_error)?;
                let before = doc.clone();
                let mut rt = doc["runtime"].clone();
                if let Some(options) = args.get("options") {
                    rt["options"] = options.clone();
                }
                if let Some(id) = args["option_id"].as_str() {
                    if !rt["options"].is_object() {
                        rt["options"] = json!({});
                    }
                    rt["options"][id] = args["option_value"].clone();
                }

                for key in [
                    "adapter_id",
                    "instance_id",
                    "credential_id",
                    "model",
                    "effort",
                    "service_tier",
                    "interaction_mode",
                    "access_mode",
                ] {
                    if let Some(v) = args[key].as_str().map(str::trim) {
                        rt[key] = json!(v);
                    }
                }
                if chat::run_side::owned_by_run(&doc["thread"]) {
                    rt = chat::run_side::admit_turn_runtime(s, &mut doc, &rt).map_err(cmd_error)?;
                }
                if let Some(mode) = rt["interaction_mode"].as_str() {
                    chat::require_interaction_mode(s, &rt, mode).map_err(cmd_error)?;
                }
                chat::resolve_runtime(s, &rt).map_err(cmd_error)?;
                doc["runtime"] = rt.clone();
                chat::emit(
                    s,
                    &target,
                    &mut doc,
                    "core.thread.runtime_updated",
                    json!({"thread_id": target, "runtime": rt}),
                );
                if let Some((events, watermark)) = s
                    .store
                    .chat_commit_checked(&target, &doc, Some(&before), None)
                    .map_err(|e| control::storage(e).1.0["error"].clone())?
                {
                    chat::finish_save(s, &target, &mut doc, events, watermark);
                    updated_runtime = rt;
                }
            }
            if updated_runtime.is_null() {
                return Err(json!({"code":"control.runtime_conflict","category":"conflict","message":"Thread runtime changed concurrently; no update was committed"}));
            }
            json!({"thread_id": target, "runtime": updated_runtime})
        }
        "steer_queued" | "drop_queued" | "pause_queue" | "resume_queue" => {
            let cmd_type = match action {
                "steer_queued" => "conversation.queue.steer",
                "drop_queued" => "conversation.queue.delete",
                "pause_queue" => "conversation.queue.pause",
                _ => "conversation.queue.resume",
            };
            let mut payload = json!({});
            if action == "steer_queued" {
                let doc=chat::load(s,&target).map_err(cmd_error)?;
                payload["expected_turn_id"]=doc["thread"]["state"]["running_turn_id"].clone();
            }
            if let Some(qid) = args["queue_id"].as_str().filter(|s| !s.is_empty()) {
                payload["queue_id"] = json!(qid);
            }
            let body = json!({
                "command_id": cmd_id,
                "command_type": cmd_type,
                "payload": payload,
                "actor": {"kind": "agent", "id": caller},
            });
            chat::command_from_platform(s.clone(), target.clone(), body, None)
                .await
                .map(|axum::Json(v)| v["receipt"].clone())
                .map_err(cmd_error)?
        }
        "cancel" => {
            let doc = chat::load(s, &target).map_err(cmd_error)?;
            if doc["thread"]["state"]["lineage"]["parent_thread_id"].as_str() == Some(caller) {
                let body = json!({
                    "command_id": cmd_id,
                    "command_type": "conversation.subagent.cancel",
                    "payload": {"subagent_id": target, "reason": args["reason"]},
                    "actor": {"kind": "agent", "id": caller},
                });
                crate::chat_subagents::command(s.clone(), caller.to_owned(), body, None)
                    .await
                    .map(|axum::Json(v)| v["receipt"].clone())
                    .map_err(cmd_error)?
            } else {
                let body = json!({
                    "command_id": cmd_id,
                    "command_type": "conversation.turn.interrupt",
                    "payload": {},
                    "actor": {"kind": "agent", "id": caller},
                });
                chat::command_from_platform(s.clone(), target.clone(), body, None)
                    .await
                    .map(|axum::Json(v)| v["receipt"].clone())
                    .map_err(cmd_error)?
            }
        }
        _ => {
            return Err(json!({
                "code": "control.control_thread.invalid_action",
                "category": "validation",
                "message": "action must be interrupt, retry, compact, set_model, set_option, fork, fork_at, steer_queued, drop_queued, pause_queue, resume_queue, or cancel"
            }));
        }
    };

    let (audit_action, audit_detail): (&str, Value) = match action {
        "interrupt" => (action::INTERRUPT, Value::Null),
        "retry" => (action::RETRY, args["turn_id"].clone()),
        "compact" => (action::COMPACT, Value::Null),
        "set_model" => (action::SET_MODEL, args["model"].clone()),
        "set_option" => (action::SET_OPTION, args["option_id"].clone()),
        "fork" | "fork_at" => (action::FORK, args["turn_id"].clone()),
        "steer_queued" => (action::STEER_QUEUED, args["queue_id"].clone()),
        "drop_queued" => (action::DROP_QUEUED, args["queue_id"].clone()),
        "cancel" => (action::CANCEL, Value::Null),
        other => (other, Value::Null),
    };
    control::audit(s, caller, &target, audit_action, audit_detail);
    Ok(result)
}

/// `muteki_respond_input`: answer a pending user-input prompt on a subagent (`Operate`) or another thread (`AnswerInput`).
pub async fn respond_input(s: &Arc<AppState>, caller: &str, args: &Value) -> Result<Value, Value> {
    let target =
        control::resolve_thread_id(caller, args["thread_id"].as_str().unwrap_or("")).to_owned();
    if target.is_empty() || target == caller {
        return Err(json!({
            "code": "control.respond_input.invalid_target",
            "category": "validation",
            "message": "`thread_id` must name another thread awaiting user input"
        }));
    }
    let doc = chat::load(s, &target).map_err(cmd_error)?;
    let pending = &doc["thread"]["state"]["pending_user_input"];
    if pending["status"] != "pending" {
        return Err(json!({
            "code": "control.respond_input.not_pending",
            "category": "state",
            "message": format!("thread {target} has no pending user_input request")
        }));
    }
    let request_id = args["request_id"]
        .as_str()
        .filter(|s| !s.is_empty())
        .or_else(|| pending["request_id"].as_str())
        .unwrap_or("")
        .to_owned();
    let decision = args["decision"].as_str().unwrap_or("submit");
    let cmd_id = format!("ctrl-input:{target}:{request_id}");
    let body = json!({
        "command_id": cmd_id,
        "command_type": "conversation.user_input.resolve",
        "payload": {
            "request_id": request_id,
            "decision": decision,
            "answers": args.get("answers").cloned().unwrap_or_else(|| json!({})),
            "text": args.get("text").cloned().unwrap_or(Value::Null),
        },
        "actor": {"kind": "agent", "id": caller},
    });
    let axum::Json(res) = chat::command_from_platform(s.clone(), target.clone(), body, None)
        .await
        .map_err(cmd_error)?;
    control::audit(
        s,
        caller,
        &target,
        "respond_input",
        json!({"request_id": request_id, "decision": decision}),
    );
    Ok(res["receipt"].clone())
}

/// `muteki_respond_approval`: resolve a pending tool/command approval on caller's descendant subagent.
pub async fn respond_approval(
    s: &Arc<AppState>,
    caller: &str,
    args: &Value,
) -> Result<Value, Value> {
    let target =
        control::resolve_thread_id(caller, args["thread_id"].as_str().unwrap_or("")).to_owned();
    if target.is_empty() || !crate::chat_subagents::is_descendant_of(s, &target, caller) {
        return Err(json!({
            "code": "control.respond_approval.subagent_only",
            "category": "permission",
            "message": "`muteki_respond_approval` may only resolve approvals on your own descendant subagents"
        }));
    }
    let cfg = control::settings(s).map_err(cmd_error)?;
    if cfg.subagent_policy.approval_mode == "manual" {
        return Err(json!({
            "code": "control.subagent.manual_approval_only",
            "category": "permission",
            "message": "subagent_policy.approval_mode is \"manual\"; only the human user may resolve subagent approvals"
        }));
    }
    let decision = args["decision"].as_str().unwrap_or("");
    if !matches!(decision, "approved" | "declined") {
        return Err(json!({
            "code": "control.respond_approval.invalid_decision",
            "category": "validation",
            "message": "`decision` must be \"approved\" or \"declined\""
        }));
    }
    let doc = chat::load(s, &target).map_err(cmd_error)?;
    let approval_id = if let Some(id) = args["approval_id"].as_str().filter(|s| !s.is_empty()) {
        id.to_owned()
    } else {
        doc["thread"]["state"]["pending_approvals"]
            .as_object()
            .and_then(|map| {
                map.iter()
                    .find(|(_, v)| v["status"] == "pending")
                    .map(|(k, _)| k.clone())
            })
            .ok_or_else(|| {
                json!({
                    "code": "control.respond_approval.not_pending",
                    "category": "state",
                    "message": format!("subagent {target} has no pending approval")
                })
            })?
    };

    let cmd_id = format!("ctrl-approval:{target}:{approval_id}:{decision}");
    let body = json!({
        "command_id": cmd_id,
        "command_type": "conversation.approval.resolve",
        "payload": {
            "approval_id": approval_id,
            "decision": decision,
            "scope": args["scope"].as_str().unwrap_or("once"),
            "option_id": args["option_id"],
            "note": args["note"],
        },
        "actor": {"kind": "agent", "id": caller},
    });
    let axum::Json(res) = chat::command_from_platform(s.clone(), target.clone(), body, None)
        .await
        .map_err(cmd_error)?;
    control::audit(
        s,
        caller,
        &target,
        "respond_approval",
        json!({"approval_id": approval_id, "decision": decision}),
    );
    Ok(res["receipt"].clone())
}

/// `muteki_rewind_thread` (`Category::HistoryEdit`): rewind files and/or conversation history to `turn_id`.
pub async fn rewind_thread(s: &Arc<AppState>, caller: &str, args: &Value) -> Result<Value, Value> {
    let target = control::resolve_thread_id(
        caller,
        args["thread_id"].as_str().unwrap_or(control::OWN_THREAD),
    )
    .to_owned();
    let turn_id = args["turn_id"].as_str().map(str::trim).unwrap_or("");
    if turn_id.is_empty() {
        return Err(json!({
            "code": "control.rewind.turn_required",
            "category": "validation",
            "message": "`turn_id` is required"
        }));
    }
    let scope = args["scope"].as_str().unwrap_or("files_and_conversation");
    if !matches!(scope, "files" | "conversation" | "files_and_conversation") {
        return Err(json!({
            "code": "control.rewind.invalid_scope",
            "category": "validation",
            "message": "`scope` must be \"files\", \"conversation\", or \"files_and_conversation\""
        }));
    }

    let doc = chat::load(s, &target).map_err(cmd_error)?;
    if doc["thread"]["state"]["status"] == "running" {
        return Err(json!({
            "code": "control.rewind.thread_running",
            "category": "state",
            "message": "cannot rewind a thread while a turn is running"
        }));
    }

    let mut file_result = Value::Null;
    if matches!(scope, "files" | "files_and_conversation") {
        let root = chat::workspace_root(s, &doc);
        if root.trim().is_empty() {
            return Err(json!({
                "code": "conversation.workspace.not_found",
                "category": "not_found",
                "message": "thread has no bound workspace to restore files"
            }));
        }
        file_result = checkpoints::restore(&root, &target, turn_id)
            .await
            .map_err(|e| {
                json!({
                    "code": e.code,
                    "category": "state",
                    "message": e.message,
                })
            })?;
    }

    let mut conv_result = Value::Null;
    if matches!(scope, "conversation" | "files_and_conversation") {
        let cmd_id = format!(
            "ctrl-rewind:{target}:{turn_id}:{}",
            uuid::Uuid::new_v4().simple()
        );
        let body = json!({
            "command_id": cmd_id,
            "command_type": "conversation.turn.native_rewind",
            "payload": {
                "turn_id": turn_id,
                "keep_files": true,
            },
            "actor": {"kind": "agent", "id": caller},
        });
        let axum::Json(res) = chat::command_from_platform(s.clone(), target.clone(), body, None)
            .await
            .map_err(cmd_error)?;
        conv_result = res["receipt"].clone();
    }

    control::audit(
        s,
        caller,
        &target,
        action::REWIND,
        json!({"turn_id": turn_id, "scope": scope}),
    );
    Ok(json!({
        "thread_id": target,
        "turn_id": turn_id,
        "scope": scope,
        "files_restored": file_result,
        "conversation_rewound": conv_result,
    }))
}

/// `muteki_move_thread`: rebind a thread to another `project_id`.
pub fn move_thread(s: &Arc<AppState>, caller: &str, args: &Value) -> Result<Value, Value> {
    let target = control::resolve_thread_id(
        caller,
        args["thread_id"].as_str().unwrap_or(control::OWN_THREAD),
    )
    .to_owned();
    let project_id = args["project_id"].as_str().map(str::trim).unwrap_or("");
    if project_id.is_empty() {
        return Err(json!({
            "code": "control.move_thread.project_required",
            "category": "validation",
            "message": "`project_id` is required"
        }));
    }
    if s.store.get("projects", project_id).ok().flatten().is_none() {
        return Err(json!({
            "code": "control.project.not_found",
            "category": "not_found",
            "message": format!("project {project_id} does not exist")
        }));
    }

    let workspace_id = args["workspace_id"]
        .as_str()
        .map(str::trim)
        .filter(|w| !w.is_empty())
        .map(str::to_owned)
        .or_else(|| {
            s.store
                .list("workspaces")
                .ok()?
                .into_iter()
                .find(|w| w["project_id"] == project_id && w["kind"] == "local")
                .and_then(|w| w["workspace_id"].as_str().map(str::to_owned))
        });

    for _ in 0..8 {
        let mut doc = chat::load(s, &target).map_err(cmd_error)?;
        if doc["thread"]["state"]["status"] == "running" {
            return Err(json!({
                "code": "control.move_thread.running",
                "category": "state",
                "message": "cannot move a thread while a turn is running"
            }));
        }
        let before = doc.clone();
        doc["thread"]["project_id"] = json!(project_id);
        if let Some(wid) = &workspace_id {
            doc["thread"]["workspace_id"] = json!(wid);
        }
        chat::emit(
            s,
            &target,
            &mut doc,
            "core.thread.moved",
            json!({"thread_id": target, "project_id": project_id, "workspace_id": workspace_id}),
        );
        if let Some((events, watermark)) = s
            .store
            .chat_commit_checked(&target, &doc, Some(&before), None)
            .map_err(|e| control::storage(e).1.0["error"].clone())?
        {
            chat::finish_save(s, &target, &mut doc, events, watermark);
            control::audit(
                s,
                caller,
                &target,
                action::MOVE,
                json!({"project_id": project_id, "workspace_id": workspace_id}),
            );
            return Ok(json!({
                "thread_id": target,
                "project_id": project_id,
                "workspace_id": workspace_id,
            }));
        }
    }
    Err(json!({
        "code": "control.thread_busy",
        "category": "state",
        "message": "thread changed concurrently while moving"
    }))
}

/// `muteki_git`: local Git operations (`Operate`) and remote/merge operations (`Category::GitRemote`).
pub async fn git_tool(s: &Arc<AppState>, caller: &str, args: &Value) -> Result<Value, Value> {
    let target = control::resolve_thread_id(
        caller,
        args["thread_id"].as_str().unwrap_or(control::OWN_THREAD),
    )
    .to_owned();
    let action = args["action"].as_str().map(str::trim).unwrap_or("");
    let paths: Vec<String> = args["paths"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_owned)
        .collect();
    let map_git_err =
        |(_, axum::Json(b)): (axum::http::StatusCode, axum::Json<Value>)| b["error"].clone();

    let result = match action {
        "checkout" | "create_branch" => {
            let branch = args["branch"].as_str().unwrap_or("").to_owned();
            let create = action == "create_branch" || args["create"].as_bool().unwrap_or(false);
            git_api::thread_checkout_op(s, &target, branch, create)
                .await
                .map_err(map_git_err)?
        }
        "stage" | "unstage" => git_api::thread_stage_op(s, &target, &paths, action == "unstage")
            .await
            .map_err(map_git_err)?,
        "commit" => {
            let message = args["message"].as_str().unwrap_or("").to_owned();
            let amend = args["amend"].as_bool().unwrap_or(false);
            let opt_paths = if paths.is_empty() { None } else { Some(paths) };
            git_api::thread_commit_op(s, &target, message, opt_paths, amend)
                .await
                .map_err(map_git_err)?
        }
        "push" | "publish_branch" => {
            let set_up = if action == "publish_branch" {
                Some(true)
            } else {
                args["set_upstream"].as_bool()
            };
            git_api::thread_push_op(s, &target, set_up)
                .await
                .map_err(map_git_err)?
        }
        "commit_push" => {
            let message = args["message"].as_str().unwrap_or("").to_owned();
            let amend = args["amend"].as_bool().unwrap_or(false);
            let opt_paths = if paths.is_empty() { None } else { Some(paths) };
            let committed = git_api::thread_commit_op(s, &target, message, opt_paths, amend)
                .await
                .map_err(map_git_err)?;
            let pushed = git_api::thread_push_op(s, &target, args["set_upstream"].as_bool())
                .await
                .map_err(map_git_err)?;
            json!({"commit": committed, "push": pushed})
        }
        "pull" => {
            let rebase = args["rebase"].as_bool().unwrap_or(false);
            git_api::thread_pull_op(s, &target, rebase)
                .await
                .map_err(map_git_err)?
        }
        "merge_worktree" => {
            let source_tid =
                control::resolve_thread_id(caller, args["source_thread_id"].as_str().unwrap_or(""))
                    .to_owned();
            if source_tid.is_empty() || source_tid == target {
                return Err(json!({
                    "code": "control.git.source_required",
                    "category": "validation",
                    "message": "`source_thread_id` must name the worktree subagent thread to merge from"
                }));
            }
            let squash = args["squash"].as_bool().unwrap_or(false);
            git_api::thread_merge_worktree_op(s, &source_tid, &target, squash)
                .await
                .map_err(map_git_err)?
        }
        _ => {
            return Err(json!({
                "code": "control.git.invalid_action",
                "category": "validation",
                "message": "action must be checkout, create_branch, stage, unstage, commit, push, publish_branch, commit_push, pull, or merge_worktree"
            }));
        }
    };

    let audit_action = match action {
        "checkout" | "create_branch" => action::GIT_CHECKOUT,
        "commit" => action::GIT_COMMIT,
        "commit_push" => action::GIT_COMMIT_PUSH,
        "push" => action::GIT_PUSH,
        "pull" => action::GIT_PULL,
        "publish_branch" => action::GIT_PUBLISH_BRANCH,
        "merge_worktree" => action::GIT_MERGE_WORKTREE,
        _ => "git",
    };
    control::audit(s, caller, &target, audit_action, json!({"action": action}));
    Ok(result)
}

/// `muteki_pull_request`: create/inspect PRs (`Operate` / `Observe`) and comment/reply (`Category::PrWrite`).
pub async fn pull_request_tool(
    s: &Arc<AppState>,
    caller: &str,
    args: &Value,
) -> Result<Value, Value> {
    let target = control::resolve_thread_id(
        caller,
        args["thread_id"].as_str().unwrap_or(control::OWN_THREAD),
    )
    .to_owned();
    let action = args["action"].as_str().unwrap_or("status");
    let map_pr_err =
        |(_, axum::Json(b)): (axum::http::StatusCode, axum::Json<Value>)| b["error"].clone();

    let result = match action {
        "status" => github_pr::inspect_thread_prs(s, &target, args["number"].as_u64(), true, true)
            .await
            .map_err(map_pr_err)?,
        "create" => {
            let title = args["title"].as_str().unwrap_or("").to_owned();
            let body = args["body"].as_str().unwrap_or("").to_owned();
            let base = args["base"].as_str().unwrap_or("").to_owned();
            let draft = args["draft"].as_bool().unwrap_or(false);
            let push_first = args["push_first"].as_bool().unwrap_or(true);
            let created =
                github_pr::create_thread_pr(s, &target, title, body, base, draft, push_first)
                    .await
                    .map_err(map_pr_err)?;
            control::audit(
                s,
                caller,
                &target,
                "pr_create",
                json!({"number": created["number"], "url": created["url"]}),
            );
            created
        }
        "comment" | "reply" => {
            let number = args["number"].as_u64().ok_or_else(|| {
                json!({
                    "code": "control.pr.number_required",
                    "category": "validation",
                    "message": "`number` (PR number) is required for comment/reply"
                })
            })?;
            let body = args["body"].as_str().unwrap_or("").to_owned();
            let reply_id = if action == "reply" {
                args["comment_id"].as_u64()
            } else {
                None
            };
            let commented = github_pr::comment_thread_pr(s, &target, number, body, reply_id)
                .await
                .map_err(map_pr_err)?;
            control::audit(
                s,
                caller,
                &target,
                if action == "reply" { action::PR_REPLY } else { action::PR_COMMENT },
                json!({"number": number, "comment_id": commented["id"]}),
            );
            commented
        }
        _ => {
            return Err(json!({
                "code": "control.pr.invalid_action",
                "category": "validation",
                "message": "action must be status, create, comment, or reply"
            }));
        }
    };
    Ok(result)
}

/// `muteki_terminal` (`Category::Terminal`): open, write input, interrupt, resize, or close PTY terminals.
pub fn terminal_tool(s: &Arc<AppState>, caller: &str, args: &Value) -> Result<Value, Value> {
    let target = control::resolve_thread_id(
        caller,
        args["thread_id"].as_str().unwrap_or(control::OWN_THREAD),
    )
    .to_owned();
    let action = args["action"].as_str().map(str::trim).unwrap_or("");

    match action {
        "open" => {
            let doc = chat::load(s, &target).map_err(cmd_error)?;
            let wid = doc["thread"]["workspace_id"].as_str().unwrap_or("");
            let root = bound_root(s, &target).map_err(|(_, axum::Json(b))| b["error"].clone())?;
            let cols = args["cols"].as_u64().unwrap_or(100).clamp(20, 500) as u16;
            let rows = args["rows"].as_u64().unwrap_or(30).clamp(5, 200) as u16;
            let name = args["name"].as_str().unwrap_or("control");
            let session = s
                .terminals
                .create(&target, wid, &root.to_string_lossy(), name, cols, rows)
                .map_err(cmd_error)?;
            let pub_view = session.public();
            control::audit(
                s,
                caller,
                &target,
                action::TERMINAL_OPEN,
                json!({"session_id": pub_view["session_id"]}),
            );
            Ok(pub_view)
        }
        "input" | "interrupt" | "resize" | "close" => {
            let sid = args["session_id"]
                .as_str()
                .map(str::trim)
                .filter(|v| !v.is_empty())
                .map(str::to_owned)
                .or_else(|| {
                    s.terminals
                        .list(&target)
                        .into_iter()
                        .find(|sess| sess.is_running())
                        .and_then(|sess| sess.public()["session_id"].as_str().map(str::to_owned))
                })
                .ok_or_else(|| {
                    json!({
                        "code": "control.terminal.not_found",
                        "category": "not_found",
                        "message": "no running terminal session found; specify session_id or call action=\"open\""
                    })
                })?;
            let session = s.terminals.get(&target, &sid).ok_or_else(|| {
                json!({
                    "code": "control.terminal.not_found",
                    "category": "not_found",
                    "message": format!("terminal session {sid} does not exist")
                })
            })?;
            match action {
                "input" => {
                    let input = args["input"].as_str().unwrap_or("");
                    let append_newline = args["append_newline"].as_bool().unwrap_or(false);
                    let payload = if append_newline {
                        format!("{input}\n")
                    } else {
                        input.to_owned()
                    };
                    session.write_input_str(&payload).map_err(
                        |e| json!({"code": e.code, "category": "state", "message": e.message}),
                    )?;
                }
                "interrupt" => {
                    session.interrupt().map_err(
                        |e| json!({"code": e.code, "category": "state", "message": e.message}),
                    )?;
                }
                "resize" => {
                    let cols = args["cols"].as_u64().unwrap_or(100).clamp(20, 500) as u16;
                    let rows = args["rows"].as_u64().unwrap_or(30).clamp(5, 200) as u16;
                    session.resize_session(cols, rows);
                }
                "close" => {
                    session.close_session("control closed terminal");
                }
                _ => {}
            }
            let audit_action = match action {
                "input" => action::TERMINAL_INPUT,
                "close" => action::TERMINAL_CLOSE,
                _ => "terminal_action",
            };
            control::audit(
                s,
                caller,
                &target,
                audit_action,
                json!({"action": action, "session_id": sid}),
            );
            Ok(session.public())
        }
        _ => Err(json!({
            "code": "control.terminal.invalid_action",
            "category": "validation",
            "message": "action must be open, input, interrupt, resize, or close"
        })),
    }
}
