//! The host side of `tcode_control`: which thread may do what, dispatch of its tool calls,
//! and the record every write leaves.

use super::*;
use control_mcp::{ControlOp, ToolRequirement};
use tcode_core::control::{ControlAction, ControlGrant, ControlGrantSource, ControlScope, action};
use tcode_core::settings::SettingsPatch;
use tcode_protocol::{ControlActivityEntry, ControlStatus, ThreadReferenceStatus};

mod content;
mod grants;
mod observe;
mod operate;
mod organize;
mod privileged;
mod references;
#[cfg(test)]
mod tests;
mod watches;

/// How many writes Settings → Control lists.
const CONTROL_ACTIVITY_LIMIT: usize = 200;

pub(super) struct ControlState {
    activity: VecDeque<ControlActivityEntry>,
    review_records: Vec<tcode_core::control::ReviewRecord>,
    /// Answers to recent calls that carried a `request_id`, so a retry does not act twice.
    answered: VecDeque<(String, serde_json::Value)>,
    /// Bytes each caller has read from each thread since the host started, for the hint to
    /// narrow the query.
    content_read: HashMap<(String, String), usize>,
}

impl ControlState {
    pub(super) fn new(
        review_records: Vec<tcode_core::control::ReviewRecord>,
        mut activity: Vec<ControlActivityEntry>,
    ) -> Self {
        activity.truncate(CONTROL_ACTIVITY_LIMIT);
        Self {
            activity: activity.into(),
            review_records,
            answered: VecDeque::new(),
            content_read: HashMap::new(),
        }
    }
}

/// A call's answer: ready on the host thread, or computed off it because it runs git, reads
/// the store or waits for work the call started, which would stall every client if awaited
/// on the host thread.
pub(super) enum Answer {
    Now(Result<serde_json::Value, String>),
    Later(HostTask<Result<serde_json::Value, String>>),
}

/// The part of a call that needs the host state, run on the host thread once the call's
/// off-thread work has finished; its result is the call's answer.
async fn finish_on_host(
    host_cx: HostCx,
    finish: impl FnOnce(&mut AppState, &mut HostCx) -> Result<serde_json::Value, String>
    + Send
    + 'static,
) -> Result<serde_json::Value, String> {
    let (answered, answer) = smol::channel::bounded(1);
    host_cx.enqueue(move |state, cx| {
        let _ = answered.try_send(finish(state, cx));
    });
    answer
        .recv()
        .await
        .unwrap_or_else(|_| Err("the host stopped before the call finished".into()))
}

/// The thread behind a `tcode_control` call, with the grant in force when it arrived.
#[derive(Clone)]
pub(super) struct Caller {
    pub(super) thread_id: String,
    pub(super) project_id: Option<String>,
    pub(super) grant: ControlGrant,
    /// Its Orchestrate parent and that thread's ancestors, which it may neither read nor
    /// act on.
    pub(super) ancestors: Vec<String>,
}

impl AppState {
    pub fn attach_control_mcp(&mut self, server: control_mcp::ControlMcpServer) {
        self.mcp.control_url = Some(server.url);
        self.mcp.control_tokens = Some(server.tokens);
        self.mcp.control_requests = Some(server.requests);
        let grants = self.mcp.control_grants.clone();
        self.mcp.control_source = Some(server.grants.clone());
        server.grants.set(Arc::new(move |thread_id| {
            grants.lock().unwrap().get(thread_id).cloned()
        }));
    }

    /// Taking the receiver makes repeated calls harmless: exactly one pump owns the stream.
    pub fn pump_control_requests(&mut self, cx: &mut HostCx) {
        let Some(requests) = self.mcp.control_requests.take() else {
            return;
        };
        let host_cx = cx.clone();
        HostCx::spawn_detached(cx, async move {
            while let Ok(request) = requests.recv().await {
                let control_mcp::BrokerRequest {
                    thread_id,
                    op,
                    reply,
                } = request;
                host_cx.enqueue(move |state, cx| state.handle_control_op(thread_id, op, reply, cx));
            }
        });
    }

    pub(super) fn caller_grant(&self, meta: &SessionMeta) -> (ControlGrant, ControlGrantSource) {
        self.settings
            .control
            .resolve(meta.control_grant.as_ref(), meta.project_id.as_deref())
    }

