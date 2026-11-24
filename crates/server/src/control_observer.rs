//! Read-only tcode_control projections over the host's actual state.
use crate::{AppState, chat, control, control_contract::*, control_state};
use serde_json::{Value, json};
use std::sync::Arc;
use tcode_control_core::{
    control::SettlementAutomation,
    project::SessionMeta,
    settlement::{ThreadActivity, automatic_settlement_due},
};

fn error(message: impl ToString) -> Value {
    json!({"code":"control.read_failed","message":message.to_string()})
}
fn id(doc: &Value) -> &str {
    doc["thread"]["thread_id"].as_str().unwrap_or("")
}
fn state(meta: &SessionMeta) -> ThreadState {
    if meta.archived_at.is_some() {
        ThreadState::Archived
    } else if meta.is_settled() {
        ThreadState::Settled
    } else {
        ThreadState::Active
    }
}
fn scope(
    s: &AppState,
    caller: &str,
    wanted: Option<tcode_control_core::control::ControlScope>,
) -> Result<(control::Grant, control::Scope, String), Value> {
    let grant = control::Grant::standard();
    let requested = match wanted {
        Some(value) => {
            serde_json::from_value(serde_json::to_value(value).map_err(error)?).map_err(error)?
        }
        None => control::Scope::Host,
    };
    let own = chat::load(s, caller).map_err(|(_, b)| b.0["error"].clone())?["thread"]["project_id"]
        .as_str()
        .unwrap_or("")
        .to_owned();
    Ok((grant, requested, own))
}
pub fn scoped(
    s: &AppState,
    caller: &str,
    wanted: Option<tcode_control_core::control::ControlScope>,
) -> Result<Vec<Value>, Value> {
    scoped_with_checkpoint(s, caller, wanted, &|| Ok(()))
}

