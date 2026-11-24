use std::pin::Pin;

use super::*;
use crate::app::orchestrate::native_provider_named;
use crate::app::sessions::{directory_within, fork_refusal, read_attachments};
use control_mcp::{
    ActionResponse, BatchResponse, BatchResult, ControlThreadParams, ControlThreadResponse,
    CreateProjectParams, CreateProjectResponse, GitOp, GitParams, MoveThreadParams,
    MoveThreadResponse, Outcome, PullRequestOp, PullRequestParams, RunChange, RunOp, RunResult,
    SendMessageParams, SendMode, StartThreadParams, StartThreadResponse, Workspace,
};
use tcode_core::control::{ControlCategory, StartedBy, action};
use tcode_core::pull_request::{PullRequestKey, PullRequestSource};
use tcode_protocol::{Command, CommandResponse, PullRequestAction, PullRequestActionResult};

/// How long a `git` or `pull_request` call waits for its work before answering that it is
/// still running; below the broker's own timeout, which would report a failure instead.
const ACTION_WAIT: Duration = Duration::from_secs(45);

type ActionWork = Pin<Box<dyn Future<Output = Result<Option<serde_json::Value>, String>> + Send>>;

fn batch(target: &str, outcome: Outcome, reason: impl Into<String>) -> BatchResult {
    BatchResult {
        target: target.to_owned(),
        outcome,
        reason: Some(reason.into()),
        side_effects: None,
    }
}

fn to_json(value: impl serde::Serialize) -> Result<serde_json::Value, String> {
    serde_json::to_value(value).map_err(|error| error.to_string())
}

fn request_key(caller: &Caller, tool: &str, request_id: &str) -> String {
    format!("{}/{tool}/{request_id}", caller.thread_id)
}

fn action_now(thread_id: &str, outcome: Outcome, reason: impl Into<String>) -> Answer {
    Answer::Now(to_json(ActionResponse {
        thread_id: thread_id.to_owned(),
        outcome,
        reason: Some(reason.into()),
        output: None,
    }))
}

/// Answer once `work` finishes, or after [`ACTION_WAIT`] with a note that it still runs.
fn action_later(thread_id: String, work: ActionWork, cx: &HostCx) -> Answer {
    Answer::Later(cx.spawn_background(async move {
        let finished = async { Some(work.await) };
        let waited = async {
            smol::Timer::after(ACTION_WAIT).await;
            None
        };
        let response = match smol::future::or(finished, waited).await {
            Some(Ok(output)) => ActionResponse {
                thread_id,
                outcome: Outcome::Applied,
                reason: None,
                output,
            },
            Some(Err(reason)) => ActionResponse {
                thread_id,
                outcome: Outcome::Failed,
                reason: Some(reason),
                output: None,
            },
            None => ActionResponse {
                thread_id,
                outcome: Outcome::Applied,
                reason: Some(format!(
                    "started; still running after {}s, its result shows in the thread",
                    ACTION_WAIT.as_secs()
                )),
                output: None,
            },
        };
        to_json(response)
    }))
}

/// A linked pull request of `meta` named by its URL, `owner/repo#number` or
/// `host/owner/repo#number`.
fn linked_key(meta: &SessionMeta, name: &str) -> Option<PullRequestKey> {
    let name = name.trim().to_ascii_lowercase();
    meta.pull_requests
        .iter()
        .filter(|link| link.visible())
        .find(|link| {
            let key = &link.key;
            link.url.to_ascii_lowercase() == name
                || format!("{}#{}", key.repository, key.number) == name
                || format!("{}/{}#{}", key.host, key.repository, key.number) == name
        })
        .map(|link| link.key.clone())
}

/// The refusal for an option that decides what runs without the user's approval, which only
/// the user changes.
fn approval_option_refusal(provider: ProviderKind, id: &str) -> Option<String> {
    agent::option_governs_approvals(provider, id).then(|| {
        format!("refused: option {id} sets the permission mode, which only the user changes")
    })
}

/// `cwd`, relative to `root`, as a directory inside it; absolute paths, `..` and links that
/// lead out are refused.
fn cwd_inside(root: &Path, cwd: &str) -> Result<PathBuf, String> {
    let relative = Path::new(cwd);
    let outside = || "refused: cwd must be inside the project".to_string();
    if relative.is_absolute()
        || relative.components().any(|part| {
            !matches!(
                part,
                std::path::Component::Normal(_) | std::path::Component::CurDir
            )
        })
    {
        return Err(outside());
    }
    let joined = root.join(relative);
    match directory_within(root, &joined) {
        None => Err(format!(
            "invalid cwd: {} is not a directory on this host",
            joined.display()
        )),
        Some(false) => Err(outside()),
        Some(true) => Ok(joined),
    }
}