    /// Whether the thread's provider is given the `tcode_control` server when it starts;
    /// with an empty grant it lists `request_grant` alone, so the model can still ask.
    pub(super) fn control_offered(&self, meta: &SessionMeta) -> bool {
        self.settings.control.enabled
            && self.mcp.control_url.is_some()
            && meta.provider.caps().mcp_servers
    }

    pub(super) fn control_registration_for(
        &mut self,
        meta: &SessionMeta,
    ) -> Option<agent::McpRegistration> {
        if !self.control_offered(meta) {
            return None;
        }
        if let Some(registration) = self.mcp.control_registrations.get(&meta.id) {
            return Some(registration.clone());
        }
        let token = self.mcp.control_tokens.as_ref()?.register(&meta.id);
        let registration = agent::McpRegistration {
            name: agent::McpRegistration::SERVER_NAME_CONTROL.into(),
            url: self.mcp.control_url.clone()?,
            bearer_token: token,
        };
        self.mcp
            .control_registrations
            .insert(meta.id.clone(), registration.clone());
        self.mcp
            .control_grants
            .lock()
            .unwrap()
            .insert(meta.id.clone(), self.caller_grant(meta).0);
        Some(registration)
    }

    pub(super) fn revoke_control_registration(&mut self, session_id: &str) {
        if let Some(registration) = self.mcp.control_registrations.remove(session_id)
            && let Some(tokens) = &self.mcp.control_tokens
        {
            tokens.revoke(&registration.bearer_token);
        }
        self.mcp.control_grants.lock().unwrap().remove(session_id);
        if let Some(source) = &self.mcp.control_source {
            source.forget(session_id);
        }
    }

    /// Re-read every registered thread's effective grant after a grant or setting changed, so
    /// the next connection lists the right tools; with the feature off a registered thread
    /// keeps only `request_grant`.
    pub(super) fn refresh_control_grants(&mut self) {
        let registered: Vec<String> = self.mcp.control_registrations.keys().cloned().collect();
        let grants: Vec<(String, ControlGrant)> = registered
            .into_iter()
            .filter_map(|id| {
                let meta = self.find_meta(&id)?;
                let grant = if self.settings.control.enabled {
                    self.caller_grant(&meta).0
                } else {
                    ControlGrant::none()
                };
                Some((id, grant))
            })
            .collect();
        let mut table = self.mcp.control_grants.lock().unwrap();
        for (id, grant) in grants {
            table.insert(id, grant);
        }
    }

    pub(super) fn control_status(&self, meta: &SessionMeta) -> Option<ControlStatus> {
        if !meta.provider.caps().mcp_servers {
            return None;
        }
        let (grant, source) = self.caller_grant(meta);
        let references = meta
            .references
            .iter()
            .map(|reference| {
                let target = self.find_meta(&reference.thread_id);
                ThreadReferenceStatus {
                    thread_id: reference.thread_id.clone(),
                    title: target.as_ref().map(|m| m.title.clone()).unwrap_or_default(),
                    deleted: target.is_none(),
                    by: reference.by,
                    boundary_notice: reference.boundary_notice,
                }
            })
            .collect();
        Some(ControlStatus {
            grant,
            source,
            pending_requests: meta.control_grant_requests.clone(),
            references,
            self_settle: meta.self_settle.clone(),
            tools_offered: self.control_offered(meta),
        })
    }

    /// `meta`'s thread as a `tcode_control` caller, with the grant in force now.
    pub(super) fn caller_of(&self, meta: &SessionMeta) -> Caller {
        let mut ancestors: Vec<String> = Vec::new();
        let mut next = meta.parent_session_id.clone();
        while let Some(id) = next.filter(|id| !ancestors.contains(id)) {
            next = self
                .find_meta(&id)
                .and_then(|parent| parent.parent_session_id);
            ancestors.push(id);
        }
        Caller {
            thread_id: meta.id.clone(),
            project_id: meta.project_id.clone(),
            grant: self.caller_grant(meta).0,
            ancestors,
        }
    }

