//! Pull-request links, host reads and watches use the pinned upstream Forge boundary.
use crate::{
    AppState, chat, control,
    control_contract::{GetPullRequestParams, PullRequestOp, PullRequestParams},
};
use serde_json::{Value, json};
use std::{path::Path, sync::Arc};
use tcode_control_core::{
    pull_request::{
        self, PullRequestKey, PullRequestSource, PullRequestState, PullRequestSyncError,
        ThreadPullRequestLink,
    },
    pull_request_watch::{self as watch, PendingWake, PullRequestWatch},
};
use tcode_control_services::forge::{Checkout, Forge, ForgeError, ForgeErrorKind};
fn error(message: impl ToString) -> Value {
    json!({"code":"control.pull_request_failed","message":message.to_string()})
}
fn host_error(error: ForgeError) -> Value {
    let code = match error.kind {
        ForgeErrorKind::HostDisabled => "pull_request_host_disabled",
        ForgeErrorKind::NoCredential | ForgeErrorKind::Unauthorized => "pull_request_no_credential",
        ForgeErrorKind::RateLimited { .. } | ForgeErrorKind::Paused { .. } => {
            "pull_request_rate_limited"
        }
        ForgeErrorKind::NotFound => "pull_request_not_found",
        ForgeErrorKind::TooLarge => "pull_request_too_large",
        ForgeErrorKind::InvalidInput => "pull_request_invalid_read",
        ForgeErrorKind::UnknownKind => "pull_request_unknown_host",
        _ => "pull_request_failed",
    };
    json!({"code":code,"message":error.to_string()})
}
pub fn host(s: &AppState) -> Result<Arc<dyn Forge>, Value> {
    if let Some(host) = s.control_forge.get() {
        return Ok(host.clone());
    }
    let root = crate::host_process::configured_state_dir(s)
        .ok_or_else(|| error("Host state directory is missing"))?
        .join("control-hosts");
    std::fs::create_dir_all(&root).map_err(error)?;
    let store = tcode_control_services::settings::SettingsStore::new(root);
    let host = tcode_control_services::forge::connect(store, std::env::vars());
    host.configure(crate::control_state::settings(s)?.source_control.hosts);
    let _ = s.control_forge.set(host.clone());
    Ok(s.control_forge.get().cloned().unwrap_or(host))
}
pub fn label(key: &PullRequestKey) -> String {
    format!("{}/{}#{}", key.host, key.repository, key.number)
}
pub fn links(s: &AppState, id: &str) -> Result<Vec<ThreadPullRequestLink>, Value> {
    let doc = control::thread_doc(s, id).map_err(|(_, b)| b.0["error"].clone())?;
    serde_json::from_value(
        doc["pull_requests"]
            .as_array()
            .cloned()
            .map(Value::Array)
            .unwrap_or_else(|| json!([])),
    )
    .map_err(error)
}
fn change<T>(
    s: &AppState,
    id: &str,
    f: impl FnOnce(&mut Vec<ThreadPullRequestLink>, &mut Value) -> Result<T, Value>,
) -> Result<T, Value> {
    let before = s.store.get(control::THREADS, id).map_err(error)?;
    let mut doc = before
        .clone()
        .unwrap_or(control::thread_doc(s, id).map_err(|(_, b)| b.0["error"].clone())?);
    let mut rows: Vec<ThreadPullRequestLink> = serde_json::from_value(
        doc["pull_requests"]
            .as_array()
            .cloned()
            .map(Value::Array)
            .unwrap_or_else(|| json!([])),
    )
    .map_err(error)?;
    let result = f(&mut rows, &mut doc)?;
    doc["pull_requests"] = serde_json::to_value(rows).map_err(error)?;
    if !s
        .store
        .put_many_if(
            &[(control::THREADS, id, before.as_ref())],
            &[(control::THREADS, id, &doc)],
        )
        .map_err(error)?
    {
        return Err(error("Pull-request state changed concurrently"));
    }
    Ok(result)
}
fn find<'a>(rows: &'a [ThreadPullRequestLink], name: &str) -> Option<&'a ThreadPullRequestLink> {
    let name = name.trim().to_lowercase();
    rows.iter().filter(|link| link.visible()).find(|link| {
        label(&link.key) == name
            || link.url.to_lowercase() == name
            || format!("{}#{}", link.key.repository, link.key.number) == name
    })
}
fn checked(
    s: &AppState,
    caller: &str,
    id: &str,
    write: bool,
    mutation: bool,
) -> Result<Value, Value> {
    let (grant, _) = control::effective(s, caller).map_err(|(_, b)| b.0["error"].clone())?;
    control::check_target(s, caller, id, &grant)?;
    if mutation && !write && !grant.tiers.contains(&control::Tier::Operate) {
        return Err(error("Operate permission is required to modify pull request links or watches"));
    }
    if write && !grant.categories.contains(&control::Category::PrWrite) {
        return Err(error("pr_write permission is required"));
    }
    let doc = chat::load(s, id).map_err(|(_, b)| b.0["error"].clone())?;
    if mutation && doc["thread"]["state"]["lineage"]["native_subagent"].is_string() {
        return Err(error("This thread is read-only"));
    }
    Ok(doc)
}
fn resolve(
    host: &dyn Forge,
    cwd: &Path,
    reference: &str,
) -> Result<(PullRequestKey, String), Value> {
    if let Some(target) = host.pull_request_url(reference) {
        return Ok(target);
    }
    if reference.contains("://") {
        return Err(json!({
            "code": "pull_request_unknown_host",
            "message": "This URL is not a pull request on a configured code host",
        }));
    }
    let number = reference
        .trim()
        .trim_start_matches('#')
        .parse::<u64>()
        .ok()
        .filter(|number| *number > 0)
        .ok_or_else(|| error("Enter a pull request URL or positive number"))?;
    let repository = host
        .checkout_repository(&Checkout::new(cwd))
        .ok_or_else(|| {
            json!({
                "code": "pull_request_no_repository",
                "message": "This project has no configured pull-request repository. Use a full PR URL.",
            })
        })?;
    let key = repository.key(number);
    let url = host
        .url(&key)
        .ok_or_else(|| error("Pull-request host is not configured"))?;
    Ok((key, url))
}
pub fn read(s: &Arc<AppState>, caller: &str, p: GetPullRequestParams) -> Result<Value, Value> {
    let id = control::resolve_thread_id(caller, &p.thread_id);
    checked(s, caller, id, false, false)?;
    let rows = links(s, id)?;
    let link = find(&rows, &p.key)
        .ok_or_else(|| error("No such pull request is linked to this thread"))?;
    Ok(
        json!({"key":label(&link.key),"pull_request":link,"omitted":["the host's last snapshot only (checks and reviews rolled up); refresh the linked pull request for a new read"]}),
    )
}
pub async fn refresh(s: &Arc<AppState>, id: &str, key: PullRequestKey) -> Result<(), Value> {
    let host = host(s)?;
    let requested = key.clone();
    let summary = tokio::task::spawn_blocking(move || host.summary(&requested))
        .await
        .map_err(error)?;
    let summary = match summary {
        Ok(summary) => summary,
        Err(failure) => {
            let retry = failure
                .retry_at()
                .and_then(|at| at.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|at| at.as_secs());
            change(s, id, |rows, _| {
                if let Some(link) = rows.iter_mut().find(|link| link.key == key) {
                    link.sync_error = Some(
                        retry
                            .map(|retry_at| PullRequestSyncError::RateLimited { retry_at })
                            .unwrap_or(PullRequestSyncError::Failed),
                    );
                }
                Ok(())
            })?;
            let _ = control::emit(
                s,
                id,
                "core.pull_request.updated",
                json!({"key":label(&key),"error":host_error(failure.clone())}),
            )
            .map_err(|(_, b)| b.0["error"].clone())?;
            return Err(host_error(failure));
        }
    };
    change(s, id, |rows, _| {
        if let Some(link) = rows
            .iter_mut()
            .find(|link| link.key == key && link.visible())
        {
            link.snapshot = Some(summary.snapshot);
            link.sync_error = None;
        }
        Ok(())
    })?;
    let _ = control::emit(
        s,
        id,
        "core.pull_request.updated",
        json!({"key":label(&key)}),
    )
    .map_err(|(_, b)| b.0["error"].clone())?;
    crate::control_actions::self_due(s, id, false).await?;
    Ok(())
}
pub async fn act(s: &Arc<AppState>, caller: &str, p: PullRequestParams) -> Result<Value, Value> {
    act_inner(s, caller, p, false).await
}
pub async fn act_as_user(s: &Arc<AppState>, p: PullRequestParams) -> Result<Value, Value> {
    act_inner(s, "user", p, true).await
}
async fn act_inner(
    s: &Arc<AppState>,
    caller: &str,
    p: PullRequestParams,
    user: bool,
) -> Result<Value, Value> {
    let id = control::resolve_thread_id(caller, &p.thread_id).to_owned();
    let doc = if user {
        chat::load(s, &id).map_err(|(_, b)| b.0["error"].clone())?
    } else {
        checked(s, caller, &id, p.op.writes(), true)?
    };
    let host = host(s)?;
    let named = p.reference.or(p.key);
    if p.op == PullRequestOp::Link {
        let reference = named.ok_or_else(|| error("link needs reference"))?;
        let cwd = crate::native_chat::session_cwd(s, &doc, &id).map_err(error)?;
        let h = host.clone();
        let (key, url) = tokio::task::spawn_blocking(move || resolve(h.as_ref(), &cwd, &reference))
            .await
            .map_err(error)??;
        let already = change(s, &id, |rows, _| {
            Ok(!pull_request::link_pull_request(
                rows,
                key.clone(),
                url,
                if user {
                    PullRequestSource::Manual
                } else {
                    PullRequestSource::Agent
                },
                chrono::Utc::now().timestamp() as u64,
                true,
            ))
        })?;
        control::audit(s, caller, &id, "pr_link", json!({"key":label(&key)}));
        let state = s.clone();
        let target = id.clone();
        let updating = key.clone();
        tokio::spawn(async move {
            if let Err(error) = refresh(&state, &target, updating).await {
                tracing::error!(%error,"pull request refresh failed");
            }
        });
        return Ok(
            json!({"thread_id":id,"outcome":"applied","output":{"key":key,"already_linked":already}}),
        );
    }
    let rows = links(s, &id)?;
    let key = find(&rows, named.as_deref().unwrap_or(""))
        .ok_or_else(|| error("No such pull request is linked to this thread"))?
        .key
        .clone();
    match p.op {
        PullRequestOp::Unlink => {
            change(s, &id, |rows, _| {
                pull_request::unlink_pull_request(rows, &key);
                Ok(())
            })?;
        }
        PullRequestOp::Watch | PullRequestOp::Unwatch => {
            let watching = p.op == PullRequestOp::Watch;
            change(s, &id, |rows, _| {
                let link = rows
                    .iter_mut()
                    .find(|link| link.key == key)
                    .ok_or_else(|| error("link disappeared"))?;
                link.watch = watching
                    .then(|| PullRequestWatch::new(chrono::Utc::now().timestamp_millis() as u64));
                Ok(())
            })?;
            if watching {
                let _ = set_watches_paused(s, &id, false);
                let state = s.clone();
                let target = id.clone();
                let key = key.clone();
                tokio::spawn(async move {
                    if let Err(error) = poll_one(&state, &target, key).await {
                        tracing::error!(%error,"pull request watch failed");
                    }
                });
            }
        }
        PullRequestOp::Refresh => {
            let state = s.clone();
            let target = id.clone();
            let key = key.clone();
            tokio::spawn(async move {
                if let Err(error) = refresh(&state, &target, key).await {
                    tracing::error!(%error,"pull request refresh failed");
                }
            });
        }
        op => {
            use tcode_control_protocol::{PullRequestAction, PullRequestActionResult};
            let action = match op {
                PullRequestOp::Comment { body } => PullRequestAction::Comment { body },
                PullRequestOp::Reply {
                    review_thread_id,
                    body,
                } => PullRequestAction::ReplyToThread {
                    thread_id: review_thread_id,
                    body,
                },
                PullRequestOp::Resolve { review_thread_id } => PullRequestAction::ResolveThread {
                    thread_id: review_thread_id,
                    resolved: true,
                },
                _ => unreachable!(),
            };
            let requested = key.clone();
            let outcome = tokio::task::spawn_blocking(move || host.act(&requested, &action))
                .await
                .map_err(error)?;
            let status = if matches!(outcome, PullRequestActionResult::Applied) {
                "applied"
            } else {
                "failed"
            };
            control::audit(
                s,
                caller,
                &id,
                "pr_write",
                json!({"key":label(&key),"outcome":outcome}),
            );
            return Ok(json!({"thread_id":id,"outcome":status,"output":outcome}));
        }
    }
    control::audit(s, caller, &id, "pull_request", json!({"key":label(&key)}));
    Ok(json!({"thread_id":id,"outcome":"applied"}))
}
async fn pending(
    s: &Arc<AppState>,
    id: &str,
    key: &PullRequestKey,
    wake: &PendingWake,
) -> Result<(), Value> {
    let ctrl = control::thread_doc(s, id).map_err(|(_, body)|body.0["error"].clone())?;
    if control::read_lifecycle(&ctrl).settled || ctrl["pull_request_watches_paused"] == true {
        return Ok(());
    }
    let active = links(s, id)?.into_iter().find(|link| &link.key == key && link.visible())
        .and_then(|link|link.watch).and_then(|watch|watch.pending_wake);
    if !active.is_some_and(|pending| pending.id == wake.id) { return Ok(()); }
    control::deliver(
        s,
        id,
        &format!("pr-watch:{}:{}", label(key), wake.id),
        &wake.text,
    )
    .await
    .map_err(|(_, b)| b.0["error"].clone())?;
    Ok(())
}
pub fn accepted(s: &Arc<AppState>, id: &str, receipt: &str) -> Result<(), Value> {
    if !receipt.starts_with("pr-watch:") {
        return Ok(());
    }
    change(s, id, |rows, _| {
        for link in rows {
            if let Some(watch) = link.watch.as_mut() {
                if let Some(pending) = &watch.pending_wake {
                    if receipt == format!("pr-watch:{}:{}", label(&link.key), pending.id) {
                        if pending.last {
                            link.watch = None;
                        } else {
                            watch.pending_wake = None;
                        }
                    }
                }
            }
        }
        Ok(())
    })
}

