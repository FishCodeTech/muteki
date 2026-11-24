//! Typed lifecycle and cross-thread actions for the pinned control contract.
use crate::{AppState, chat, control, control_contract::*, control_state};
use serde_json::{Value, json};
use std::sync::Arc;
fn error(message: impl ToString) -> Value {
    json!({"code":"control.action_failed","message":message.to_string()})
}
fn reason(error: &Value) -> String {
    error["message"]
        .as_str()
        .map(str::to_owned)
        .unwrap_or_else(|| error.to_string())
}
fn check(s: &AppState, caller: &str, target: &str) -> Result<String, Value> {
    let target = control::resolve_thread_id(caller, target);
    let (grant, _) = control::effective(s, caller).map_err(|(_, b)| b.0["error"].clone())?;
    control::check_target(s, caller, target, &grant)?;
    Ok(target.to_owned())
}
fn save(s: &AppState, id: &str, doc: &Value) -> Result<(), Value> {
    control::save_thread_doc(s, id, doc).map_err(|(_, b)| b.0["error"].clone())
}

fn describe_settle(effects: &Value) -> Vec<String> {
    let mut described = Vec::new();
    if effects["provider_detached"] == true { described.push("provider detached".into()); }
    for (key, one, many, suffix) in [
        ("pull_request_watches_ended", "PR watch", "PR watches", " ended"),
        ("queued_dropped", "non-Human queued message", "non-Human queued messages", " dropped"),
        ("terminals_checked", "terminal", "terminals", " checked; those at an idle shell closed"),
    ] {
        let count = effects[key].as_u64().unwrap_or(0);
        if count > 0 { described.push(format!("{count} {}{suffix}", if count == 1 { one } else { many })); }
    }
    if effects["unpinned"] == true { described.push("unpinned".into()); }
    if effects["input_closed"] == true { described.push("pending non-blocking input closed unanswered".into()); }
    described
}