    /// Whether a call may reach `target`: inside `scope` (the grant's, or the narrower one the
    /// call asked for), or, for a read of its content, the caller itself, one of its
    /// Orchestrate children or a thread it references; never the caller's own Orchestrate
    /// parent or its ancestors.
    pub(super) fn target_reachable(
        &self,
        caller: &Caller,
        scope: &ControlScope,
        target: &SessionMeta,
        reads_content: bool,
    ) -> bool {
        if caller.ancestors.contains(&target.id) {
            return false;
        }
        if scope.reaches(caller.project_id.as_deref(), target.project_id.as_deref()) {
            return true;
        }
        let caller_meta = || {
            self.sessions
                .iter()
                .find(|meta| meta.id == caller.thread_id)
                .or_else(|| {
                    self.resident(&caller.thread_id)
                        .map(|session| &session.meta)
                })
        };
        reads_content
            && (target.id == caller.thread_id
                || target.parent_session_id.as_deref() == Some(caller.thread_id.as_str())
                || caller_meta().is_some_and(|meta| {
                    meta.references
                        .iter()
                        .any(|reference| reference.thread_id == target.id)
                }))
    }

    /// Resolve one `tcode_control` call on the host thread.
    pub(crate) fn handle_control_op(
        &mut self,
        thread_id: String,
        op: ControlOp,
        reply: smol::channel::Sender<Result<serde_json::Value, String>>,
        cx: &mut HostCx,
    ) {
        let mut authored_cx = cx.clone();
        authored_cx.origin = Some(MessageOrigin::Agent);
        authored_cx.author = None;
        let cx = &mut authored_cx;
        let answer = match self.find_meta(&thread_id) {
            _ if !self.settings.control.enabled => Answer::Now(Err(
                "tcode_control is turned off in Settings → Control".into(),
            )),
            Some(meta) if meta.archived_at.is_none() => {
                let caller = self.caller_of(&meta);
                match self.authorize_control(&caller, &op) {
                    // A tool call must never take the host down: a panic in one
                    // handler answers that call and leaves every other thread served.
                    Ok(()) => {
                        let name = op.tool_name();
                        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                            self.dispatch_control_op(&caller, op, cx)
                        })) {
                            Ok(answer) => answer,
                            Err(_) => {
                                log::error!(
                                    "tcode_control: {name} panicked for thread {thread_id}"
                                );
                                Answer::Now(Err(format!(
                                    "{name} failed inside tcode; see the host log"
                                )))
                            }
                        }
                    }
                    Err(refusal) => Answer::Now(Err(refusal)),
                }
            }
            _ => Answer::Now(Err("unknown thread".into())),
        };
        match answer {
            Answer::Now(result) => {
                let _ = reply.try_send(result);
            }
            Answer::Later(task) => HostCx::spawn_detached(cx, async move {
                let _ = reply.send(task.await).await;
            }),
        }
    }

    /// The answer an earlier call with this `key` got, if it is still remembered.
    pub(super) fn answered(&self, key: &str) -> Option<serde_json::Value> {
        self.control
            .answered
            .iter()
            .find(|(answered, _)| answered == key)
            .map(|(_, value)| value.clone())
    }

    pub(super) fn remember_answer(&mut self, key: String, value: serde_json::Value) {
        self.control.answered.push_front((key, value));
        self.control.answered.truncate(CONTROL_ACTIVITY_LIMIT);
    }

    /// The grant check every call passes host-side, however the connection listed its tools:
    /// tier or category, the call's own categories, the scope it asks for, the targets it
    /// names, and the settings no grant lets an agent change. Dropping a queued human message needs `history_edit` and is judged where the
    /// queue is read. The refusal names the reason and the grant.
    pub(super) fn authorize_control(&self, caller: &Caller, op: &ControlOp) -> Result<(), String> {
        let name = op.tool_name();
        let grant = &caller.grant;
        let refused = |need: String| {
            Err(format!(
                "refused: `{name}` needs {need}; your grant: {}",
                grant.describe()
            ))
        };
        match op.requirement() {
            ToolRequirement::Tier(tier) if !grant.allows_tier(tier) => {
                return refused(format!("tier {}", tier.name()));
            }
            ToolRequirement::Category(category) if !grant.allows_category(category) => {
                return refused(format!("category {}", category.name()));
            }
            _ => {}
        }
        for category in op.extra_categories() {
            if !grant.allows_category(category) {
                return refused(format!("category {}", category.name()));
            }
        }
        if let Some(scope) = op.requested_scope()
            && !scope.within(&grant.scope, caller.project_id.as_deref())
        {
            return Err(format!(
                "refused: scope {} is wider than your grant's {}",
                scope.describe(),
                grant.scope.describe()
            ));
        }
        if let ControlOp::PatchSettings(params) = op {
            let patch: SettingsPatch = serde_json::from_value(params.patch.clone())
                .map_err(|error| format!("invalid settings patch: {error}"))?;
            if privileged::settings_patch_never_exposed(&patch) {
                let key = params
                    .patch
                    .get("type")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default();
                return Err(format!(
                    "refused: `patch_settings` never changes {key}: an agent may change only \
                     display preferences (language, theme, diff word wrap, panels, sidebar \
                     provider marks, frame throttling, the fallback review advisor)"
                ));
            }
        }
        if op.answers_per_target() {
            return Ok(());
        }
        for target in op.target_thread_ids() {
            self.check_target(caller, &grant.scope, target, op.reads_content())
                .map_err(|refusal| refusal.whole_call(target, &grant.scope))?;
        }
        Ok(())
    }

    /// Why `target` is closed to `caller`, or its meta when it is open.
    fn check_target(
        &self,
        caller: &Caller,
        scope: &ControlScope,
        target: &str,
        reads_content: bool,
    ) -> Result<SessionMeta, TargetRefusal> {
        let Some(meta) = self.find_meta(target) else {
            let referenced = self.find_meta(&caller.thread_id).is_some_and(|meta| {
                meta.references
                    .iter()
                    .any(|reference| reference.thread_id == target)
            });
            return Err(if referenced {
                TargetRefusal::DeletedReference
            } else {
                TargetRefusal::Unknown
            });
        };
        if caller.ancestors.iter().any(|ancestor| ancestor == target) {
            return Err(TargetRefusal::Ancestor);
        }
        if !self.target_reachable(caller, scope, &meta, reads_content) {
            return Err(TargetRefusal::OutsideScope);
        }
        Ok(meta)
    }

    /// One target of a per-item batch ([`ControlOp::answers_per_target`]): its meta, or the
    /// reason that item is skipped.
    pub(super) fn batch_target(
        &self,
        caller: &Caller,
        target: &str,
        reads_content: bool,
    ) -> Result<SessionMeta, String> {
        self.check_target(caller, &caller.grant.scope, target, reads_content)
            .map_err(|refusal| refusal.per_item(target, &caller.grant.scope))
    }

    fn dispatch_control_op(&mut self, caller: &Caller, op: ControlOp, cx: &mut HostCx) -> Answer {
        let now = match op {
            ControlOp::ListThreads(p) => return self.op_list_threads(caller, p, cx),
            ControlOp::Search(p) => return self.op_search(caller, p, cx),
            ControlOp::GetGit(p) => return self.op_get_git(caller, p, cx),
            ControlOp::ReviewCandidates(p) => return self.op_review_candidates(caller, p, cx),
            ControlOp::UpdateThreads(p) => return self.op_update_threads(caller, p, cx),
            ControlOp::ListProjects(p) => self.op_list_projects(caller, p, cx),
            ControlOp::GetThread(p) => self.op_get_thread(caller, p, cx),
            ControlOp::Outline(p) => return self.op_outline(caller, p, cx),
            ControlOp::Read(p) => return self.op_read(caller, p, cx),
            ControlOp::SearchInItem(p) => return self.op_search_in_item(caller, p, cx),
            ControlOp::FileHistory(p) => return self.op_file_history(caller, p, cx),
            ControlOp::GetPullRequest(p) => self.op_get_pull_request(caller, p, cx),
            ControlOp::ListProviders(p) => self.op_list_providers(caller, p, cx),
            ControlOp::GetSettings(p) => self.op_get_settings(caller, p, cx),
            ControlOp::ReadTerminal(p) => self.op_read_terminal(caller, p, cx),
            ControlOp::RecordReview(p) => self.op_record_review(caller, p, cx),
            ControlOp::SettleSelf(p) => self.op_settle_self(caller, p, cx),
            ControlOp::StartThread(p) => self.op_start_thread(caller, p, cx),
            ControlOp::SendMessage(p) => self.op_send_message(caller, p, cx),
            ControlOp::ControlThread(p) => self.op_control_thread(caller, p, cx),
            ControlOp::PullRequest(p) => return self.op_pull_request(caller, p, cx),
            ControlOp::Git(p) => return self.op_git(caller, p, cx),
            ControlOp::CreateProject(p) => self.op_create_project(caller, p, cx),
            ControlOp::MoveThread(p) => self.op_move_thread(caller, p, cx),
            ControlOp::NotifyWhen(p) => self.op_notify_when(caller, p, cx),
            ControlOp::CancelNotify(p) => self.op_cancel_notify(caller, p, cx),
            ControlOp::Reference(p) => self.op_reference(caller, p, cx),
            ControlOp::RespondInput(p) => self.op_respond_input(caller, p, cx),
            ControlOp::Terminal(p) => self.op_terminal(caller, p, cx),
            ControlOp::DeleteThreads(p) => self.op_delete_threads(caller, p, cx),
            ControlOp::Rewind(p) => self.op_rewind(caller, p, cx),
            ControlOp::PatchSettings(p) => self.op_patch_settings(caller, p, cx),
            ControlOp::RequestGrant(p) => self.op_request_grant(caller, p, cx),
        };
        Answer::Now(now)
    }

    /// Record a write in the target thread, as the actor's thread, and in the activity list.
    pub(super) fn audit(
        &mut self,
        caller: &Caller,
        target: &str,
        action: &str,
        detail: Option<String>,
        cx: &mut HostCx,
    ) {
        let actor_title = self
            .find_meta(&caller.thread_id)
            .map(|meta| meta.title)
            .unwrap_or_default();
        let target_title = self
            .find_meta(target)
            .map(|meta| meta.title)
            .unwrap_or_default();
        let action = ControlAction {
            actor_thread_id: caller.thread_id.clone(),
            actor_title,
            action: action.to_owned(),
            detail,
        };
        let at = now_millis();
        self.record_event(target, &AgentEvent::ControlAction(action.clone()), cx);
        if target != caller.thread_id {
            let in_caller = ControlAction {
                detail: Some(match &action.detail {
                    Some(detail) => format!("{target_title} — {detail}"),
                    None => target_title.clone(),
                }),
                ..action.clone()
            };
            self.record_event(&caller.thread_id, &AgentEvent::ControlAction(in_caller), cx);
        }
        self.control.activity.push_front(ControlActivityEntry {
            target_thread_id: target.to_owned(),
            target_title,
            at,
            action,
        });
        self.control.activity.truncate(CONTROL_ACTIVITY_LIMIT);
        let entries = self.control.activity.iter().cloned().collect();
        self.enqueue_store_write(StoreWrite::WriteControlActivity(entries), cx);
    }

    pub(crate) fn control_activity(&self, limit: usize) -> Vec<ControlActivityEntry> {
        self.control.activity.iter().take(limit).cloned().collect()
    }

    pub(crate) fn review_records(&self, limit: usize) -> Vec<tcode_core::control::ReviewRecord> {
        self.control
            .review_records
            .iter()
            .rev()
            .take(limit)
            .cloned()
            .collect()
    }

    /// Write a changed meta through to the resident copy, the index and the store.
    pub(super) fn store_meta(&mut self, meta: SessionMeta, cx: &mut HostCx) {
        if let Some(live) = self.meta_mut(&meta.id) {
            *live = meta.clone();
        }
        self.persist_meta(&meta, cx);
    }
}

/// A target closed to the caller; worded for a whole-call refusal or a per-item skip.
enum TargetRefusal {
    Unknown,
    DeletedReference,
    Ancestor,
    OutsideScope,
}

impl TargetRefusal {
    fn whole_call(&self, target: &str, scope: &ControlScope) -> String {
        match self {
            Self::Unknown => format!("unknown thread: {target}"),
            Self::DeletedReference => format!(
                "referenced thread {target} was deleted; what you already read cannot be recalled"
            ),
            Self::Ancestor => format!(
                "refused: thread {target} orchestrates this thread; a child may not read or act \
                 on its parent"
            ),
            Self::OutsideScope => format!(
                "refused: thread {target} is outside your grant's scope ({})",
                scope.describe()
            ),
        }
    }

    fn per_item(&self, target: &str, scope: &ControlScope) -> String {
        match self {
            Self::Unknown => "unknown_thread".to_owned(),
            Self::DeletedReference => format!("deleted: referenced thread {target} was deleted"),
            Self::Ancestor => "ancestor: a child may not read or act on its parent".to_owned(),
            Self::OutsideScope => {
                format!("outside_scope: your grant's scope is {}", scope.describe())
            }
        }
    }
}