#[derive(Default)]
struct LastRead {
    at: Option<std::time::Instant>,
    snapshot: String,
    checks_running: bool,
    remarks_complete: bool,
    watches: std::collections::HashSet<(String, String)>,
    status: Option<String>,
    remarks: Option<String>,
    tails: tcode_control_services::forge::Tails,
}
#[derive(Clone)]
struct WatchTarget {
    id: String,
    link: ThreadPullRequestLink,
}
struct WatchGroup {
    key: (Option<String>, PullRequestKey),
    targets: Vec<WatchTarget>,
    snapshot: String,
}
fn fingerprint(link: &ThreadPullRequestLink) -> String {
    link.snapshot
        .as_ref()
        .map(|snapshot| {
            format!(
                "{:?} {:?} {:?} {:?} {:?} {}",
                snapshot.state,
                snapshot.updated_at,
                snapshot.checks_state,
                snapshot.mergeability,
                snapshot.review_decision,
                snapshot.is_draft
            )
        })
        .unwrap_or_default()
}
fn plan(
    group: &WatchGroup,
    last: Option<&LastRead>,
    fingerprint: Option<&tcode_control_services::forge::Fingerprint>,
    now: std::time::Instant,
) -> (bool, bool, bool) {
    let Some(last) = last.filter(|last| {
        group.targets.iter().all(|target| {
            last.watches.contains(&(
                target.id.clone(),
                target.link.watch.as_ref().unwrap().started_at.clone(),
            ))
        })
    }) else {
        return (true, true, false);
    };
    let elapsed = last
        .at
        .map(|at| now.saturating_duration_since(at))
        .unwrap_or_default();
    let Some(fingerprint) = fingerprint else {
        let read = last.checks_running
            || !last.remarks_complete
            || last.snapshot != group.snapshot
            || elapsed >= std::time::Duration::from_secs(600);
        return (read, read, false);
    };
    let reread = elapsed >= std::time::Duration::from_secs(1800);
    let activity =
        last.remarks.as_ref() != Some(&fingerprint.remarks) || !last.remarks_complete || reread;
    (
        activity || last.status.as_ref() != Some(&fingerprint.status) || last.checks_running,
        activity,
        reread,
    )
}
async fn failed_read(
    s: &Arc<AppState>,
    target: &WatchTarget,
    key: &PullRequestKey,
    failure: &ForgeError,
) -> Result<(), Value> {
    let retry = failure
        .retry_at()
        .and_then(|at| at.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|at| at.as_secs());
    change(s, &target.id, |rows, doc| {
        if let Some(link) = rows.iter_mut().find(|link| &link.key == key) {
            link.sync_error = Some(if let Some(at) = retry {
                PullRequestSyncError::RateLimited { retry_at: at }
            } else {
                PullRequestSyncError::Failed
            });
            if !matches!(
                failure.kind,
                ForgeErrorKind::RateLimited { .. } | ForgeErrorKind::Paused { .. }
            ) {
                let name = label(key);
                let failures = doc["pr_watch_failures"][&name].as_u64().unwrap_or(0) + 1;
                doc["pr_watch_failures"][&name] = json!(failures);
                if failures >= watch::READ_FAILURE_LIMIT as u64 {
                    if let Some(watch) = link.watch.as_mut() {
                        watch.pending_wake = Some(PendingWake {
                            id: uuid::Uuid::new_v4().to_string(),
                            text: watch::unreadable_message(key.number, &link.url),
                            last: true,
                        });
                    }
                }
            }
        }
        Ok(())
    })?;
    if let Some(wake) = links(s, &target.id)?
        .into_iter()
        .find(|link| &link.key == key)
        .and_then(|link| link.watch)
        .and_then(|watch| watch.pending_wake)
    {
        pending(s, &target.id, key, &wake).await?;
    }
    Ok(())
}
async fn apply_read(
    s: &Arc<AppState>,
    target: &WatchTarget,
    key: &PullRequestKey,
    read: &watch::PullRequestWatchRead,
    remarks: Option<&[watch::PullRequestRemark]>,
) -> Result<(), Value> {
    let Some(active) = target.link.watch.as_ref() else {
        return Ok(());
    };
    let report = watch::evaluate(active, read, remarks);
    let ending = matches!(
        read.state,
        PullRequestState::Closed | PullRequestState::Merged
    );
    let text = match read.state {
        PullRequestState::Closed => Some(watch::closed_message(key.number, &target.link.url)),
        PullRequestState::Merged => Some(watch::merged_message(key.number, &target.link.url)),
        _ if !report.changes.is_empty() => Some(watch::update_message(
            key.number,
            &target.link.url,
            &read.base_branch,
            &report,
        )),
        _ => None,
    };
    let mut wake = None;
    change(s, &target.id, |rows, doc| {
        if let Some(link) = rows
            .iter_mut()
            .find(|link| &link.key == key && link.visible())
        {
            if let Some(current) = link
                .watch
                .as_ref()
                .filter(|current| current.started_at == active.started_at)
            {
                let mut next = report.next;
                next.pending_wake = current.pending_wake.clone();
                if let Some(text) = text {
                    wake = Some(PendingWake {
                        id: current
                            .pending_wake
                            .as_ref()
                            .map(|old| old.id.clone())
                            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
                        text: current
                            .pending_wake
                            .as_ref()
                            .map(|old| format!("{}\n\n{text}", old.text))
                            .unwrap_or(text),
                        last: ending || report.exhausted,
                    });
                    next.pending_wake = wake.clone();
                }
                link.watch = Some(next);
                link.sync_error = None;
                doc["pr_watch_failures"][label(key)] = json!(0);
            }
        }
        Ok(())
    })?;
    if let Err(error) = refresh(s, &target.id, key.clone()).await {
        tracing::warn!(%error,"PR summary refresh failed");
    }
    if let Some(wake) = wake {
        let survived = links(s, &target.id)?.into_iter().any(|link| &link.key == key && link.watch.as_ref().is_some_and(|current| current.started_at == active.started_at && current.pending_wake.as_ref().is_some_and(|pending|pending.id == wake.id)));
        if !survived { return Ok(()); }
        crate::control_tools::external_event(s, &target.id, "pr_changed", &wake.text).await;
        pending(s, &target.id, key, &wake).await?;
    }
    Ok(())
}
pub async fn poll_one(s: &Arc<AppState>, id: &str, key: PullRequestKey) -> Result<(), Value> {
    let ctrl = control::thread_doc(s, id).map_err(|(_, b)| b.0["error"].clone())?;
    let doc = chat::load(s, id).map_err(|(_, b)| b.0["error"].clone())?;
    if ctrl["pull_request_watches_paused"] == true
        || control::read_lifecycle(&ctrl).settled
        || doc["thread"]["archived"] == true
    {
        return Ok(());
    }
    let Some(link) = links(s, id)?
        .into_iter()
        .find(|link| link.key == key && link.watch.is_some())
    else {
        return Ok(());
    };
    let target = WatchTarget {
        id: id.into(),
        link,
    };
    let host = host(s)?;
    let requested = key.clone();
    let read = tokio::task::spawn_blocking(move || {
        Ok::<_, ForgeError>((
            host.watch_detail(&requested)?,
            host.activity(&requested, &mut Default::default())?,
        ))
    })
    .await
    .map_err(error)?;
    match read {
        Ok((read, remarks)) => apply_read(s, &target, &key, &read, remarks.as_deref()).await,
        Err(failure) => failed_read(s, &target, &key, &failure).await,
    }
}
async fn pass(
    s: &Arc<AppState>,
    cache: &mut std::collections::HashMap<(Option<String>, PullRequestKey), LastRead>,
) -> Result<(), Value> {
    let mut groups =
        std::collections::HashMap::<(Option<String>, PullRequestKey), WatchGroup>::new();
    for (id, ctrl) in s.store.entries(control::THREADS).map_err(error)? {
        let doc = chat::load(s, &id).map_err(|(_, b)| b.0["error"].clone())?;
        if doc["thread"]["archived"] == true
            || control::read_lifecycle(&ctrl).settled
            || ctrl["pull_request_watches_paused"] == true
        {
            continue;
        }
        for link in links(s, &id)?
            .into_iter()
            .filter(|link| link.visible() && link.watch.is_some())
        {
            let active = link.watch.as_ref().unwrap();
            if let Some(wake) = &active.pending_wake {
                pending(s, &id, &link.key, wake).await?;
                if wake.last {
                    continue;
                }
            }
            let key = (
                doc["thread"]["project_id"].as_str().map(str::to_owned),
                link.key.clone(),
            );
            let group = groups.entry(key.clone()).or_insert_with(|| WatchGroup {
                key,
                snapshot: fingerprint(&link),
                targets: Vec::new(),
            });
            group.targets.push(WatchTarget {
                id: id.clone(),
                link,
            });
        }
    }
    cache.retain(|key, _| groups.contains_key(key));
    if groups.is_empty() {
        return Ok(());
    }
    let groups: Vec<_> = groups.into_values().collect();
    let host = host(s)?;
    let keys: Vec<_> = groups.iter().map(|group| group.key.1.clone()).collect();
    let reader = host.clone();
    let fingerprints = tokio::task::spawn_blocking(move || reader.fingerprints(&keys))
        .await
        .map_err(error)?;
    let mut planned = Vec::new();
    for (group, fingerprint) in groups.into_iter().zip(fingerprints) {
        let fingerprint = match fingerprint {
            Ok(value) => value,
            Err(failure)
                if matches!(
                    failure.kind,
                    ForgeErrorKind::RateLimited { .. } | ForgeErrorKind::Paused { .. }
                ) =>
            {
                continue;
            }
            Err(_) => None,
        };
        let (_, activity, reread) = match plan(
            &group,
            cache.get(&group.key),
            fingerprint.as_ref(),
            std::time::Instant::now(),
        ) {
            (false, _, _) => continue,
            plan => plan,
        };
        let tails = if reread {
            Default::default()
        } else {
            cache
                .get_mut(&group.key)
                .map(|last| std::mem::take(&mut last.tails))
                .unwrap_or_default()
        };
        planned.push((group, fingerprint, activity, tails));
    }
    for chunk in planned.chunks_mut(4) {
        let reads =
            futures_util::future::join_all(chunk.iter_mut().map(|(group, _, activity, tails)| {
                let host = host.clone();
                let key = group.key.1.clone();
                let activity = *activity;
                let mut tails = std::mem::take(tails);
                tokio::task::spawn_blocking(move || {
                    let read = host.watch_detail(&key);
                    let remarks = if activity {
                        Some(host.activity(&key, &mut tails))
                    } else {
                        None
                    };
                    (read, remarks, tails)
                })
            }))
            .await;
        for ((group, fingerprint, activity, _), result) in chunk.iter_mut().zip(reads) {
            let (read, remarks, tails) = result.map_err(error)?;
            let read = match read {
                Ok(read) => read,
                Err(failure) => {
                    for target in &group.targets {
                        failed_read(s, target, &group.key.1, &failure).await?;
                    }
                    continue;
                }
            };
            let remarks = match remarks {
                Some(Ok(remarks)) => remarks,
                Some(Err(failure)) => {
                    tracing::warn!(%failure,"pull-request activity could not be read completely");
                    None
                }
                None => None,
            };
            for target in &group.targets {
                apply_read(s, target, &group.key.1, &read, remarks.as_deref()).await?;
            }
            let previous = cache.remove(&group.key).unwrap_or_default();
            cache.insert(
                group.key.clone(),
                LastRead {
                    at: if *activity {
                        Some(std::time::Instant::now())
                    } else {
                        previous.at
                    },
                    snapshot: group.snapshot.clone(),
                    checks_running: read
                        .checks
                        .iter()
                        .any(|check| check.status == watch::CheckStatus::Pending),
                    remarks_complete: if *activity {
                        remarks.is_some()
                    } else {
                        previous.remarks_complete
                    },
                    watches: group
                        .targets
                        .iter()
                        .map(|target| {
                            (
                                target.id.clone(),
                                target.link.watch.as_ref().unwrap().started_at.clone(),
                            )
                        })
                        .collect(),
                    status: fingerprint.as_ref().map(|value| value.status.clone()),
                    remarks: fingerprint.as_ref().map(|value| value.remarks.clone()),
                    tails,
                },
            );
        }
    }
    Ok(())
}
pub async fn run_watches(s: Arc<AppState>) {
    let mut cache = std::collections::HashMap::new();
    loop {
        if let Err(error) = pass(&s, &mut cache).await {
            tracing::error!(%error,"PR watch pass failed");
        }
        tokio::time::sleep(std::time::Duration::from_secs(120)).await;
    }
}