impl AppState {
    /// `paths` as attachments the caller may send: the same rule a space client's attachment
    /// passes, over the projects the caller's grant reaches.
    fn agent_attachments(
        &self,
        caller: &Caller,
        paths: Option<Vec<String>>,
        cx: &HostCx,
    ) -> Result<Vec<PathBuf>, String> {
        let projects: Vec<String> = self
            .projects
            .iter()
            .filter(|project| {
                caller
                    .grant
                    .scope
                    .reaches(caller.project_id.as_deref(), Some(&project.id))
            })
            .map(|project| project.id.clone())
            .collect();
        paths
            .unwrap_or_default()
            .into_iter()
            .map(PathBuf::from)
            .map(|path| {
                self.authorize_attachment(&path, &projects, cx)
                    .map(|()| path.clone())
                    .map_err(|_| {
                        format!(
                            "refused: attachment {} is not in the attachments folder of a \
                             thread in your scope",
                            path.display()
                        )
                    })
            })
            .collect()
    }

    /// Why an agent may not write to `target`, if it may not: archived threads, read-only
    /// subagent mirrors and unsent drafts, which belong to their composer.
    fn agent_write_refusal(&self, target: &str) -> Option<String> {
        let Some(meta) = self.find_meta(target) else {
            return Some("unknown thread".into());
        };
        if meta.archived_at.is_some() {
            return Some("archived: unarchive the thread first".into());
        }
        if Self::conversation_read_only(&meta) {
            return Some("read-only: a native subagent's mirror takes no writes".into());
        }
        if self.resident(target).is_some_and(|session| session.draft) {
            return Some("draft: an unsent draft belongs to its composer".into());
        }
        None
    }

    /// The owners the pipe calls act on resident threads; a stored one is loaded in the
    /// background, as a message to it would be.
    fn ensure_resident(&mut self, target: &str, cx: &mut HostCx) {
        if self.resident(target).is_none()
            && let Some(meta) = self.find_meta(target)
        {
            self.load_background_session(meta, cx);
        }
    }

    pub(super) fn op_start_thread(
        &mut self,
        caller: &Caller,
        params: StartThreadParams,
        cx: &mut HostCx,
    ) -> Result<serde_json::Value, String> {
        if params.request_id.trim().is_empty() {
            return Err("request_id must not be empty".into());
        }
        if let Some(meta) = self.sessions.iter().find(|meta| {
            meta.started_by.as_ref().is_some_and(|by| {
                by.thread_id == caller.thread_id && by.request_id == params.request_id
            })
        }) {
            return to_json(StartThreadResponse {
                thread_id: meta.id.clone(),
                created: false,
                title: meta.title.clone(),
                cwd: meta.cwd.display().to_string(),
            });
        }
        let attachment_paths = self.agent_attachments(caller, params.attachments, cx)?;
        if params.message.trim().is_empty() && attachment_paths.is_empty() {
            return Err("message is empty".into());
        }
        let project = self
            .projects
            .iter()
            .find(|project| project.id == params.project_id)
            .cloned()
            .ok_or_else(|| format!("unknown project: {}", params.project_id))?;
        if !caller
            .grant
            .scope
            .reaches(caller.project_id.as_deref(), Some(project.id.as_str()))
        {
            return Err(format!(
                "refused: project {} is outside your grant's scope ({})",
                project.id,
                caller.grant.scope.describe()
            ));
        }
        if params.workspace == Some(Workspace::Worktree) {
            return Err(
                "refused: workspace worktree is not available to start_thread; Tcode \
                        makes a thread's worktree only from a draft's first send. Use local."
                    .into(),
            );
        }
        let provider = native_provider_named(&params.provider).ok_or_else(|| {
            format!(
                "unknown provider: {}; start_thread starts the native providers list_providers lists",
                params.provider.trim()
            )
        })?;
        let profile_id = params
            .profile
            .map(|id| id.trim().to_owned())
            .filter(|id| !id.is_empty() && !Settings::is_builtin_profile_id(id));
        if let Some(id) = &profile_id {
            match self.settings.resolved_profile(id) {
                None => return Err(format!("unknown profile: {id}")),
                Some(profile) if profile.kind != provider => {
                    return Err(format!("profile {id} belongs to another provider"));
                }
                Some(_) => {}
            }
        }
        let cwd = match params.cwd {
            None => project.root.clone(),
            Some(cwd) => cwd_inside(&project.root, &cwd)?,
        };
        if !cwd.is_dir() {
            return Err(format!(
                "invalid cwd: {} is not a directory on this host",
                cwd.display()
            ));
        }
        let options = params.options.unwrap_or_default();
        if let Some(refusal) = options
            .keys()
            .find_map(|id| approval_option_refusal(provider, id))
        {
            return Err(refusal);
        }
        let model = params
            .model
            .map(|model| model.trim().to_owned())
            .filter(|model| !model.is_empty());
        let mut meta = SessionMeta::new(provider, cwd, model);
        meta.project_id = Some(project.id.clone());
        meta.profile_id = profile_id;
        if let Some(title) = params
            .title
            .map(|title| title.trim().to_owned())
            .filter(|title| !title.is_empty())
        {
            meta.title = title;
        }
        meta.option_selections = options
            .into_iter()
            .map(|(id, value)| OptionSelection { id, value })
            .collect();
        meta.option_selections
            .extend(self.initial_permission_selection(Some(&project.id), provider));
        meta.started_by = Some(StartedBy {
            thread_id: caller.thread_id.clone(),
            request_id: params.request_id,
        });
        let response = StartThreadResponse {
            thread_id: meta.id.clone(),
            created: true,
            title: meta.title.clone(),
            cwd: meta.cwd.display().to_string(),
        };
        let id = self.install_background_thread(
            meta,
            params.message,
            read_attachments(attachment_paths),
            cx,
        );
        self.audit(caller, &id, action::START_THREAD, None, cx);
        to_json(response)
    }