pub async fn update(
    s: &Arc<AppState>,
    caller: &str,
    p: UpdateThreadsParams,
) -> Result<Value, Value> {
    let dry = p.dry_run.unwrap_or(false);
    let mut results = Vec::new();
    for item in p.changes {
        let id = match check(s, caller, &item.thread_id) {
            Ok(id) => id,
            Err(error) => {
                results.push(json!({"target":item.thread_id,"outcome":"skipped","reason":reason(&error)}));
                continue;
            }
        };
        let result = async {
            let doc = chat::load(s, &id).map_err(|(_, body)| body.0["error"].clone())?;
            let mut ctrl = control::thread_doc(s, &id).map_err(|(_, body)| body.0["error"].clone())?;
            let meta = control_state::meta(s, &doc)?;
            let skipped = |reason: &str| json!({"target":id,"outcome":"skipped","reason":reason});
            if let Some(expect) = &item.expect {
                let same_time = expect.last_activity_at.is_none_or(|at| control_state::activity(&doc).last_activity_at() == Some(at));
                let same_state = expect.state.as_ref().is_none_or(|wanted| wanted.eq_ignore_ascii_case(if meta.archived_at.is_some() { "archived" } else if meta.is_settled() { "settled" } else { "active" }));
                let same_position = match expect.max_position {
                    Some(wanted) => crate::control_log::load(s, &id)?.1.as_of_position() == wanted,
                    None => true,
                };
                if !same_time || !same_state || !same_position { return Ok(skipped("changed_since_preview")); }
            }
            let archived = meta.archived_at.is_some();
            let no_change = match &item.op {
                ThreadOp::Settle if meta.is_settled() && !archived => Some("already settled"),
                ThreadOp::Unsettle if ctrl["manual_unsettle"] == true => Some("already held active"),
                ThreadOp::Archive if archived => Some("already archived"),
                ThreadOp::Unarchive if !archived => Some("not archived"),
                ThreadOp::Pin if meta.pinned_at.is_some() && !meta.is_settled() => Some("already pinned"),
                ThreadOp::Unpin if meta.pinned_at.is_none() && !archived => Some("not pinned"),
                ThreadOp::Rename { title } if title.trim().is_empty() => Some("a title cannot be empty"),
                ThreadOp::FollowUp { on: false, .. } if meta.follow_up.is_none() => Some("not on Follow-up"),
                ThreadOp::Park { on } if *on == meta.parked_at.is_some() => Some(if *on { "already parked" } else { "not parked" }),
                ThreadOp::Keep { on } if *on == meta.auto_settle_disabled_at.is_some() => Some(if *on { "already kept" } else { "not kept" }),
                _ => None,
            };
            if let Some(reason) = no_change { return Ok(skipped(reason)); }
            if archived && matches!(item.op, ThreadOp::Settle | ThreadOp::Unsettle | ThreadOp::Pin | ThreadOp::Unpin | ThreadOp::Keep { .. }) {
                return Ok(skipped("Unarchive this thread before changing its lifecycle."));
            }
            let blockers = control_state::blockers(&doc);
            if matches!(item.op, ThreadOp::Settle) && (crate::control_subagents::busy(&doc) || blockers.unsettled_children || blockers.pending_approval || blockers.pending_human_message || (blockers.pending_input && doc["thread"]["state"]["pending_user_input"]["native"]["background"] != true)) {
                return Ok(skipped("busy"));
            }
            let mut effects = match &item.op {
                ThreadOp::Unsettle => vec!["override becomes active; nothing is restored".into()],
                ThreadOp::Settle if dry => describe_settle(&json!({
                    "provider_detached":doc["native_session"]["open"] == true,
                    "pull_request_watches_ended":ctrl["pull_requests"].as_array().into_iter().flatten().filter(|link| !link["watch"].is_null()).count(),
                    "queued_dropped":doc["queue"].as_array().into_iter().flatten().filter(|row| row["origin"] != "human").count(),
                    "terminals_checked":s.terminals.list(&id).len(),
                    "unpinned":meta.pinned_at.is_some(), "input_closed":blockers.pending_input,
                })),
                _ => Vec::new(),
            };
            if !dry {
                let mut args = match &item.op {
                    ThreadOp::Settle => json!({"action":"settle"}),
                    ThreadOp::Unsettle => json!({"action":"unsettle"}),
                    ThreadOp::Archive => json!({"action":"archive"}),
                    ThreadOp::Unarchive => json!({"action":"unarchive"}),
                    ThreadOp::Pin => json!({"action":"pin"}),
                    ThreadOp::Unpin => json!({"action":"unpin"}),
                    ThreadOp::Rename { title } => json!({"action":"rename","title":title}),
                    ThreadOp::MarkRead => json!({"action":"mark_read"}),
                    ThreadOp::MarkUnread => json!({"action":"mark_unread"}),
                    ThreadOp::FollowUp { on, note } => json!({"action":"follow_up","on":on,"note":note}),
                    ThreadOp::Park { on } => json!({"action":"park","on":on}),
                    ThreadOp::Keep { on } => json!({"action":"keep","on":on}),
                };
                args["thread_id"] = json!(id);
                let applied = crate::control_lifecycle::update_threads(s, caller, &args).await?;
                if let Some(failed) = applied["outcomes"].as_array().and_then(|rows| rows.first()).filter(|row| row["ok"] == false) {
                    return Ok(json!({"target":id,"outcome":"failed","reason":failed["error"]}));
                }
                if matches!(item.op, ThreadOp::Settle) { effects = describe_settle(&applied["outcomes"][0]["result"]["side_effects"]); }
                ctrl = control::thread_doc(s, &id).map_err(|(_, body)| body.0["error"].clone())?;
                match &item.op {
                    ThreadOp::Unsettle => ctrl["manual_unsettle"] = json!(true),
                    ThreadOp::Settle => ctrl["manual_unsettle"] = json!(false),
                    ThreadOp::Pin if meta.is_settled() => ctrl["manual_unsettle"] = json!(true),
                    ThreadOp::Keep { on } => ctrl["kept_at"] = if *on { json!(chrono::Utc::now().timestamp()) } else { Value::Null },
                    ThreadOp::FollowUp { on: true, .. } if meta.follow_up.is_none() => ctrl["follow_up_since"] = json!(control::now()),
                    ThreadOp::Park { on: true } => ctrl["parked_at"] = json!(control::now()),
                    _ => {},
                }
                save(s, &id, &ctrl)?;
            }
            let mut result = json!({"target":id,"outcome":if dry { "would_apply" } else { "applied" }});
            if !effects.is_empty() { result["side_effects"] = json!(effects); }
            Ok::<_, Value>(result)
        }.await;
        results.push(match result {
            Ok(value) => value,
            Err(error) => json!({"target":id,"outcome":"failed","reason":reason(&error)}),
        });
    }
    Ok(json!({"dry_run":dry,"results":results}))
}