pub fn set_watches_paused(s: &Arc<AppState>, id: &str, paused: bool) -> Result<(), Value> {
    let before = s.store.get(control::THREADS, id).map_err(error)?;
    let mut ctrl = before.clone().unwrap_or_else(|| json!({}));
    let was_paused = ctrl["pull_request_watches_paused"] == true;
    if was_paused == paused {
        return Ok(());
    }
    ctrl["pull_request_watches_paused"] = json!(paused);
    if paused {
        if let Some(rows) = ctrl["pull_requests"].as_array_mut() {
            for row in rows {
                if let Some(watch) = row.get_mut("watch").and_then(Value::as_object_mut) {
                    watch.remove("pending_wake");
                }
            }
        }
    }
    if !s
        .store
        .put_many_if(
            &[(control::THREADS, id, before.as_ref())],
            &[(control::THREADS, id, &ctrl)],
        )
        .map_err(error)?
    {
        return Err(error("Pull-request watch state changed concurrently"));
    }
    let _ = control::emit(
        s,
        id,
        "core.control.updated",
        json!({"pull_request_watches_paused": paused}),
    );
    if !paused {
        if let Ok(rows) = links(s, id) {
            for link in rows.into_iter().filter(|link| link.visible() && link.watch.is_some()) {
                let state = s.clone();
                let target = id.to_owned();
                let key = link.key;
                tokio::spawn(async move {
                    if let Err(error) = poll_one(&state, &target, key).await {
                        tracing::warn!(%error, "resumed PR watch poll failed");
                    }
                });
            }
        }
    }
    Ok(())
}