    pub(super) fn op_send_message(
        &mut self,
        caller: &Caller,
        params: SendMessageParams,
        cx: &mut HostCx,
    ) -> Result<serde_json::Value, String> {
        let key = params
            .request_id
            .as_deref()
            .map(|id| request_key(caller, "send_message", id));
        if let Some(answer) = key.as_deref().and_then(|key| self.answered(key)) {
            return Ok(answer);
        }
        let attachment_paths = self.agent_attachments(caller, params.attachments, cx)?;
        if params.text.trim().is_empty() && attachment_paths.is_empty() {
            return Err("text is empty".into());
        }
        let mut results = Vec::new();
        for target in &params.thread_ids {
            let attachments = read_attachments(attachment_paths.clone());
            results.push(self.send_agent_message(
                caller,
                target,
                params.text.clone(),
                attachments,
                &params.mode,
                cx,
            ));
        }
        let answer = to_json(BatchResponse { results })?;
        if let Some(key) = key {
            self.remember_answer(key, answer.clone());
        }
        Ok(answer)
    }

    fn send_agent_message(
        &mut self,
        caller: &Caller,
        target: &str,
        text: String,
        attachments: Vec<Attachment>,
        mode: &SendMode,
        cx: &mut HostCx,
    ) -> BatchResult {
        if let Some(reason) = self
            .batch_target(caller, target, false)
            .err()
            .or_else(|| self.agent_write_refusal(target))
        {
            return batch(target, Outcome::Skipped, reason);
        }
        self.ensure_resident(target, cx);
        let command = match mode {
            SendMode::Schedule { at } => Command::ScheduleTurn {
                session_id: target.to_owned(),
                text: text.clone(),
                attachment_paths: Vec::new(),
                fire_at_unix_secs: *at,
            },
            SendMode::Queue | SendMode::Steer => Command::SendTurn {
                session_id: target.to_owned(),
                text: text.clone(),
                attachment_paths: Vec::new(),
            },
        };
        if let Err(error) = self.validate_command_target(&command) {
            return batch(target, Outcome::Skipped, error.message);
        }
        let mut side_effects = Vec::new();
        if self.find_meta(target).is_some_and(|meta| meta.is_settled()) {
            side_effects.push("unsettled: a message reopens a settled thread".to_owned());
        }
        let routing = self.resident(target).map(|session| session.route(true));
        let delivery = match mode {
            SendMode::Steer if routing == Some(SendRouting::Steer) => "steered",
            SendMode::Schedule { .. } => "scheduled",
            SendMode::Steer => {
                side_effects.push(
                    if routing == Some(SendRouting::QueueUnsupported) {
                        "the provider cannot steer this running turn; queued instead"
                    } else {
                        "no turn was running; queued instead"
                    }
                    .to_owned(),
                );
                "queued"
            }
            SendMode::Queue => "queued",
        };
        let key = match delivery {
            "steered" => action::SEND_STEERED,
            "scheduled" => action::SEND_SCHEDULED,
            _ => action::SEND_QUEUED,
        };
        self.audit(caller, target, key, None, cx);
        match (mode, delivery) {
            (_, "steered") => self.steer_assembled(target, text, attachments, cx),
            (SendMode::Schedule { at }, _) => {
                self.schedule_agent_message(target, text, attachments, *at, cx)
            }
            _ => self.queue_agent_message(target, text, attachments, cx),
        }
        BatchResult {
            target: target.to_owned(),
            outcome: Outcome::Applied,
            reason: Some(delivery.to_owned()),
            side_effects: (!side_effects.is_empty()).then_some(side_effects),
        }
    }