pub async fn run(s: &Arc<AppState>, caller: &str, p: ControlThreadParams) -> Result<Value, Value> {
    let mut results = Vec::new();
    for item in p.changes {
        let id = match check(s, caller, &item.thread_id) {
            Ok(id) => id,
            Err(error) => {
                results.push(
                    json!({"target":item.thread_id,"outcome":"skipped","reason":reason(&error)}),
                );
                continue;
            }
        };
        let result = async {
            let doc = chat::load(s, &id).map_err(|(_, b)| b.0["error"].clone())?;
            if doc["thread"]["state"]["lineage"]["native_subagent"].is_string() {
                return Err(error("provider-native mirror is read-only"));
            }
            let mut args = match item.op {
                RuntimeOperation::Runtime { set_runtime } => {
                    let mut args = serde_json::to_value(set_runtime).map_err(error)?;
                    args.as_object_mut().expect("runtime patch object").retain(|_, value| !value.is_null());
                    args["action"] = json!("set_runtime");
                    args
                }
                RuntimeOperation::Native(op) => match op {
                RunOp::Interrupt => json!({"action":"interrupt"}),
                RunOp::Retry { turn } => json!({"action":"retry","turn_id":turn}),
                RunOp::Compact => json!({"action":"compact"}),
                RunOp::SetModel { model } => json!({"action":"set_model","model":model}),
                RunOp::SetOption { id, value } => {
                    json!({"action":"set_option","option_id":id,"option_value":value})
                }
                RunOp::Fork => json!({"action":"fork"}),
                RunOp::ForkAt { turn } => {
                    let selected = doc["turns"]
                        .as_array()
                        .and_then(|turns| turns.get(turn))
                        .ok_or_else(|| error("fork_at turn is not present"))?;
                    json!({"action":"fork_at","turn_id":selected["turn_id"]})
                }
                RunOp::SteerQueued { id } => {
                    json!({"action":"steer_queued","queue_id":id.to_string()})
                }
                RunOp::DropQueued { id } => {
                    json!({"action":"drop_queued","queue_id":id.to_string()})
                }
                RunOp::Cancel => {
                    if doc["thread"]["state"]["lineage"]["parent_thread_id"] != caller {
                        return Err(error("only the thread's own subagent may be cancelled"));
                    }
                    json!({"action":"cancel"})
                }
                },
            };
            args["thread_id"] = json!(id);
            control::authorize_call(s, caller, "muteki_control_thread", &args)?;
            let output = crate::control_operate::control_thread(s, caller, &args).await?;
            if args["action"] == "interrupt" {
                let mut ctrl =
                    control::thread_doc(s, &id).map_err(|(_, b)| b.0["error"].clone())?;
                ctrl["pull_request_watches_paused"] = json!(true);
                save(s, &id, &ctrl)?;
            }
            Ok::<_, Value>(output)
        }
        .await;
        results.push(match result {
            Ok(output) => {
                let mut value = json!({"target":id,"outcome":"applied"});
                if let Some(created) = output["output"]["thread_id"].as_str() {
                    value["new_thread_id"] = json!(created);
                }
                value
            }
            Err(error) => json!({"target":id,"outcome":"failed","reason":reason(&error)}),
        });
    }
    Ok(json!({"results":results}))
}