pub fn resume_watches_if_paused(s: &Arc<AppState>, id: &str) {
    if let Ok(Some(ctrl)) = s.store.get(control::THREADS, id) {
        if ctrl["pull_request_watches_paused"] == true {
            if let Err(error) = set_watches_paused(s, id, false) {
                tracing::warn!(%error, thread_id = id, "failed to resume PR watches");
            }
        }
    }
}

fn resolve_linked_key(s: &AppState, id: &str, key_str: &str) -> Result<PullRequestKey, Value> {
    let rows = links(s, id)?;
    find(&rows, key_str)
        .map(|link| link.key.clone())
        .ok_or_else(|| error("No such pull request is linked to this thread"))
}

pub async fn read_as_user(
    s: &Arc<AppState>,
    id: &str,
    key_str: &str,
    read: tcode_control_protocol::PullRequestRead,
) -> Result<Value, Value> {
    let key = resolve_linked_key(s, id, key_str)?;
    let host = host(s)?;
    let caps = host.capabilities(&key);
    let terms = host.terms(&key);
    let requested = key.clone();
    let (response, _) = tokio::task::spawn_blocking(move || host.read(&requested, read))
        .await
        .map_err(error)?
        .map_err(host_error)?;
    Ok(json!({
        "key": key,
        "capabilities": caps,
        "terms": {
            "name": terms.name,
        },
        "response": response,
    }))
}