    fn queue_agent_message(
        &mut self,
        target: &str,
        text: String,
        attachments: Vec<Attachment>,
        cx: &mut HostCx,
    ) {
        self.reactivate_session(target, cx);
        let default_window = self.default_context_window(target);
        self.queue_automatic_turn(
            target,
            |session| {
                session.push_queued(text, attachments);
                session.stamp_context_window_change(default_window);
                session.queue.last_mut().unwrap().origin = MessageOrigin::Agent;
            },
            cx,
        );
    }

    fn schedule_agent_message(
        &mut self,
        target: &str,
        text: String,
        attachments: Vec<Attachment>,
        at: u64,
        cx: &mut HostCx,
    ) {
        let default_window = self.default_context_window(target);
        let Some(session) = self.resident_mut(target) else {
            return;
        };
        session.push_scheduled(text, attachments, UNIX_EPOCH + Duration::from_secs(at));
        session.stamp_context_window_change(default_window);
        session.queue.last_mut().unwrap().origin = MessageOrigin::Agent;
        let idle = matches!(session.runtime, Runtime::Idle);
        self.reactivate_session(target, cx);
        if idle {
            self.ensure_started(target, cx);
        }
        self.reschedule_scheduled_wake(cx);
    }

    pub(super) fn op_control_thread(
        &mut self,
        caller: &Caller,
        params: ControlThreadParams,
        cx: &mut HostCx,
    ) -> Result<serde_json::Value, String> {
        let key = params
            .request_id
            .as_deref()
            .map(|id| request_key(caller, "control_thread", id));
        if let Some(answer) = key.as_deref().and_then(|key| self.answered(key)) {
            return Ok(answer);
        }
        let mut results = Vec::new();
        for change in params.changes {
            results.push(self.run_change(caller, change, cx));
        }
        let answer = to_json(ControlThreadResponse { results })?;
        if let Some(key) = key {
            self.remember_answer(key, answer.clone());
        }
        Ok(answer)
    }

    fn run_change(&mut self, caller: &Caller, change: RunChange, cx: &mut HostCx) -> RunResult {
        let target = change.thread_id;
        let (result, new_thread_id) = match self.apply_run_op(caller, &target, change.op, cx) {
            Ok((done, new_thread_id)) => (batch(&target, Outcome::Applied, done), new_thread_id),
            Err((outcome, reason)) => (batch(&target, outcome, reason), None),
        };
        RunResult {
            result,
            new_thread_id,
        }
    }