pub fn record(s: &Arc<AppState>, caller: &str, p: RecordReviewParams) -> Result<Value, Value> {
    let id = p
        .request_id
        .clone()
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    let key = format!("{caller}:{id}");
    if let Some(existing) = s.store.get("control_reviews_v2", &key).map_err(error)? {
        return Ok(
            json!({"status":"recorded","id":existing["id"],"items":existing["items"].as_array().map(Vec::len),"coverage":existing["coverage"]}),
        );
    }
    if p.coverage.trim().is_empty() {
        return Err(error("coverage is required: say what the review looked at"));
    }
    for (index, item) in p.items.iter().enumerate() {
        crate::control_reader::checked_thread(s, caller, &item.thread_id)?;
        if ![
            "continue",
            "needs_decision",
            "waiting_on_others",
            "can_settle",
            "parked_or_superseded",
            "unsure",
        ]
        .contains(&item.group.as_str())
        {
            return Err(error(format!("item {index}: invalid review group")));
        }
        if !["clear_evidence", "inference", "unsure"].contains(&item.confidence.as_str()) {
            return Err(error(format!("item {index}: invalid confidence")));
        }
    }
    let count = p.items.len();
    let record = json!({"id":id,"thread_id":caller,"recorded_at":chrono::Utc::now().timestamp(),"coverage":p.coverage,"items":p.items});
    s.store
        .put("control_reviews_v2", &key, &record)
        .map_err(error)?;
    let mut records = s.store.entries("control_reviews_v2").map_err(error)?;
    records.sort_by(|a, b| {
        b.1["recorded_at"]
            .as_u64()
            .cmp(&a.1["recorded_at"].as_u64())
    });
    for (key, _) in records.into_iter().skip(50) {
        s.store.delete("control_reviews_v2", &key).map_err(error)?;
    }
    control::audit(s, caller, caller, "record_review", record.clone());
    Ok(json!({"status":"recorded","id":id,"items":count,"coverage":record["coverage"]}))
}
pub fn arm(s: &Arc<AppState>, caller: &str, p: SettleSelfParams) -> Result<Value, Value> {
    if let tcode_control_core::control::SelfSettleWhen::PrMerged { url }
    | tcode_control_core::control::SelfSettleWhen::PrClosed { url } = &p.when
    {
        if !crate::control_pr::links(s, caller)?.iter().any(|link| {
            link.visible() && link.url.trim_end_matches('/') == url.trim_end_matches('/')
        }) {
            return Err(error("link the pull request before arming settlement"));
        }
    }
    let mut ctrl = control::thread_doc(s, caller).map_err(|(_, b)| b.0["error"].clone())?;
    ctrl["canonical_self_settle"] = json!({"when":p.when,"consent_quote":p.consent_quote,"granted_at":chrono::Utc::now().timestamp()});
    save(s, caller, &ctrl)?;
    control::audit(
        s,
        caller,
        caller,
        "settle_self_armed",
        ctrl["canonical_self_settle"].clone(),
    );
    Ok(
        json!({"status":"armed","when":match &p.when{tcode_control_core::control::SelfSettleWhen::EndOfTurn=>"end of turn".to_owned(),tcode_control_core::control::SelfSettleWhen::PrMerged{url}=>format!("when {url} merges"),tcode_control_core::control::SelfSettleWhen::PrClosed{url}=>format!("when {url} closes")},"note":"The host settles this thread only after its current turn has ended. A new message from the user withdraws the grant, and the user can revoke it."}),
    )
}
pub async fn self_due(s: &Arc<AppState>, id: &str, turn_ended: bool) -> Result<(), Value> {
    let mut ctrl = control::thread_doc(s, id).map_err(|(_, b)| b.0["error"].clone())?;
    if ctrl["canonical_self_settle"].is_null() {
        return Ok(());
    }
    let grant: tcode_control_core::control::SelfSettleGrant =
        serde_json::from_value(ctrl["canonical_self_settle"].clone()).map_err(error)?;
    let doc = chat::load(s, id).map_err(|(_, b)| b.0["error"].clone())?;
    let blockers = control_state::blockers(&doc);
    if doc["thread"]["archived"] == true || crate::control_subagents::busy(&doc) || blockers.unsettled_children {
        return Ok(());
    }
    if doc["queue"]
        .as_array()
        .into_iter()
        .flatten()
        .any(|message| message["origin"] == "human")
    {
        ctrl["canonical_self_settle"] = Value::Null;
        return save(s, id, &ctrl);
    }
    let due = match grant.when {
        tcode_control_core::control::SelfSettleWhen::EndOfTurn => turn_ended,
        tcode_control_core::control::SelfSettleWhen::PrMerged { url } => {
            crate::control_pr::links(s, id)?.iter().any(|link| {
                link.url.trim_end_matches('/') == url.trim_end_matches('/')
                    && link.snapshot.as_ref().is_some_and(|snapshot| {
                        snapshot.state == tcode_control_core::pull_request::PullRequestState::Merged
                    })
            })
        }
        tcode_control_core::control::SelfSettleWhen::PrClosed { url } => {
            crate::control_pr::links(s, id)?.iter().any(|link| {
                link.url.trim_end_matches('/') == url.trim_end_matches('/')
                    && link.snapshot.as_ref().is_some_and(|snapshot| {
                        snapshot.state == tcode_control_core::pull_request::PullRequestState::Closed
                    })
            })
        }
    };
    if due {
        crate::control_lifecycle::settle_thread_now(s, "muteki-control", id, None).await?;
        ctrl = control::thread_doc(s, id).map_err(|(_, b)| b.0["error"].clone())?;
        ctrl["canonical_self_settle"] = Value::Null;
        save(s, id, &ctrl)?;
    }
    Ok(())
}
pub fn reference(s: &Arc<AppState>, caller: &str, p: ReferenceParams) -> Result<Value, Value> {
    let target = check(s, caller, &p.thread_id)?;
    let boundary = control::compute_boundary_notice(s, caller, &target).is_some();
    crate::control_observe::reference_thread(
        s,
        caller,
        &json!({"target_thread_id":target,"reason":p.reason}),
    )?;
    Ok(json!({"thread_id":target,"referenced":true,"boundary_notice":boundary}))
}