pub async fn stack_as_user(s: &Arc<AppState>, id: &str, key_str: &str) -> Result<Value, Value> {
    let key = resolve_linked_key(s, id, key_str)?;
    let host = host(s)?;
    let requested = key.clone();
    let stack = tokio::task::spawn_blocking(move || host.stack(&requested))
        .await
        .map_err(error)?
        .map_err(host_error)?;
    Ok(json!({ "key": key, "stack": stack }))
}

pub async fn set_viewed_as_user(
    s: &Arc<AppState>,
    id: &str,
    key_str: &str,
    paths: Vec<String>,
    viewed: bool,
) -> Result<Value, Value> {
    let key = resolve_linked_key(s, id, key_str)?;
    let host = host(s)?;
    let requested = key.clone();
    tokio::task::spawn_blocking(move || host.set_viewed(&requested, &paths, viewed))
        .await
        .map_err(error)?
        .map_err(host_error)?;
    Ok(json!({ "ok": true }))
}

pub async fn submit_review_as_user(
    s: &Arc<AppState>,
    id: &str,
    key_str: &str,
    verdict: tcode_control_protocol::PullRequestReviewVerdict,
    head: String,
    body: String,
    comments: Vec<tcode_control_core::pull_request::PullRequestReviewDraftComment>,
) -> Result<Value, Value> {
    let key = resolve_linked_key(s, id, key_str)?;
    let host = host(s)?;
    let requested = key.clone();
    let outcome = tokio::task::spawn_blocking(move || {
        host.submit_review(&requested, verdict, &head, &body, &comments)
    })
    .await
    .map_err(error)?;
    if matches!(outcome, tcode_control_protocol::PullRequestActionResult::Applied) {
        let state = s.clone();
        let target = id.to_owned();
        tokio::spawn(async move {
            let _ = refresh(&state, &target, key).await;
        });
    }
    Ok(json!({ "outcome": outcome }))
}

pub async fn host_action_as_user(
    s: &Arc<AppState>,
    id: &str,
    key_str: &str,
    action: tcode_control_protocol::PullRequestAction,
) -> Result<Value, Value> {
    let key = resolve_linked_key(s, id, key_str)?;
    let host = host(s)?;
    let requested = key.clone();
    let outcome = tokio::task::spawn_blocking(move || host.act(&requested, &action))
        .await
        .map_err(error)?;
    if matches!(outcome, tcode_control_protocol::PullRequestActionResult::Applied) {
        let state = s.clone();
        let target = id.to_owned();
        tokio::spawn(async move {
            let _ = refresh(&state, &target, key).await;
        });
    }
    Ok(json!({ "outcome": outcome }))
}