    /// One `control_thread` change through the owner the pipe's command calls: what happened
    /// and a fork's new id, or the outcome and reason it did not apply.
    fn apply_run_op(
        &mut self,
        caller: &Caller,
        target: &str,
        op: RunOp,
        cx: &mut HostCx,
    ) -> Result<(&'static str, Option<String>), (Outcome, String)> {
        let skipped = |reason: String| (Outcome::Skipped, reason);
        let failed = |reason: String| (Outcome::Failed, reason);
        if let Some(reason) = self
            .batch_target(caller, target, false)
            .err()
            .or_else(|| self.agent_write_refusal(target))
        {
            return Err(skipped(reason));
        }
        self.ensure_resident(target, cx);
        let meta = self
            .find_meta(target)
            .ok_or_else(|| skipped("unknown thread".into()))?;
        let session_id = target.to_owned();
        let (command, key, detail) = match &op {
            RunOp::Interrupt => (Command::Interrupt { session_id }, action::INTERRUPT, None),
            RunOp::Retry { turn } => (
                Command::RetryTurn {
                    session_id,
                    turn_id: turn.clone(),
                },
                action::RETRY,
                Some(turn.clone()),
            ),
            RunOp::Compact => {
                if !meta.provider.caps().manual_compaction {
                    return Err(skipped(format!(
                        "{} has no manual compaction",
                        meta.provider.display_name()
                    )));
                }
                (
                    Command::CompactContext { session_id },
                    action::COMPACT,
                    None,
                )
            }
            RunOp::SetModel { model } => (
                Command::SetActiveModel {
                    session_id,
                    provider: meta.provider,
                    model: Some(model.clone()),
                    profile_id: meta.profile_id.clone(),
                },
                action::SET_MODEL,
                Some(model.clone()),
            ),
            RunOp::SetOption { id, value } => {
                if let Some(refusal) = approval_option_refusal(meta.provider, id) {
                    return Err(failed(refusal));
                }
                (
                    Command::SetActiveOption {
                        session_id,
                        id: id.clone(),
                        value: Some(value.clone()),
                    },
                    action::SET_OPTION,
                    Some(id.clone()),
                )
            }
            RunOp::Fork => {
                if let Some(refusal) = fork_refusal(Self::session_fork_availability(
                    &meta,
                    self.resident(target),
                )) {
                    return Err(failed(refusal.into()));
                }
                (Command::ForkThread { id: session_id }, action::FORK, None)
            }
            RunOp::SteerQueued { id } | RunOp::DropQueued { id } => {
                let id = *id;
                let (origin, editable) = self
                    .resident(target)
                    .and_then(|session| {
                        let message = session.queue.iter().find(|message| message.id == id)?;
                        Some((message.origin, Self::queued_message_editable(session, id)))
                    })
                    .ok_or_else(|| skipped(format!("no queued message {id}")))?;
                if !editable {
                    return Err(skipped(format!(
                        "queued message {id} is being delivered or is the host's own"
                    )));
                }
                if matches!(op, RunOp::DropQueued { .. }) {
                    if origin == MessageOrigin::Human
                        && !caller.grant.allows_category(ControlCategory::HistoryEdit)
                    {
                        return Err(failed(
                            "refused: dropping a queued human message needs history_edit".into(),
                        ));
                    }
                    (
                        Command::DropQueued { session_id, id },
                        action::DROP_QUEUED,
                        Some(id.to_string()),
                    )
                } else {
                    (
                        Command::SteerQueued { session_id, id },
                        action::STEER_QUEUED,
                        Some(id.to_string()),
                    )
                }
            }
        };
        self.validate_command_target(&command)
            .map_err(|error| skipped(error.message))?;
        let mut new_thread_id = None;
        let done = match command {
            Command::Interrupt { .. } => {
                let running = self
                    .resident(target)
                    .is_some_and(|session| session.turn_in_flight || session.timeline.turn_running);
                self.interrupt(target, cx)
                    .map_err(|error| failed(error.message))?;
                if running {
                    "interrupted"
                } else {
                    "nothing was running"
                }
            }
            Command::RetryTurn { turn_id, .. } => {
                self.retry_turn(target, turn_id)
                    .map_err(|error| failed(error.message))?;
                "retrying"
            }
            Command::CompactContext { .. } => {
                self.compact_context(target, cx)
                    .map_err(|error| failed(error.message))?;
                "compacting"
            }
            Command::SetActiveModel {
                provider,
                model,
                profile_id,
                ..
            } => {
                self.set_active_model(target, provider, model, profile_id, cx);
                "model set; it takes effect from the next turn"
            }
            Command::SetActiveOption { id, value, .. } => {
                self.set_active_option(target, &id, value, cx);
                "option set"
            }
            Command::ForkThread { .. } => {
                let fork = self
                    .fork_thread(target, cx)
                    .ok_or_else(|| failed("the fork could not be made".into()))?;
                new_thread_id = Some(fork);
                "forked"
            }
            Command::SteerQueued { id, .. } => {
                self.steer_queued(target, id, cx);
                "steered"
            }
            Command::DropQueued { id, .. } => {
                self.drop_queued(target, id, cx);
                "dropped"
            }
            _ => unreachable!("every run op maps to one of these commands"),
        };
        self.audit(caller, target, key, detail, cx);
        Ok((done, new_thread_id))
    }