pub async fn respond_input(
    s: &Arc<AppState>,
    caller: &str,
    p: RespondInputParams,
) -> Result<Value, Value> {
    let id = check(s, caller, &p.thread_id)?;
    let doc = chat::load(s, &id).map_err(|(_, b)| b.0["error"].clone())?;
    let pending = &doc["thread"]["state"]["pending_user_input"];
    let request = pending["request_id"]
        .as_str()
        .ok_or_else(|| error("thread has no pending user-input request"))?;
    if p.question_id.as_ref().is_some_and(|id| id != request) {
        return Err(error("request is not pending in this thread"));
    }
    let questions = pending["questions"].as_array().cloned().unwrap_or_default();
    let ids: Vec<_> = questions
        .iter()
        .filter_map(|q| q["id"].as_str().or_else(|| q["question_id"].as_str()))
        .collect();
    if p.answers.keys().any(|id| !ids.contains(&id.as_str()))
        || ids.iter().any(|id| !p.answers.contains_key(*id))
    {
        return Err(error("answers must name every question exactly once"));
    }
    crate::control_operate::respond_input(
        s,
        caller,
        &json!({"thread_id":id,"request_id":request,"answers":p.answers,"decision":"submit"}),
    )
    .await?;
    Ok(json!({"thread_id":id,"request_id":request,"answered":true}))
}
pub async fn respond_approval(
    s: &Arc<AppState>,
    caller: &str,
    p: RespondApprovalParams,
) -> Result<Value, Value> {
    let id = check(s, caller, &p.thread_id)?;
    let doc = chat::load(s, &id).map_err(|(_, b)| b.0["error"].clone())?;
    if doc["thread"]["state"]["lineage"]["parent_thread_id"] != caller {
        return Err(error("only your own subagent's approvals may be answered"));
    }
    if control::settings(s)
        .map_err(|(_, b)| b.0["error"].clone())?
        .subagent_policy
        .approval_mode
        == "manual"
    {
        return Err(error("the user answers subagent approvals in manual mode"));
    }
    if p.cancel == p.option.is_some() {
        return Err(error("supply option or cancel: true, never both"));
    }
    let pending: Vec<_> = doc["thread"]["state"]["pending_approvals"]
        .as_object()
        .into_iter()
        .flat_map(|rows| rows.iter())
        .filter(|(_, row)| row["status"] == "pending")
        .collect();
    let (approval_id, row) = match p.approval_id.as_deref() {
        Some(id) => pending
            .iter()
            .copied()
            .find(|(key, _)| key.as_str() == id)
            .ok_or_else(|| error("approval is not pending"))?,
        None if pending.len() == 1 => pending[0],
        _ => {
            return Err(error(
                "approval_id is required unless exactly one approval is pending",
            ));
        }
    };
    if let Some(option) = &p.option {
        if !row["options"]
            .as_array()
            .into_iter()
            .flatten()
            .any(|row| row["option_id"] == *option || row["id"] == *option)
        {
            return Err(error("unknown native approval option"));
        }
    }
    let body = json!({"command_id":format!("control-approval:{}",uuid::Uuid::new_v4()),"command_type":"conversation.approval.resolve","payload":{"approval_id":approval_id,"decision":if p.cancel{"cancel"}else{"allow"},"option_id":p.option},"actor":{"kind":"agent","id":caller}});
    let _ = chat::command_from_platform(s.clone(), id.clone(), body, None)
        .await
        .map_err(|(_, b)| b.0["error"].clone())?;
    control::audit(
        s,
        caller,
        &id,
        "respond_approval",
        json!({"approval_id":approval_id}),
    );
    Ok(json!({"thread_id":id,"approval_id":approval_id,"answered":true}))
}
pub async fn create_project(
    s: &Arc<AppState>,
    caller: &str,
    p: CreateProjectParams,
) -> Result<Value, Value> {
    let root = std::path::Path::new(&p.root);
    if !root.is_absolute() || !root.is_dir() {
        return Err(error("root must be an existing absolute directory"));
    }
    let root = root.canonicalize().map_err(error)?;
    for workspace in s.store.list("workspaces").map_err(error)? {
        if workspace["root_path"] == root.to_string_lossy().as_ref() {
            let project_id = workspace["project_id"]
                .as_str()
                .ok_or_else(|| error("registered checkout has no project"))?;
            let project = s
                .store
                .get("projects", project_id)
                .map_err(error)?
                .ok_or_else(|| error("registered project is missing"))?;
            return Ok(
                json!({"project_id":project_id,"name":project["name"],"created":false,"root":root}),
            );
        }
    }
    let name = tcode_control_core::project::project_name_from_root(&root);
    let body = json!({"command_id":format!("control-project:{}",uuid::Uuid::new_v4()),"name":name,"root_path":root});
    let result =
        crate::workspaces::create_project(axum::extract::State(s.clone()), axum::Json(body))
            .await
            .map_err(|(_, b)| b.0["error"].clone())?
            .0;
    let id = result["receipt"]["aggregate"]["id"].clone();
    control::audit(
        s,
        caller,
        id.as_str().unwrap_or(""),
        "create_project",
        json!({"root":root}),
    );
    Ok(json!({"project_id":id,"name":name,"created":true,"root":root}))
}
pub async fn move_thread(
    s: &Arc<AppState>,
    caller: &str,
    p: MoveThreadParams,
) -> Result<Value, Value> {
    let id = check(s, caller, &p.thread_id)?;
    let root = crate::workspace_read_api::project_bound_root(s, &p.project_id)
        .map_err(|(_, body)| body.0["error"].clone())?;
    let relative = std::path::Path::new(p.cwd.as_deref().unwrap_or("."));
    if relative.is_absolute()
        || relative
            .components()
            .any(|part| matches!(part, std::path::Component::ParentDir))
    {
        return Err(error("cwd must be relative to the project root"));
    }
    let cwd = root.join(relative).canonicalize().map_err(error)?;
    if !cwd.starts_with(&root) || !cwd.is_dir() {
        return Err(error("cwd must stay inside the project"));
    }
    let guard = s.chat.active.lock().await;
    let mut doc = chat::load(s, &id).map_err(|(_, body)| body.0["error"].clone())?;
    if doc["thread"]["state"]["lineage"]["native_subagent"].is_string() {
        return Err(error("provider-native mirror is read-only"));
    }
    if doc["thread"]["state"]["lineage"]["isolation"] == "worktree" {
        return Err(error("a thread with a dedicated worktree cannot be moved"));
    }
    let blockers = control_state::blockers(&doc);
    if id != caller
        && (blockers.live_run
            || blockers.background_work
            || blockers.pending_approval
            || blockers.pending_input)
    {
        return Err(error(
            "busy: another thread cannot be moved while it is working or waiting for input",
        ));
    }
    let previous = crate::native_chat::session_cwd(s, &doc, &id).map_err(error)?;
    let changed = doc["thread"]["project_id"] != p.project_id || previous != cwd;
    let project = s
        .store
        .get("projects", &p.project_id)
        .map_err(error)?
        .ok_or_else(|| error("destination project is missing"))?;
    if changed {
        let workspace = s
            .store
            .list("workspaces")
            .map_err(error)?
            .into_iter()
            .find(|workspace| {
                workspace["project_id"] == p.project_id
                    && workspace["settings"]["mode"]
                        .as_str()
                        .is_none_or(str::is_empty)
            })
            .ok_or_else(|| error("destination project has no checkout"))?;
        doc["thread"]["project_id"] = json!(p.project_id);
        doc["thread"]["workspace_id"] = workspace["workspace_id"].clone();
        doc["runtime"]["cwd"] = json!(cwd);
        if !control_state::meta(s, &doc)?.provider.caps().session_move {
            doc["native_rebuild"] = json!({"reason":"project_moved","from":previous});
            doc["native_session"]["handoff_pending"] = json!(true);
        }
        chat::emit(
            s,
            &id,
            &mut doc,
            "core.thread.moved",
            json!({"project_id":p.project_id,"cwd":cwd,"from":previous}),
        );
        chat::save(s, &id, &mut doc).map_err(|(_, body)| body.0["error"].clone())?;
    }
    drop(guard);
    if changed {
        if !blockers.live_run {
            crate::native_chat::close(s, &id, "project_moved").await;
        }
        control::audit(
            s,
            caller,
            &id,
            "move_thread",
            json!({"project_id":p.project_id,"cwd":cwd}),
        );
    }
    Ok(json!({"thread_id":id,"project_id":p.project_id,"project_name":project["name"],"cwd":cwd}))
}