fn scoped_with_checkpoint(
    s: &AppState,
    caller: &str,
    wanted: Option<tcode_control_core::control::ControlScope>,
    checkpoint: &impl Fn() -> Result<(), Value>,
) -> Result<Vec<Value>, Value> {
    let (_, scope, own) = scope(s, caller, wanted)?;
    let docs = s.store.list(chat::COLLECTION).map_err(error)?;
    let ancestors = control::ancestor_restrictions(s, caller)?;
    let mut visible = Vec::new();
    for doc in docs {
        checkpoint()?;
        let tid = id(&doc);
        if !ancestors.contains(tid)
            && (tid == caller
                || (!chat::is_ctf_thread(&doc["thread"]) && !chat::is_side_chat(&doc["thread"])))
            && scope.reaches(&own, doc["thread"]["project_id"].as_str().unwrap_or(""))
            && control::src_isolation_allows(s, caller, tid)
        {
            visible.push(doc);
        }
    }
    Ok(visible)
}
pub fn projects(s: &AppState, caller: &str) -> Result<Value, Value> {
    let (_, scope, own) = scope(s, caller, None)?;
    let threads = scoped(s, caller, None)?;
    let workspaces = s.store.list("workspaces").map_err(error)?;
    let mut rows = Vec::new();
    for project in s.store.list("projects").map_err(error)? {
        let pid = project["project_id"].as_str().unwrap_or("");
        if !scope.reaches(&own, pid) {
            continue;
        }
        let mut counts = json!({"active":0,"settled":0,"archived":0});
        for doc in threads
            .iter()
            .filter(|doc| doc["thread"]["project_id"] == pid)
        {
            let meta = control_state::meta(s, doc)?;
            let kind = serde_json::to_value(state(&meta)).map_err(error)?;
            let key = kind.as_str().unwrap();
            counts[key] = json!(counts[key].as_u64().unwrap_or(0) + 1);
        }
        let root = workspaces
            .iter()
            .find(|workspace| {
                workspace["project_id"] == pid
                    && workspace["settings"]["mode"]
                        .as_str()
                        .is_none_or(str::is_empty)
            })
            .map(|workspace| workspace["root_path"].clone())
            .unwrap_or(Value::Null);
        rows.push(json!({"id":pid,"name":project["name"],"root":root,"quick_chat":project["quick_chat"].as_bool().unwrap_or(false),"threads":counts}));
    }
    Ok(json!({"projects":rows}))
}
fn row(doc: &Value, meta: &SessionMeta, activity: &ThreadActivity) -> Value {
    let runtime = &doc["runtime"];
    let engine = runtime["adapter_id"].as_str()
        .and_then(crate::agent_runtime::descriptors::for_adapter)
        .map(|(engine, _)| engine.engine());
    let routing = &doc["thread"]["routing"];
    let role = routing.get("role").unwrap_or(&doc["thread"]["state"]["lineage"]["role"]);
    let runtime = json!({"engine":engine,"adapter_id":runtime["adapter_id"],
        "instance_id":runtime["instance_id"],"profile":runtime["credential_id"],
        "model":runtime["model"],"effort":runtime["effort"]});
    let session = &doc["native_session"];
    let runtime_session = session.is_object().then(|| json!({
        "engine":session["engine"],"adapter_id":session["adapter_id"],
        "instance_id":session["instance_id"],"profile":session["credential_id"],
        "model":session["model"],"open":session["open"],"started_at":session["started_at"],
        "provider_session_id":session["provider_session_id"]}));
    let prs:Vec<_>=meta.pull_requests.iter().filter(|link|link.visible()).map(|link|json!({"key":format!("{}/{}#{}",link.key.host,link.key.repository,link.key.number),"url":link.url,"state":link.snapshot.as_ref().map(|snapshot|snapshot.state)})).collect();
    json!({"id":meta.id,"title":meta.title,"project_id":meta.project_id,"provider":engine,"engine":engine,"adapter_id":runtime["adapter_id"],"instance_id":runtime["instance_id"],"profile_id":runtime["profile"],"acp_agent_id":meta.acp_agent_id,"model":runtime["model"],"effort":runtime["effort"],"role":role,"routing":routing,"runtime":runtime,"runtime_session":runtime_session,"state":state(meta),"settled_at":meta.settled_at,"archived_at":meta.archived_at,"pinned":meta.pinned_at.is_some(),"follow_up":meta.follow_up,"parked":meta.parked_at.is_some(),"last_activity_at":activity.last_activity_at(),"last_human_message_at":activity.last_human_message_at,"parent_session_id":meta.parent_session_id,"pull_requests":prs,"started_by":meta.started_by})
}
pub async fn threads(
    s: &Arc<AppState>,
    caller: &str,
    p: ListThreadsParams,
) -> Result<Value, Value> {
    let states = p
        .states
        .unwrap_or_else(|| vec![ThreadState::Active, ThreadState::Settled]);
    let title = p.title_query.map(|title| title.to_lowercase());
    let mut matched = Vec::new();
    for doc in scoped(s, caller, p.scope)? {
        let meta = control_state::meta(s, &doc)?;
        let activity = control_state::activity(&doc);
        let at = activity
            .last_activity_at()
            .unwrap_or(meta.created_at * 1000);
        if !states.contains(&state(&meta))
            || (!p.include_children.unwrap_or(false) && meta.parent_session_id.is_some())
            || p.follow_up.is_some_and(|on| meta.follow_up.is_some() != on)
            || title
                .as_ref()
                .is_some_and(|title| !meta.title.to_lowercase().contains(title))
            || p.active_since.is_some_and(|since| at < since)
            || p.active_before.is_some_and(|before| at >= before)
        {
            continue;
        }
        matched.push((at, doc, meta, activity));
    }
    matched.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.2.id.cmp(&b.2.id)));
    let total = matched.len();
    let after = p
        .cursor
        .as_ref()
        .map(|cursor| {
            let (at, id) = cursor
                .split_once(':')
                .ok_or_else(|| error("invalid cursor"))?;
            Ok::<_, Value>((at.parse::<u64>().map_err(error)?, id.to_owned()))
        })
        .transpose()?;
    let page: Vec<_> = matched
        .into_iter()
        .filter(|(at, _, meta, _)| {
            after.as_ref().is_none_or(|(cursor_at, id)| {
                *at < *cursor_at || (*at == *cursor_at && meta.id > *id)
            })
        })
        .collect();
    let limit = p.limit.unwrap_or(50).clamp(1, 100) as usize;
    let next = if page.len() > limit {
        page.get(limit - 1)
            .map(|(at, _, meta, _)| format!("{at}:{}", meta.id))
    } else {
        None
    };
    let mut rows = Vec::new();
    for (_, doc, meta, activity) in page.into_iter().take(limit) {
        let mut item = row(&doc, &meta, &activity);
        if p.include_worktree.unwrap_or(false) {
            item["worktree"] = worktree(s, caller, &doc, &meta, true).await?;
        }
        rows.push(item);
    }
    Ok(json!({"threads":rows,"total":total,"next_cursor":next,"omitted":[]}))
}
async fn worktree(
    s: &Arc<AppState>,
    _caller: &str,
    doc: &Value,
    meta: &SessionMeta,
    dirty: bool,
) -> Result<Value, Value> {
    let dedicated = doc["thread"]["state"]["lineage"]["isolation"] == "worktree";
    let mut value =
        json!({"cwd":meta.cwd,"kind":if dedicated{"dedicated"}else{"checkout"},"dirty":null});
    if dedicated {
        let workspace = s
            .store
            .get(
                "workspaces",
                doc["thread"]["workspace_id"].as_str().unwrap_or(""),
            )
            .map_err(error)?;
        value["branch"] = workspace
            .as_ref()
            .map(|workspace| workspace["settings"]["branch"].clone())
            .unwrap_or(Value::Null);
    }
    if dirty {
        let cwd = meta.cwd.clone();
        let bound = crate::control_git::workspace_root(s, doc)?;
        match tokio::task::spawn_blocking(move || {
            crate::control_git::repository_boundary(&cwd, &bound)?;
            tcode_control_services::git::read_status(&cwd)
        })
            .await
            .map_err(error)?
        {
            Ok(status) => {
                value["dirty"] = if status.is_repo {
                    json!(status.has_working_tree_changes)
                } else {
                    Value::Null
                }
            }
            Err(error) => value["git_error"] = json!(error),
        }
    }
    Ok(value)
}
pub fn explanation(
    s: &AppState,
    doc: &Value,
    meta: &SessionMeta,
    activity: &ThreadActivity,
) -> Result<Value, Value> {
    let settings = control_state::settings(s)?;
    let project = meta
        .project_id
        .as_ref()
        .and_then(|id| settings.project_settlement_overrides.get(id));
    let project_days = project.and_then(|value| value.auto_settle_after_days);
    let days = project_days.unwrap_or(settings.auto_settle_after_days);
    let project_merge = project.and_then(|value| value.auto_settle_on_merge);
    let on_merge = project_merge.unwrap_or(settings.auto_settle_on_merge);
    let blockers = control_state::blockers(doc);
    let due = automatic_settlement_due(
        meta,
        activity,
        blockers,
        chrono::Utc::now().timestamp_millis() as u64,
        days,
        on_merge,
    );
    let mut reasons = Vec::new();
    use settlement_reason as reason;
    for (hold, key) in [
        (meta.archived_at.is_some(), reason::ARCHIVED),
        (meta.is_settled(), reason::SETTLED),
        (
            meta.settled_override == Some(tcode_control_core::project::SettledOverride::Active),
            reason::OVERRIDE_ACTIVE,
        ),
        (meta.auto_settle_disabled_at.is_some(), reason::KEEP),
        (meta.pinned_at.is_some(), reason::PINNED),
        (meta.parent_session_id.is_some(), reason::ORCHESTRATE_CHILD),
        (
            settings.settlement_automation == SettlementAutomation::Off,
            reason::AUTOMATION_OFF,
        ),
        (
            settings.settlement_automation == SettlementAutomation::Suggest,
            reason::AUTOMATION_SUGGEST_ONLY,
        ),
        (meta.follow_up.is_some(), reason::FOLLOW_UP),
        (meta.parked_at.is_some(), reason::PARKED),
        (blockers.pending_approval, reason::PENDING_APPROVAL),
        (blockers.pending_input, reason::PENDING_INPUT),
        (blockers.live_run, reason::LIVE_RUN),
        (blockers.background_work, reason::BACKGROUND_WORK),
        (blockers.unsettled_children, reason::UNSETTLED_CHILDREN),
        (
            blockers.pending_human_message,
            reason::PENDING_HUMAN_MESSAGE,
        ),
        (
            blockers.pending_agent_message,
            reason::PENDING_AGENT_MESSAGE,
        ),
        (blockers.scheduled_wake, reason::SCHEDULED_WAKE),
    ] {
        if hold {
            reasons.push(key);
        }
    }
    if reasons.is_empty() && due.is_none() {
        reasons.push(if activity.last_activity_at().is_none() {
            reason::ACTIVITY_UNKNOWN
        } else if project_days.is_some() {
            reason::PROJECT_TIME_RULE_OFF
        } else {
            reason::TIME_RULE_OFF
        });
    }
    Ok(
        json!({"would_auto_settle":reasons.is_empty(),"due_at":due.filter(|_|reasons.is_empty()).map(|due|due.due_at),"reasons":reasons,"rule":{"after_days":days,"after_days_source":if project_days.is_some(){"project"}else{"settings"},"on_merge":on_merge,"on_merge_source":if project_merge.is_some(){"project"}else{"settings"}}}),
    )
}
pub async fn thread(s: &Arc<AppState>, caller: &str, p: GetThreadParams) -> Result<Value, Value> {
    let id = control::resolve_thread_id(caller, &p.thread_id);
    let (grant, _) = control::effective(s, caller).map_err(|(_, b)| b.0["error"].clone())?;
    control::check_target(s, caller, id, &grant)?;
    let doc = chat::load(s, id).map_err(|(_, b)| b.0["error"].clone())?;
    let meta = control_state::meta(s, &doc)?;
    let activity = control_state::activity(&doc);
    let mut out = row(&doc, &meta, &activity);
    out["worktree"] = worktree(s, caller, &doc, &meta, false).await?;
    out["settlement"] = explanation(s, &doc, &meta, &activity)?;
    let blockers = control_state::blockers(&doc);
    let ctrl = control::thread_doc(s, id).map_err(|(_, b)| b.0["error"].clone())?;
    let children = doc["thread"]["state"]["subagents"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let unsettled = children
        .iter()
        .filter(|child| child["delivery"] == "awaiting_settle")
        .count();
    let agents = children
        .iter()
        .filter(|child| child["delivery"] == "running")
        .count();
    out["activity"] = json!({"working":blockers.live_run||blockers.background_work||agents>0,"turn_running":blockers.live_run,"background":{"tasks":doc["native_session"]["background_tasks"].as_u64().unwrap_or(0),"agents":agents},"unsettled":unsettled,"waiting_for_approval":blockers.pending_approval,"waiting_for_input":blockers.pending_input,"settle_blocked":crate::control_subagents::busy(&doc)||blockers.unsettled_children||blockers.pending_approval||blockers.pending_human_message||(blockers.pending_input&&doc["thread"]["state"]["pending_user_input"]["native"]["background"]!=true),"failed":activity.failed,"unread":control::read_lifecycle(&ctrl).unread,"fork":if meta.native_subagent.is_some(){"unsupported"}else if blockers.live_run{"running"}else if doc["turns"].as_array().is_none_or(Vec::is_empty){"empty"}else{"available"},"agent":if meta.parent_session_id.is_some(){json!({"execution":crate::control_subagents::execution(&doc,doc["thread"]["state"]["status"].as_str().unwrap_or("idle")),"delivery":crate::control_subagents::delivery(&doc,doc["thread"]["state"]["status"].as_str().unwrap_or("idle")),"report":doc["thread"]["state"]["subagent_report"]["state"].as_str().unwrap_or("none"),"run_started_at":activity.last_run_started_at,"run_completed_at":activity.last_run_completed_at})}else{Value::Null},"wakes":{"scheduled_at":doc["queue"].as_array().into_iter().flatten().filter_map(|item|item["not_before"].as_u64()).min(),"pull_requests":meta.pull_requests.iter().filter(|link|link.visible()&&link.watch.is_some()&&!meta.pull_request_watches_paused).map(|link|&link.key).collect::<Vec<_>>(),"notify":0}});
    let mut queue = json!({"human":0,"agent":0,"server":0,"scheduled":0});
    for item in doc["queue"].as_array().into_iter().flatten() {
        if item["not_before"].is_number() {
            queue["scheduled"] = json!(queue["scheduled"].as_u64().unwrap_or(0) + 1);
        }
        let origin = item["origin"].as_str().unwrap_or("human");
        if let Some(count) = queue[origin].as_u64() {
            queue[origin] = json!(count + 1);
        }
    }
    out["queue"] = queue;
    out["pending_approvals"]=doc["thread"]["state"]["pending_approvals"].as_object().into_iter().flatten().filter(|(_,row)|row["status"]=="pending").map(|(id,row)|json!({"id":id,"kind":row["kind"],"options":row["options"].as_array().into_iter().flatten().map(|option|json!({"id":option["option_id"],"label":option["label"],"kind":option["kind"]})).collect::<Vec<_>>()})).collect::<Vec<_>>().into();
    let pending = &doc["thread"]["state"]["pending_user_input"];
    out["pending_input"] = if pending.is_object() {
        json!({"request_id":pending["request_id"],"questions":pending["questions"].as_array().map(Vec::len).unwrap_or(0)})
    } else {
        Value::Null
    };
    let (ctrl_grant, source) = control::effective(s, id).map_err(|(_, b)| b.0["error"].clone())?;
    out["control"] = json!({"grant":ctrl_grant,"source":source,"requests":ctrl["requests"],"references":meta.references,"self_settle":meta.self_settle});
    out["lineage"] = json!({"parent_session_id":meta.parent_session_id,"children":doc["thread"]["state"]["subagents"].as_array().into_iter().flatten().filter_map(|child|child["thread_id"].as_str()).collect::<Vec<_>>(),"pending_fork":meta.pending_fork.is_some(),"forked_from":meta.fork_origin.map(|origin|origin.thread_id),"imported_from":meta.imported_from,"native_subagent":meta.native_subagent.is_some()});
    out["report"] = doc["thread"]["state"]["subagent_report"]["state"].clone();
    out["terminals"] = json!(crate::control_terminal::list(s, id)?);
    out["omitted"] =
        json!(["worktree.dirty: read it live with get_git or list_threads include_worktree"]);
    Ok(out)
}

pub async fn search(s: &Arc<AppState>, caller: &str, p: SearchParams) -> Result<Value, Value> {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    const SEARCH_DEADLINE: Duration = Duration::from_secs(175);

    struct StopOnDrop(Arc<AtomicBool>);
    impl Drop for StopOnDrop {
        fn drop(&mut self) {
            self.0.store(true, Ordering::Release);
        }
    }

    let state = s.clone();
    let caller = caller.to_owned();
    let stopped = Arc::new(AtomicBool::new(false));
    let _stop_on_drop = StopOnDrop(stopped.clone());
    let worker = tokio::task::spawn_blocking(move || {
        let checkpoint = || {
            if stopped.load(Ordering::Acquire) {
                Err(json!({"code":"control.search_cancelled","message":"Conversation search was cancelled"}))
            } else {
                Ok(())
            }
        };
        let result = checkpoint().and_then(|()| search_records(&state, &caller, p, &checkpoint));
        if stopped.load(Ordering::Acquire) {
            tracing::debug!("Conversation search worker stopped after cancellation");
        }
        result
    });
    tokio::time::timeout(SEARCH_DEADLINE, worker)
        .await
        .map_err(|_| json!({
            "code":"control.search_timeout",
            "message":format!("Conversation search exceeded its {}-second limit", SEARCH_DEADLINE.as_secs())
        }))?
        .map_err(error)?
}

fn search_records(
    state: &AppState,
    caller: &str,
    p: SearchParams,
    checkpoint: &impl Fn() -> Result<(), Value>,
) -> Result<Value, Value> {
    use tcode_control_services::session_search::{SearchKind, extract_searchable_entries};

    let query = p.query.trim();
    if query.is_empty() {
        return Err(error("search needs a non-empty query"));
    }
    let offset = p.cursor.as_deref().map(str::parse::<usize>).transpose().map_err(error)?.unwrap_or(0);
    let limit = p.limit.unwrap_or(20).clamp(1, 50) as usize;
    let lower_query = query.to_lowercase();
    let mut omitted = vec![
        "neighbouring entries: read around a hit's anchor with read from/to".to_owned(),
        "command and tool output is not indexed; use search_in_item".to_owned(),
    ];
    let kinds = p.kinds.map(|kinds| {
        kinds.into_iter().filter_map(|kind| Some(match kind {
            ItemKind::UserMessage => SearchKind::UserMessage,
            ItemKind::AssistantMessage => SearchKind::AssistantMessage,
            ItemKind::Command => SearchKind::CommandExecution,
            ItemKind::FileChange => SearchKind::FileChange,
            ItemKind::ToolCall => SearchKind::ToolCall,
            ItemKind::Steer => SearchKind::Steer,
            ItemKind::Reasoning => {
                omitted.push("reasoning is not indexed and was not searched".into());
                return None;
            }
            _ => SearchKind::Other,
        })).collect::<Vec<_>>()
    });
    let mut docs = scoped_with_checkpoint(state, caller, p.scope, checkpoint)?;
    docs.retain(|doc| p.thread_ids.as_ref().is_none_or(|ids| {
        ids.iter().any(|id| control::resolve_thread_id(caller, id) == self::id(doc))
    }));
    docs.sort_by_cached_key(|doc| (std::cmp::Reverse(control_state::activity(doc).last_activity_at()), id(doc).to_owned()));

    let mut hits = Vec::new();
    let mut total = 0;
    let mut per_thread = std::collections::BTreeMap::<String, usize>::new();
    let mut per_turn = std::collections::BTreeMap::<(String, usize), usize>::new();
    let mut redacted = 0;
    let grant = control::Grant::standard();
    for doc in docs {
        checkpoint()?;
        let id = self::id(&doc);
        if control::check_target(state, caller, id, &grant).is_err() {
            continue;
        }
        let (_, log, notes) = match crate::control_log::records_with_checkpoint(state, id, checkpoint) {
            Ok(log) => log,
            Err(error) => {
                checkpoint()?;
                omitted.push(format!("a thread that could not be searched: {id}: {error}"));
                continue;
            }
        };
        omitted.extend(notes);
        for entry in extract_searchable_entries(&log) {
            checkpoint()?;
            if kinds.as_ref().is_some_and(|kinds| !kinds.contains(&entry.kind)) {
                continue;
            }
            let Some(snippet) = crate::control_search_text::snippet(&entry.text, &lower_query, 140) else {
                continue;
            };
            *per_thread.entry(id.to_owned()).or_default() += 1;
            *per_turn.entry((id.to_owned(), entry.turn)).or_default() += 1;
            let (snippet, count) = crate::control_index::redact(&snippet);
            redacted += count;
            if total >= offset && hits.len() < limit {
                let anchor = format!("{id}#{}", entry.position);
                hits.push(json!({"anchor":anchor,"thread_id":id,"title":doc["thread"]["title"],"turn":entry.turn,"kind":entry.kind,"snippet":crate::control_index::fence(&anchor,&snippet)}));
            }
            total += 1;
        }
    }
    checkpoint()?;
    let next = (offset.saturating_add(hits.len()) < total).then(|| (offset + hits.len()).to_string());
    if per_turn.len() > 200 {
        omitted.push(format!("per_turn lists the first 200 of {} turns with hits", per_turn.len()));
    }
    Ok(json!({
        "query":query,"hits":hits,
        "per_thread":per_thread.into_iter().map(|(thread_id,hits)|json!({"thread_id":thread_id,"hits":hits})).collect::<Vec<_>>(),
        "per_turn":per_turn.into_iter().take(200).map(|((thread_id,turn),hits)|json!({"thread_id":thread_id,"turn":turn,"hits":hits})).collect::<Vec<_>>(),
        "total":total,"truncated":next.is_some(),"next_cursor":next,"redacted":redacted,"omitted":omitted
    }))
}

pub async fn review(
    s: &Arc<AppState>,
    caller: &str,
    p: ReviewCandidatesParams,
) -> Result<Value, Value> {
    let now = chrono::Utc::now().timestamp_millis() as u64;
    let since = p.since.unwrap_or(now.saturating_sub(7 * 86_400_000));
    let mut docs = scoped(s, caller, p.scope)?;
    docs.retain(|doc| doc["thread"]["archived"] != true);
    let total = docs.len();
    docs.sort_by(|a, b| {
        control_state::activity(b)
            .last_activity_at()
            .cmp(&control_state::activity(a).last_activity_at())
    });
    let mut candidates = Vec::new();
    let mut collapsed = Vec::new();
    let mut without = 0;
    let mut redacted = 0;
    let mut omitted = Vec::new();
    let mut cwds = std::collections::HashSet::new();
    for doc in docs {
        let meta = control_state::meta(s, &doc)?;
        cwds.insert(meta.cwd.clone());
        let activity = control_state::activity(&doc);
        let at = activity
            .last_activity_at()
            .unwrap_or(meta.created_at * 1000);
        let reason = if meta.parent_session_id.is_some() {
            Some("orchestrate_child")
        } else if doc["thread"]["state"]["status"] == "running" {
            Some("running")
        } else if now.saturating_sub(at) < 600_000 {
            Some("just_active")
        } else {
            None
        };
        if let Some(reason) = reason {
            collapsed.push(json!({"id":meta.id,"title":meta.title,"reason":reason,"parent_session_id":meta.parent_session_id}));
            continue;
        }
        if meta.is_settled() && at < since {
            continue;
        }
        let mut item = row(&doc, &meta, &activity);
        item["worktree"] = worktree(s, caller, &doc, &meta, true).await?;
        item["settlement"] = explanation(s, &doc, &meta, &activity)?;
        let mut last = Value::Null;
        match crate::control_log::load(s, &meta.id) {
            Ok((_, log, notes)) => {
                omitted.extend(notes);
                let timeline = log.timeline();
                if !timeline.turns.is_empty() {
                    let message =
                        timeline
                            .entries
                            .iter()
                            .rev()
                            .find_map(|entry| match &entry.content {
                                tcode_control_core::session::EntryContent::Item(
                                    tcode_control_agent::ItemContent::AssistantMessage {
                                        text, ..
                                    },
                                ) => Some(text),
                                _ => None,
                            });
                    let text = message.map(|text| {
                        let (clean, count) = crate::control_index::redact(text);
                        redacted += count;
                        crate::control_index::fence(
                            &format!("{} last assistant message", meta.id),
                            &clean,
                        )
                    });
                    last = json!({"status":timeline.last_turn_status,"assistant_message":text});
                }
            }
            Err(error) => omitted.push(format!(
                "last turn of {} could not be read: {error}",
                meta.id
            )),
        }
        if activity.last_activity_at().is_none() || last.is_null() {
            without += 1;
        }
        let holds = control_state::blockers(&doc);
        item["last_turn"] = last;
        item["pending_approval"] = json!(holds.pending_approval);
        item["pending_input"] = json!(holds.pending_input);
        item["manual_unsettle"] = json!(
            meta.settled_override == Some(tcode_control_core::project::SettledOverride::Active)
        );
        item["idle_days"] = if !meta.is_settled() && activity.last_activity_at().is_some() {
            json!(now.saturating_sub(at) / 86_400_000)
        } else {
            Value::Null
        };
        candidates.push(item);
    }
    let projects = projects(s, caller)?["projects"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let mut unattributed = Vec::new();
    for project in &projects {
        if let Some(root) = project["root"].as_str() {
            let path = std::path::PathBuf::from(root);
            if !cwds.contains(&path) {
                let target = path.clone();
                let dirty = tokio::task::spawn_blocking(move || {
                    crate::control_git::repository_boundary(&target, &target)?;
                    tcode_control_services::git::read_status(&target)
                })
                .await
                .map_err(error)?;
                if dirty.is_ok_and(|status| status.is_repo && status.has_working_tree_changes) {
                    unattributed.push(json!({"project_id":project["id"],"root":path}));
                }
            }
        }
    }
    if collapsed.len() > 100 {
        omitted.push(format!(
            "{} more collapsed threads are not listed; list_threads finds them",
            collapsed.len() - 100
        ));
        collapsed.truncate(100);
    }
    let mut used = 0;
    let mut left = Vec::new();
    candidates.retain(|item| {
        let size = serde_json::to_vec(item)
            .map(|bytes| bytes.len())
            .unwrap_or(0);
        if used + size > 65536 {
            left.push(item["id"].clone());
            false
        } else {
            used += size;
            true
        }
    });
    if !left.is_empty() {
        omitted.push(format!(
            "{} candidates did not fit the response budget; get_thread each of: {}",
            left.len(),
            json!(left)
        ));
    }
    Ok(
        json!({"candidates":candidates,"collapsed":collapsed,"unattributed_dirty_worktrees":unattributed,"coverage":{"projects":projects.len(),"threads":total,"without_enough_data":without,"window_since":since},"redacted":redacted,"omitted":omitted}),
    )
}