    pub(super) fn op_pull_request(
        &mut self,
        caller: &Caller,
        params: PullRequestParams,
        cx: &mut HostCx,
    ) -> Answer {
        let thread = params.thread_id;
        if let Some(reason) = self.agent_write_refusal(&thread) {
            return action_now(&thread, Outcome::Skipped, reason);
        }
        let Some(meta) = self.find_meta(&thread) else {
            return action_now(&thread, Outcome::Skipped, "unknown thread");
        };
        let name = params.reference.or(params.key);
        if let PullRequestOp::Link = params.op {
            let Some(reference) = name else {
                return Answer::Now(Err(
                    "link needs reference: a pull request URL or owner/repo#number".into(),
                ));
            };
            let command = Command::LinkPullRequest {
                session_id: thread.clone(),
                reference: reference.clone(),
            };
            if let Err(error) = self.validate_command_target(&command) {
                return action_now(&thread, Outcome::Skipped, error.message);
            }
            self.audit(
                caller,
                &thread,
                action::PR_LINK,
                Some(reference.clone()),
                cx,
            );
            let linking = self.link_pull_request(&thread, reference, PullRequestSource::Agent, cx);
            return action_later(
                thread,
                Box::pin(async move {
                    let response = linking.await.map_err(|error| error.message)?;
                    Ok(Some(to_json(response)?))
                }),
                cx,
            );
        }
        let Some(key) = name.as_deref().and_then(|name| linked_key(&meta, name)) else {
            return Answer::Now(Err(format!(
                "no pull request linked to thread {thread} matches {}; pass its URL or \
                 owner/repo#number",
                name.as_deref().unwrap_or("nothing")
            )));
        };
        let label = Some(format!("{}#{}", key.repository, key.number));
        let audit_key = match &params.op {
            PullRequestOp::Link => action::PR_LINK,
            PullRequestOp::Unlink => action::PR_UNLINK,
            PullRequestOp::Watch => action::PR_WATCH,
            PullRequestOp::Unwatch => action::PR_UNWATCH,
            PullRequestOp::Refresh => action::PR_REFRESH,
            PullRequestOp::Comment { .. } => action::PR_COMMENT,
            PullRequestOp::Reply { .. } => action::PR_REPLY,
            PullRequestOp::Resolve { .. } => action::PR_RESOLVE,
        };
        let action = match params.op {
            PullRequestOp::Link => unreachable!("linking is answered above"),
            op @ (PullRequestOp::Unlink | PullRequestOp::Watch | PullRequestOp::Unwatch) => {
                let command = match op {
                    PullRequestOp::Unlink => Command::UnlinkPullRequest {
                        session_id: thread.clone(),
                        key: key.clone(),
                    },
                    _ => Command::WatchPullRequest {
                        session_id: thread.clone(),
                        key: key.clone(),
                        watching: op == PullRequestOp::Watch,
                    },
                };
                if let Err(error) = self.validate_command_target(&command) {
                    return action_now(&thread, Outcome::Skipped, error.message);
                }
                let result = match command {
                    Command::WatchPullRequest { watching, .. } => self
                        .watch_pull_request(&thread, &key, watching, cx)
                        .map(|_| ()),
                    _ => {
                        self.unlink_pull_request(&thread, &key, cx);
                        Ok(())
                    }
                };
                return match result {
                    Ok(()) => {
                        self.audit(caller, &thread, audit_key, label, cx);
                        Answer::Now(to_json(ActionResponse {
                            thread_id: thread,
                            outcome: Outcome::Applied,
                            reason: None,
                            output: None,
                        }))
                    }
                    Err(error) => action_now(&thread, Outcome::Failed, error.message),
                };
            }
            PullRequestOp::Refresh => {
                let command = Command::RefreshPullRequest {
                    session_id: thread.clone(),
                    key: key.clone(),
                };
                if let Err(error) = self.validate_command_target(&command) {
                    return action_now(&thread, Outcome::Skipped, error.message);
                }
                return match self.refresh_pull_request(&thread, key, cx) {
                    Ok(()) => action_now(&thread, Outcome::Applied, "refresh requested"),
                    Err(error) => action_now(&thread, Outcome::Failed, error.message),
                };
            }
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
        };
        let command = Command::RunPullRequestAction {
            session_id: thread.clone(),
            key: key.clone(),
            action: action.clone(),
        };
        if let Err(error) = self.validate_command_target(&command) {
            return action_now(&thread, Outcome::Skipped, error.message);
        }
        self.audit(caller, &thread, audit_key, label, cx);
        let writing = self.run_pull_request_action(&thread, key, action, cx);
        action_later(
            thread,
            Box::pin(async move {
                let response = writing.await.map_err(|error| error.message)?;
                if let CommandResponse::PullRequestAction(PullRequestActionResult::Rejected(
                    rejection,
                )) = &response
                {
                    return Err(format!("rejected: {}", to_json(rejection)?));
                }
                Ok(Some(to_json(response)?))
            }),
            cx,
        )
    }