pub async fn delete_threads(
    s: &Arc<AppState>,
    caller: &str,
    p: DeleteThreadsParams,
) -> Result<Value, Value> {
    let mut results = Vec::new();
    let dry = p.dry_run.unwrap_or(false);
    for raw in p.thread_ids {
        let id = match check(s, caller, &raw) {
            Ok(id) => id,
            Err(error) => {
                results.push(json!({"target":raw,"outcome":"skipped","reason":reason(&error)}));
                continue;
            }
        };
        if id == caller {
            results.push(
                json!({"target":id,"outcome":"skipped","reason":"a thread cannot delete itself"}),
            );
            continue;
        }
        let docs = s.store.list(chat::COLLECTION).map_err(error)?;
        let mut tree: Vec<_> = docs
            .iter()
            .cloned()
            .filter(|doc| {
                let child = doc["thread"]["thread_id"].as_str().unwrap_or("");
                child == id || crate::chat_subagents::is_descendant_of(s, child, &id)
            })
            .collect();
        if tree
            .iter()
            .any(|doc| doc["thread"]["state"]["status"] == "running")
        {
            results.push(json!({"target":id,"outcome":"skipped","reason":"a thread in this subtree is running"}));
            continue;
        }
        if dry {
            results.push(json!({"target":id,"outcome":"would_apply","side_effects":[format!("delete {} threads",tree.len())]}));
            continue;
        }
        tree.sort_by_key(|doc| {
            std::cmp::Reverse(
                doc["thread"]["state"]["lineage"]["depth"]
                    .as_u64()
                    .unwrap_or(0),
            )
        });
        let mut failed = None;
        let mut removed = Vec::new();
        for doc in tree {
            let target = doc["thread"]["thread_id"].as_str().unwrap_or("");
            if p.remove_worktree.unwrap_or(false)
                && doc["thread"]["state"]["lineage"]["isolation"] == "worktree"
            {
                let workspace_id = doc["thread"]["workspace_id"].as_str().unwrap_or("");
                let shared = docs.iter().any(|other| {
                    other["thread"]["workspace_id"] == workspace_id
                        && other["thread"]["thread_id"] != target
                        && !crate::chat_subagents::is_descendant_of(
                            s,
                            other["thread"]["thread_id"].as_str().unwrap_or(""),
                            &id,
                        )
                });
                if !shared {
                    if let Some(workspace) =
                        s.store.get("workspaces", workspace_id).map_err(error)?
                    {
                        let checkout = crate::workspace_read_api::project_bound_root(
                            s,
                            doc["thread"]["project_id"].as_str().unwrap_or(""),
                        )
                        .map_err(|(_, body)| body.0["error"].clone())?;
                        let path = std::path::PathBuf::from(
                            workspace["root_path"]
                                .as_str()
                                .ok_or_else(|| error("worktree path is missing"))?,
                        );
                        match tokio::task::spawn_blocking(move || {
                            tcode_control_services::worktree::remove(&checkout, &path)
                        })
                        .await
                        .map_err(error)?
                        {
                            Ok(()) => {
                                s.store.delete("workspaces", workspace_id).map_err(error)?;
                            }
                            Err(cause) => {
                                failed = Some(error(cause));
                                break;
                            }
                        }
                    }
                }
            }
            match crate::control_lifecycle::delete_threads(
                s,
                caller,
                &json!({"thread_ids":[target]}),
            )
            .await
            {
                Ok(output)
                    if output["outcomes"]
                        .as_array()
                        .is_some_and(|rows| rows.iter().all(|row| row["ok"] == true)) =>
                {
                    removed.push(target.to_owned())
                }
                Ok(output) => {
                    failed = Some(output);
                    break;
                }
                Err(error) => {
                    failed = Some(error);
                    break;
                }
            }
        }
        results.push(json!({"target":id,"outcome":if failed.is_some(){"failed"}else{"applied"},"reason":failed,"deleted":removed}));
    }
    Ok(json!({"dry_run":dry,"results":results}))
}