    /// Checkout and commit refuse while a turn runs in the target thread, so they never act on
    /// work half made; checkout also refuses uncommitted changes. A thread may commit its own.
    pub(super) fn op_git(&mut self, caller: &Caller, params: GitParams, cx: &mut HostCx) -> Answer {
        let thread = params.thread_id;
        if let Some(reason) = self.agent_write_refusal(&thread) {
            return action_now(&thread, Outcome::Skipped, reason);
        }
        self.ensure_resident(&thread, cx);
        let busy = thread != caller.thread_id
            && self.resident(&thread).is_some_and(Self::checkout_blocked);
        let (action, message, included) = match params.op {
            GitOp::CheckoutBranch { branch } => {
                let command = Command::CheckoutBranch {
                    session_id: thread.clone(),
                    branch: branch.clone(),
                };
                if let Err(error) = self.validate_command_target(&command) {
                    return action_now(&thread, Outcome::Skipped, error.message);
                }
                self.audit(
                    caller,
                    &thread,
                    action::GIT_CHECKOUT,
                    Some(branch.clone()),
                    cx,
                );
                let checkout = self.checkout_branch(&thread, branch, cx);
                return action_later(
                    thread,
                    Box::pin(async move {
                        checkout
                            .recv()
                            .await
                            .map_err(|_| "the checkout was dropped".to_owned())?
                            .map(|()| None)
                    }),
                    cx,
                );
            }
            GitOp::MergeWorktree => {
                let command = Command::MergeWorktree {
                    session_id: thread.clone(),
                };
                if let Err(error) = self.validate_command_target(&command) {
                    return action_now(&thread, Outcome::Skipped, error.message);
                }
                self.audit(caller, &thread, action::GIT_MERGE_WORKTREE, None, cx);
                let merge = self.merge_worktree(&thread, cx);
                return action_later(
                    thread,
                    Box::pin(async move {
                        let notice = merge
                            .recv()
                            .await
                            .map_err(|_| "the merge was dropped".to_owned())?;
                        match notice {
                            RuntimeNotice::WorktreeMergeFailed { .. } => {
                                Err(to_json(&notice)?.to_string())
                            }
                            notice => Ok(Some(to_json(notice)?)),
                        }
                    }),
                    cx,
                );
            }
            GitOp::Commit { message, included } => (GitAction::Commit, Some(message), included),
            GitOp::CommitPush { message, included } => {
                (GitAction::CommitPush, Some(message), included)
            }
            GitOp::Push => (GitAction::Push, None, None),
            GitOp::Pull => (GitAction::Pull, None, None),
            GitOp::PublishBranch => (GitAction::PublishBranch, None, None),
        };
        if busy && matches!(action, GitAction::Commit | GitAction::CommitPush) {
            return action_now(
                &thread,
                Outcome::Skipped,
                "busy: a turn is running in this thread; committing now could take half-made \
                 changes",
            );
        }
        let command = Command::RunGitAction {
            session_id: thread.clone(),
            action,
            message: message.clone(),
            included: included.clone(),
            feature_branch: None,
        };
        if let Err(error) = self.validate_command_target(&command) {
            return action_now(&thread, Outcome::Skipped, error.message);
        }
        let key = match action {
            GitAction::Commit => action::GIT_COMMIT,
            GitAction::CommitPush => action::GIT_COMMIT_PUSH,
            GitAction::Push => action::GIT_PUSH,
            GitAction::Pull => action::GIT_PULL,
            GitAction::PublishBranch => action::GIT_PUBLISH_BRANCH,
            GitAction::InitializeGit => action::GIT_INITIALIZE,
        };
        self.audit(caller, &thread, key, None, cx);
        let running = self.run_git_action(&thread, action, message, included, None, cx);
        action_later(
            thread,
            Box::pin(async move {
                let output = running
                    .recv()
                    .await
                    .map_err(|_| "the git action was dropped".to_owned())??;
                Ok(Some(serde_json::Value::String(output)))
            }),
            cx,
        )
    }

    pub(super) fn op_create_project(
        &mut self,
        caller: &Caller,
        params: CreateProjectParams,
        cx: &mut HostCx,
    ) -> Result<serde_json::Value, String> {
        let root = PathBuf::from(params.root);
        let existed = self.projects.iter().any(|project| project.root == root);
        let command = Command::CreateProject { root: root.clone() };
        self.validate_command_target(&command)
            .map_err(|error| error.message)?;
        let id = self
            .create_project(root, cx)
            .map_err(|error| error.message)?;
        let project = self
            .projects
            .iter()
            .find(|project| project.id == id)
            .cloned()
            .ok_or("the project disappeared")?;
        if !existed {
            self.audit(
                caller,
                &caller.thread_id,
                action::CREATE_PROJECT,
                Some(project.root.display().to_string()),
                cx,
            );
        }
        to_json(CreateProjectResponse {
            project_id: project.id,
            name: project.name,
            root: project.root.display().to_string(),
            created: !existed,
        })
    }

    /// Whether `caller` may move `thread_id` into `project`: into a project its grant reaches,
    /// or, for the caller itself, into a project that has no threads yet, whose history the
    /// move therefore opens to no one.
    fn move_target_refusal(
        &self,
        caller: &Caller,
        thread_id: &str,
        project: &Project,
    ) -> Option<String> {
        if caller
            .grant
            .scope
            .reaches(caller.project_id.as_deref(), Some(project.id.as_str()))
        {
            return None;
        }
        let empty = !self
            .sessions
            .iter()
            .chain(self.residents.live.values().map(|session| &session.meta))
            .chain(self.residents.parked.values().map(|session| &session.meta))
            .any(|meta| meta.project_id.as_deref() == Some(project.id.as_str()));
        if thread_id == caller.thread_id && empty {
            return None;
        }
        Some(format!(
            "refused: project {} is outside your grant's scope ({}){}",
            project.id,
            caller.grant.scope.describe(),
            if thread_id == caller.thread_id {
                " and already has threads"
            } else {
                ""
            }
        ))
    }

    /// Move a thread into a project [`Self::move_target_refusal`] allows; a thread may move
    /// itself, keeping the turn that asked ([`AppState::move_session_by`]).
    pub(super) fn op_move_thread(
        &mut self,
        caller: &Caller,
        params: MoveThreadParams,
        cx: &mut HostCx,
    ) -> Result<serde_json::Value, String> {
        let key = params
            .request_id
            .as_deref()
            .map(|id| request_key(caller, "move_thread", id));
        if let Some(answer) = key.as_deref().and_then(|key| self.answered(key)) {
            return Ok(answer);
        }
        let project = self
            .projects
            .iter()
            .find(|project| project.id == params.project_id)
            .cloned()
            .ok_or_else(|| format!("unknown project: {}", params.project_id))?;
        if let Some(refusal) = self.move_target_refusal(caller, &params.thread_id, &project) {
            return Err(refusal);
        }
        let cwd = params
            .cwd
            .map(|cwd| cwd_inside(&project.root, &cwd))
            .transpose()?;
        let before = self
            .find_meta(&params.thread_id)
            .map(|meta| (meta.project_id, meta.cwd));
        let by_own_turn = params.thread_id == caller.thread_id;
        self.move_session_by(&params.thread_id, &project.id, cwd, by_own_turn, cx)
            .map_err(|error| format!("refused: {}", error.message))?;
        let meta = self
            .find_meta(&params.thread_id)
            .ok_or("the thread disappeared")?;
        if before != Some((meta.project_id.clone(), meta.cwd.clone())) {
            self.audit(
                caller,
                &params.thread_id,
                action::MOVE,
                Some(format!("{} ({})", project.name, project.root.display())),
                cx,
            );
        }
        let answer = to_json(MoveThreadResponse {
            thread_id: meta.id,
            project_id: project.id,
            project_name: project.name,
            cwd: meta.cwd.display().to_string(),
        })?;
        if let Some(key) = key {
            self.remember_answer(key, answer.clone());
        }
        Ok(answer)
    }
}

#[cfg(test)]
#[path = "operate_tests.rs"]
mod tests;