pub async fn patch_settings(
    s: &Arc<AppState>,
    caller: &str,
    p: PatchSettingsParams,
) -> Result<Value, Value> {
    use tcode_control_core::settings::SettingsPatch;
    let kind = p.patch["type"].as_str().unwrap_or("");
    let content = p.patch["content"].clone();
    if kind == "thread_grant" || crate::workbench_settings::TARGETS.contains(&kind) {
        return crate::workbench_settings::update(s, caller, kind, content).await.map_err(|(_, body)| body.0);
    }
    if kind == "orchestration" {
        let mode = p
            .patch
            .get("mode")
            .or_else(|| content.get("mode"))
            .and_then(Value::as_str)
            .map(str::to_owned);
        let payload = if let Some(mode) = mode {
            let profile = if let Some(inner) = content.get("profile") {
                inner.clone()
            } else if content.is_null() {
                Value::Null
            } else {
                let mut cloned = content.clone();
                if let Some(obj) = cloned.as_object_mut() {
                    obj.remove("mode");
                }
                cloned
            };
            json!({"orchestration_mode": mode, "orchestration": profile})
        } else {
            json!({"orchestration": content})
        };
        return crate::workbench_settings::update(s, caller, "control", payload)
            .await
            .map_err(|(_, body)| body.0);
    }
    if kind == "ui" {
        return crate::control_lifecycle::patch_settings(s, caller, &json!({"patch":content})).await;
    }
    let patch: SettingsPatch = serde_json::from_value(p.patch).map_err(error)?;
    let current = crate::compat::get_ui(axum::extract::State(s.clone()))
        .await
        .map_err(|(_, b)| b.0)?;
    let mut values = current.0["values"].as_object().cloned().unwrap_or_default();
    match patch {
        SettingsPatch::SourceControlHost { host, kind, enabled, account } => {
            let mut source = crate::control_state::settings(s)?.source_control;
            let name = kind.authority(&host).map_err(|_| error("Invalid source control host"))?;
            let mut hosts = serde_json::to_value(&source.hosts).map_err(error)?;
            let row = hosts.as_object_mut().expect("hosts object").entry(name).or_insert_with(|| json!({"kind":kind,"enabled":true}));
            if let Some(value) = enabled { row["enabled"] = json!(value); }
            if let Some(value) = account { row["account"] = json!(value); }
            source.hosts = serde_json::from_value(hosts).map_err(error)?;
            return crate::workbench_settings::update(s, caller, "control_policy", json!({"source_control":source})).await.map_err(|(_, body)| body.0);
        }
        SettingsPatch::RemoveSourceControlHost { host } => {
            let mut source = crate::control_state::settings(s)?.source_control;
            source.hosts.remove(&host);
            return crate::workbench_settings::update(s, caller, "control_policy", json!({"source_control":source})).await.map_err(|(_, body)| body.0);
        }
        SettingsPatch::ResumeOnLimitReset(value) => {
            let current = crate::compat::get_conversation_preferences(axum::extract::State(s.clone()), axum::extract::Query(std::collections::HashMap::new())).await.map_err(|(_, body)| body.0)?.0;
            let (status, result) = crate::compat::put_conversation_preferences(axum::extract::State(s.clone()), axum::extract::Query(std::collections::HashMap::new()), axum::Json(json!({"version":current["version"],"auto_resume_on_quota_reset":value}))).await.map_err(|(_, body)| body.0)?;
            if !status.is_success() { return Err(result.0); }
            control::audit(s, caller, caller, "patch_settings", json!({"type":"resume_on_limit_reset"}));
            return Ok(result.0);
        }
        SettingsPatch::ControlEnabled(value) => {
            return crate::workbench_settings::update(s, caller, "control", json!({"enabled":value})).await.map_err(|(_, body)| body.0);
        }
        SettingsPatch::ControlDefaultGrant(value) => {
            return crate::workbench_settings::update(s, caller, "control", json!({"default_grant":value})).await.map_err(|(_, body)| body.0);
        }
        SettingsPatch::ControlProjectGrant { project_id, value } => {
            return crate::workbench_settings::update(s, caller, "control", json!({"project_id":project_id,"project_grant":value})).await.map_err(|(_, body)| body.0);
        }
        SettingsPatch::OrchestrateChildApproval(value) => {
            let mut policy = serde_json::to_value(control::settings(s).map_err(|(_, b)| b.0)?.subagent_policy).map_err(error)?;
            policy["approval_mode"] = json!(value);
            return crate::workbench_settings::update(s, caller, "control", json!({"subagent_policy":policy})).await.map_err(|(_, body)| body.0);
        }
        SettingsPatch::OrchestrateChildWorktrees(value) => {
            let mut policy = serde_json::to_value(control::settings(s).map_err(|(_, b)| b.0)?.subagent_policy).map_err(error)?;
            policy["child_worktrees"] = json!(value);
            return crate::workbench_settings::update(s, caller, "control", json!({"subagent_policy":policy})).await.map_err(|(_, body)| body.0);
        }
        SettingsPatch::AutoSettleAfterDays(value) => {
            return crate::workbench_settings::update(s, caller, "control_policy", json!({"auto_settle_after_days":value})).await.map_err(|(_, body)| body.0);
        }
        SettingsPatch::AutoSettleOnMerge(value) => {
            return crate::workbench_settings::update(s, caller, "control_policy", json!({"auto_settle_on_merge":value})).await.map_err(|(_, body)| body.0);
        }
        SettingsPatch::SettlementAutomation(value) => {
            return crate::workbench_settings::update(s, caller, "control_policy", json!({"settlement_automation":value})).await.map_err(|(_, body)| body.0);
        }
        SettingsPatch::ProjectSettlement { project_id, value } => {
            let mut overrides = crate::control_state::settings(s)?.project_settlement_overrides;
            match value { Some(value) => { overrides.insert(project_id, value); }, None => { overrides.remove(&project_id); } }
            return crate::workbench_settings::update(s, caller, "control_policy", json!({"project_settlement_overrides":overrides})).await.map_err(|(_, body)| body.0);
        }
        SettingsPatch::BrowserEnabled(value) => {
            crate::capabilities::set_enabled(s, "mcp", crate::capabilities::BROWSER_SERVER, value).map_err(|(_, body)| body.0)?;
            crate::native_chat::invalidate_all_capabilities(s).await;
            control::audit(s, caller, caller, "patch_settings", json!({"type":"browser_enabled"}));
            return Ok(json!({"status":"applied","enabled":value}));
        }
        SettingsPatch::BrowserHomeUrl(value) => {
            return crate::workbench_settings::update(s, caller, "browser", json!({"home_url":value.unwrap_or_default()})).await.map_err(|(_, body)| body.0);
        }
        SettingsPatch::BrowserAllowEvaluate(value) => {
            return crate::workbench_settings::update(s, caller, "browser", json!({"allow_evaluate":value})).await.map_err(|(_, body)| body.0);
        }
        SettingsPatch::SkipDeleteConfirmation(value) => {
            values.insert("skipDeleteConfirmation".into(), json!(value));
        }
        SettingsPatch::ProviderUpdateChecksDisabled(value) => {
            values.insert("providerUpdateChecksDisabled".into(), json!(value));
        }
        SettingsPatch::Language(language) => {
            values.insert("language".into(), json!(language.unwrap_or_else(|| "system".into())));
        }
        SettingsPatch::ThemeMode(theme) => {
            values.insert("theme".into(), serde_json::to_value(theme).map_err(error)?);
        }
        SettingsPatch::WordWrapDiffs(value) => {
            values.insert("diffWrap".into(), json!(value));
        }
        SettingsPatch::LiveCommandPanelDisabled(value) => {
            values.insert("liveCommandPanelDisabled".into(), json!(value));
        }
        SettingsPatch::SidebarProviderMarks(value) => {
            values.insert("sidebarModelIcon".into(), json!(value));
        }
        SettingsPatch::InactiveFrameThrottleDisabled(value) => {
            values.insert("inactiveFrameThrottleDisabled".into(), json!(value));
        }
        SettingsPatch::ScrollToLatestOnSend(value) => {
            values.insert("scrollToLatestOnSend".into(), json!(value));
        }
        SettingsPatch::ShortenLongCommands(value) => {
            values.insert("shortenLongCommands".into(), json!(value));
        }
        _ => {
            return Err(
                json!({"code":"control.settings_unsupported","category":"validation","message":"Unsupported typed patch in Muteki. Use type=ui, orchestration, control, control_policy, browser, agent_version_policy, competition_advisor or runner_listener; model/runtime/plugin settings use their platform tools."}),
            );
        }
    }
    let (status, result) = crate::compat::put_ui(
        axum::extract::State(s.clone()),
        axum::Json(json!({"version":current.0["version"],"values":values})),
    )
    .await
    .map_err(|(_, b)| b.0)?;
    if !status.is_success() {
        return Err(error("display settings changed concurrently"));
    }
    control::audit(s, caller, caller, "patch_settings", result.0);
    Ok(json!({"status":"applied"}))
}
