use super::*;
use crate::app::orchestrate::PullRequestRegistration;
use tcode_core::pull_request::{
    self, PullRequestKey, PullRequestSnapshot, PullRequestSource, PullRequestStackState,
    PullRequestState, PullRequestSyncError,
};
use tcode_protocol::{
    CommandResponse, ProtocolError, PullRequestAction, PullRequestActionResult, PullRequestRead,
    PullRequestReadResponse, QueryResponse,
};
use tcode_services::forge::{Checkout, Forge, ForgeError, ForgeErrorKind, Summary};

pub(super) struct PullRequestRuntime {
    pub(super) forge: Arc<dyn Forge>,
    last_synced: HashMap<PullRequestKey, u64>,
    requested: HashMap<PullRequestKey, u64>,
    generation: u64,
    paused: HashMap<(Option<String>, String), SystemTime>,
    syncing: bool,
    sync_scheduled: bool,
    merge_commands: HashSet<String>,
    pub(super) workers: Vec<HostTask<()>>,
    /// Stacks with a write submitted and not yet answered.
    pub(super) stack_writes: HashSet<super::pull_request_stacks::StackKey>,
    /// Counts the review submissions that have landed, so a conversation read clears an
    /// unanswered one only when it began after it.
    submissions: u64,
}
impl PullRequestRuntime {
    pub(super) fn new(forge: Arc<dyn Forge>) -> Self {
        Self {
            forge,
            last_synced: HashMap::new(),
            requested: HashMap::new(),
            generation: 0,
            paused: HashMap::new(),
            syncing: false,
            sync_scheduled: false,
            merge_commands: HashSet::new(),
            workers: Vec::new(),
            stack_writes: HashSet::new(),
            submissions: 0,
        }
    }
}
struct SyncGroup {
    key: PullRequestKey,
    projects: Vec<Option<String>>,
    threads: Vec<String>,
    observations: Vec<(Option<PullRequestSnapshot>, PullRequestStackState)>,
    request: Option<u64>,
}

pub(super) fn failure(message: impl Into<String>) -> ProtocolError {
    ProtocolError {
        code: "pull_request_failed".into(),
        message: message.into(),
    }
}
/// Codes a client localizes; the message is the transport's display, which carries no request
/// values.
fn read_error(error: ForgeError) -> ProtocolError {
    let code = match &error.kind {
        ForgeErrorKind::HostDisabled => "pull_request_host_disabled",
        ForgeErrorKind::NoCredential | ForgeErrorKind::Unauthorized => "pull_request_no_credential",
        ForgeErrorKind::RateLimited { .. } | ForgeErrorKind::Paused { .. } => {
            "pull_request_rate_limited"
        }
        ForgeErrorKind::NotFound => "pull_request_not_found",
        ForgeErrorKind::TooLarge => "pull_request_too_large",
        ForgeErrorKind::InvalidInput => "pull_request_invalid_read",
        ForgeErrorKind::UnsupportedMedia => "pull_request_unsupported_media",
        ForgeErrorKind::Deadline => "pull_request_deadline",
        _ => "pull_request_failed",
    };
    ProtocolError {
        code: code.into(),
        message: error.to_string(),
    }
}
fn resolve_reference(
    forge: &dyn Forge,
    hosts: &str,
    cwd: &Path,
    reference: &str,
) -> Result<(PullRequestKey, String), ProtocolError> {
    if let Some(target) = forge.pull_request_url(reference) {
        return Ok(target);
    }
    let invalid = || ProtocolError {
        code: "pull_request_invalid_reference".into(),
        message: "Enter a pull request URL or positive number.".into(),
    };
    let number: u64 = reference
        .trim()
        .trim_start_matches('#')
        .parse()
        .map_err(|_| invalid())?;
    if number == 0 {
        return Err(invalid());
    }
    let no_repository = || ProtocolError {
        code: "pull_request_no_repository".into(),
        message: format!("This project has no {hosts} repository. Use a full PR URL."),
    };
    let key = forge
        .checkout_repository(&Checkout::new(cwd))
        .ok_or_else(no_repository)?
        .key(number);
    let url = forge.url(&key).ok_or_else(no_repository)?;
    Ok((key, url))
}
impl AppState {
    pub(crate) fn pump_pull_request_requests(
        &mut self,
        server: Option<pull_request_mcp::PullRequestMcpServer>,
        cx: &mut HostCx,
    ) {
        let Some(server) = server else { return };
        self.mcp.pull_request_url = Some(server.url);
        self.mcp.pull_request_tokens = Some(server.tokens);
        self.mcp.pull_request_hosts = Some(server.hosts);
        self.name_pull_request_hosts();
        let host = cx.clone();
        cx.spawn_detached(async move {
            while let Ok(request) = server.requests.recv().await {
                host.enqueue(move |state, cx| state.handle_pull_request_request(request, cx));
            }
        });
    }
    pub(super) fn pull_request_registration_for(
        &mut self,
        meta: &SessionMeta,
    ) -> Option<agent::McpRegistration> {
        if let Some(existing) = self.mcp.pull_request_registrations.get(&meta.id) {
            return Some(existing.registration.clone());
        }
        let url = self.mcp.pull_request_url.clone()?;
        let bearer_token = self.mcp.pull_request_tokens.as_ref()?.register(&meta.id);
        let names = self
            .mcp
            .pull_request_hosts
            .as_ref()
            .map(|names| names.get())
            .unwrap_or_default();
        let registration = agent::McpRegistration {
            name: agent::McpRegistration::SERVER_NAME_PULL_REQUESTS.into(),
            url,
            bearer_token,
        };
        let instructions = pull_request::linking_instructions(&self.pull_request_hosts());
        self.mcp.pull_request_registrations.insert(
            meta.id.clone(),
            PullRequestRegistration {
                registration: registration.clone(),
                hosts: names,
                instructions,
            },
        );
        Some(registration)
    }
    /// Whether a provider of this kind receives the pull request tools when it starts.
    pub(super) fn pull_request_tools_offered(&self, provider: ProviderKind) -> bool {
        provider.caps().mcp_servers && self.mcp.pull_request_url.is_some()
    }
    /// The linking instructions the thread's provider is given: its registration's, else the
    /// ones a registration made now would have.
    pub(super) fn pull_request_instructions(&self, session_id: &str) -> String {
        self.mcp
            .pull_request_registrations
            .get(session_id)
            .map(|existing| existing.instructions.clone())
            .unwrap_or_else(|| pull_request::linking_instructions(&self.pull_request_hosts()))
    }
    /// The host names the thread's pull request tools are described with: its registration's,
    /// else the ones a registration made now would have.
    pub(super) fn pull_request_tool_hosts(&self, session_id: &str) -> String {
        self.mcp
            .pull_request_registrations
            .get(session_id)
            .map(|existing| existing.hosts.clone())
            .unwrap_or_else(|| pull_request::host_names(&self.pull_request_hosts()))
    }
    /// The hosts whose terms Tcode's text to the model names: GitHub's, and every configured
    /// host's kind.
    pub(super) fn pull_request_hosts(&self) -> Vec<&'static pull_request::HostTerms> {
        pull_request::hosts_in(
            self.settings
                .source_control
                .hosts
                .values()
                .map(|choice| choice.kind),
        )
    }
    pub(super) fn revoke_pull_request_registration(&mut self, session_id: &str) {
        if let Some(existing) = self.mcp.pull_request_registrations.remove(session_id)
            && let Some(tokens) = &self.mcp.pull_request_tokens
        {
            tokens.revoke(&existing.registration.bearer_token);
        }
    }
    pub(super) fn handle_pull_request_request(
        &mut self,
        request: pull_request_mcp::BrokerRequest,
        cx: &mut HostCx,
    ) {
        use pull_request_mcp::Operation;
        let Some(meta) = self.find_meta(&request.session_id) else {
            let _ = request.reply.try_send(Err("Unknown thread.".into()));
            return;
        };
        if !self.mcp.pull_request_registrations.contains_key(&meta.id) {
            let _ = request.reply.try_send(Err(
                "Pull request tools are unavailable for this thread.".into(),
            ));
            return;
        }
        if matches!(request.operation, Operation::List) {
            let mut positions = HashMap::new();
            let chains: Vec<_> = pull_request::groups(&meta.pull_requests)
                .iter()
                .map(|group| {
                    let kind = match group.kind {
                        pull_request::PullRequestGroupKind::Native => "native",
                        pull_request::PullRequestGroupKind::Derived => "derived",
                        pull_request::PullRequestGroupKind::Single => "single",
                    };
                    if group.links.len() > 1 {
                        for (position, link) in group.links.iter().enumerate() {
                            positions.insert(
                                link.key.clone(),
                                serde_json::json!({
                                    "kind": kind,
                                    "position": position + 1,
                                    "size": group.links.len(),
                                }),
                            );
                        }
                    }
                    let numbers: Vec<_> = group.links.iter().map(|link| link.key.number).collect();
                    serde_json::json!({ "kind": kind, "numbers": numbers })
                })
                .collect();
            let rows: Vec<_> = meta
                .pull_requests
                .iter()
                .filter(|link| link.visible())
                .map(|link| {
                    let snapshot = link.snapshot.as_ref();
                    serde_json::json!({
                        "host": link.key.host,
                        "repository": link.key.repository,
                        "number": link.key.number,
                        "url": link.url,
                        "source": link.source,
                        "state": snapshot.map(|snapshot| snapshot.state),
                        "title": snapshot.map(|snapshot| &snapshot.title),
                        "headBranch": snapshot.map(|snapshot| &snapshot.head_branch),
                        "baseBranch": snapshot.map(|snapshot| &snapshot.base_branch),
                        "isDraft": snapshot.map(|snapshot| snapshot.is_draft),
                        "stack": positions.get(&link.key),
                    })
                })
                .collect();
            let _ = request
                .reply
                .try_send(Ok(serde_json::json!({"pullRequests":rows,"chains":chains})));
            return;
        }
        // `Some(linking)` links or unlinks; `None(watching)` starts or stops a watch.
        let (target, linking, watching) = match request.operation {
            Operation::Link(target) => (target, Some(true), false),
            Operation::Unlink(target) => (target, Some(false), false),
            Operation::Watch(target) => (target, None, true),
            Operation::Unwatch(target) => (target, None, false),
            Operation::List => unreachable!(),
        };
        let cwd = self.pull_request_project_cwd(&meta);
        let forge = self.pull_requests.forge.clone();
        let hosts = pull_request::host_names(&self.pull_request_hosts());
        let host = cx.clone();
        cx.spawn_detached(async move {
            let target = host
                .unblock(move || {
                    if let Some(url) = target.url {
                        return forge
                            .pull_request_url(&url)
                            .ok_or_else(|| "Invalid pull request URL.".to_owned());
                    }
                    let missing = || "Pass url or repository plus number.".to_owned();
                    let number = target.number.filter(|n| *n > 0).ok_or_else(missing)?;
                    let name = target.repository.ok_or_else(missing)?;
                    let host = target
                        .host
                        .or_else(|| {
                            forge
                                .checkout_repository(&Checkout::new(&cwd))
                                .map(|r| r.host)
                        })
                        .ok_or_else(|| "Pass host or a full PR URL.".to_owned())?;
                    let invalid = || format!("Invalid {hosts} repository.");
                    let key = forge
                        .repository(&name, &host)
                        .ok_or_else(invalid)?
                        .key(number);
                    let url = forge.url(&key).ok_or_else(invalid)?;
                    Ok((key, url))
                })
                .await;
            host.enqueue(move |state, cx| {
                let result = target.and_then(|(key, url)| {
                    let Some(linking) = linking else {
                        return state.watch_pull_request_from_agent(
                            &request.session_id,
                            key,
                            url,
                            watching,
                            cx,
                        );
                    };
                    let linked = state
                        .find_meta(&request.session_id)
                        .ok_or_else(|| "Thread disappeared.".to_owned())?
                        .pull_requests
                        .iter()
                        .any(|link| link.visible() && link.key == key);
                    if linking {
                        state.apply_pull_request_link(
                            &request.session_id,
                            key.clone(),
                            url.clone(),
                            PullRequestSource::Agent,
                            true,
                            cx,
                        )?;
                    } else {
                        state.unlink_pull_request(&request.session_id, &key, cx);
                    }
                    Ok(serde_json::json!({
                        "host": key.host,
                        "repository": key.repository,
                        "number": key.number,
                        "url": url,
                        "alreadyLinked": linking && linked,
                        "wasLinked": !linking && linked,
                    }))
                });
                let _ = request.reply.try_send(result);
            });
        });
    }
    /// Reads answer only for a pull request the thread shows, linked or a layer of a linked
    /// stack, so a client reads nothing through a thread it can see that the thread does not
    /// show.
    fn linked_pull_request(
        &self,
        session_id: &str,
        key: &PullRequestKey,
    ) -> Result<(), ProtocolError> {
        self.find_meta(session_id)
            .filter(|meta| pull_request::shown(&meta.pull_requests, key))
            .map(|_| ())
            .ok_or_else(|| ProtocolError {
                code: "pull_request_not_linked".into(),
                message: "The pull request is not linked to this thread.".into(),
            })
    }
    pub(crate) fn read_pull_request(
        &mut self,
        session_id: &str,
        key: PullRequestKey,
        read: PullRequestRead,
        cx: &mut HostCx,
    ) -> HostTask<Result<QueryResponse, ProtocolError>> {
        if let Err(error) = self.linked_pull_request(session_id, &key) {
            return cx.spawn_background(async move { Err(error) });
        }
        let forge = self.pull_requests.forge.clone();
        let conversation = matches!(read, PullRequestRead::Conversation).then(|| {
            (
                session_id.to_owned(),
                key.clone(),
                self.pull_requests.submissions,
            )
        });
        let task = cx.unblock(move || forge.read(&key, read));
        let host = cx.clone();
        cx.spawn_background(async move {
            let answer = task.await;
            host.enqueue(|state, cx| state.refresh_host_credentials(cx));
            if let (Ok((PullRequestReadResponse::Conversation(read), _)), Some((id, key, since))) =
                (&answer, conversation)
            {
                let account = read.account.clone();
                let _ = host
                    .enqueue_and_wait(move |state, cx| {
                        state.read_after_submission(&id, &key, &account, since, cx)
                    })
                    .await;
            }
            answer
                .map(|(response, expires_at)| QueryResponse::PullRequest {
                    response: Box::new(response),
                    expires_at: expires_at
                        .duration_since(UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs(),
                })
                .map_err(|error| {
                    log::debug!("pull request read failed: {error}");
                    read_error(error)
                })
        })
    }
    /// A manual refresh: the host's answers about the pull request go, and the sync reads it.
    pub fn refresh_pull_request(
        &mut self,
        session_id: &str,
        key: PullRequestKey,
        cx: &mut HostCx,
    ) -> Result<(), ProtocolError> {
        self.linked_pull_request(session_id, &key)?;
        self.pull_requests.forge.invalidate(&key);
        self.request_pull_request_sync(key, cx);
        Ok(())
    }
    /// A conversation read that began after every submission landed shows what the host made of
    /// them, which is what an unanswered one waits for.
    fn read_after_submission(
        &mut self,
        session_id: &str,
        key: &PullRequestKey,
        account: &str,
        since: u64,
        cx: &mut HostCx,
    ) {
        if self.pull_requests.submissions != since {
            return;
        }
        if let Some(mut meta) = self.find_meta(session_id)
            && let Some(draft) = meta
                .pull_request_reviews
                .iter_mut()
                .find(|draft| draft.key == *key && draft.account == account && draft.uncertain)
        {
            draft.uncertain = false;
            self.save_pull_request_meta(meta, cx);
        }
    }
    pub fn set_pull_request_files_viewed(
        &mut self,
        session_id: &str,
        key: PullRequestKey,
        paths: Vec<String>,
        viewed: bool,
        cx: &mut HostCx,
    ) -> HostTask<Result<CommandResponse, ProtocolError>> {
        if let Err(error) = self.linked_pull_request(session_id, &key) {
            return cx.spawn_background(async move { Err(error) });
        }
        let forge = self.pull_requests.forge.clone();
        let task = cx.unblock(move || forge.set_viewed(&key, &paths, viewed));
        cx.spawn_background(async move {
            task.await
                .map(|()| CommandResponse::Unit)
                .map_err(read_error)
        })
    }
    /// A write to the pull request. Whenever the host may have applied it, the sync reads the pull
    /// request again. A review takes the account's draft and leaves it only once the host took it;
    /// a revert's new pull request is linked to the thread.
    pub fn run_pull_request_action(
        &mut self,
        session_id: &str,
        key: PullRequestKey,
        action: PullRequestAction,
        cx: &mut HostCx,
    ) -> HostTask<Result<CommandResponse, ProtocolError>> {
        if let Err(error) = self.linked_pull_request(session_id, &key) {
            return cx.spawn_background(async move { Err(error) });
        }
        if matches!(
            action,
            PullRequestAction::MergeStack { .. } | PullRequestAction::RebaseStack { .. }
        ) {
            return self.run_stack_action(session_id, key, action, cx);
        }
        // A native stack merges a layer with the layers below it only through its own route, so
        // nothing of one pull request's merge is sent for a layer, nor for a pull request whose
        // stack is not known yet.
        if matches!(
            action,
            PullRequestAction::Merge { .. } | PullRequestAction::UpdateBranch { .. }
        ) {
            let links = self
                .find_meta(session_id)
                .map(|meta| meta.pull_requests)
                .unwrap_or_default();
            let rejection = match pull_request::stack_route(&links, &key).0 {
                pull_request::PullRequestStackRoute::Single => None,
                pull_request::PullRequestStackRoute::Layer { .. } => {
                    Some(tcode_protocol::PullRequestRejection::Invalid)
                }
                pull_request::PullRequestStackRoute::Unknown => {
                    Some(tcode_protocol::PullRequestRejection::StackUnknown)
                }
            };
            if let Some(rejection) = rejection {
                return cx.spawn_background(async move {
                    Ok(CommandResponse::PullRequestAction(
                        PullRequestActionResult::Rejected(rejection),
                    ))
                });
            }
        }
        let drafts = self.review_drafts(session_id);
        let forge = self.pull_requests.forge.clone();
        let writing = key.clone();
        let task = cx.unblock(move || match &action {
            PullRequestAction::SubmitReview { verdict, head } => {
                let account = match forge.account(&writing) {
                    Ok(account) => account,
                    Err(error) => {
                        return (None, PullRequestActionResult::Rejected(error.rejection()));
                    }
                };
                let (anchor, body, comments) =
                    match pull_request::review_draft(&drafts, &writing, &account) {
                        Some(draft) => (
                            draft.head.as_str(),
                            draft.body.clone(),
                            draft.comments.as_slice(),
                        ),
                        None => ("", String::new(), &[][..]),
                    };
                // Comments are anchored at the draft's head, which is what the host must still be at.
                let head = if comments.is_empty() { head } else { anchor };
                let outcome = forge.submit_review(&writing, *verdict, head, &body, comments);
                let sent = match &outcome {
                    PullRequestActionResult::Applied => Some((
                        comments
                            .iter()
                            .map(|comment| comment.id)
                            .collect::<Vec<_>>(),
                        body,
                    )),
                    PullRequestActionResult::Partial { applied, .. } => {
                        let mut posted = applied.clone();
                        let ids = comments
                            .iter()
                            .filter_map(|comment| {
                                let name = format!("{}:{}", comment.path, comment.end_line);
                                let index = posted.iter().position(|posted| *posted == name)?;
                                posted.remove(index);
                                Some(comment.id)
                            })
                            .collect();
                        let body = if posted.iter().any(|name| name == "summary") {
                            body
                        } else {
                            String::new()
                        };
                        Some((ids, body))
                    }
                    _ => None,
                };
                (Some((account, sent)), outcome)
            }
            action => (None, forge.act(&writing, action)),
        });
        let host = cx.clone();
        let id = session_id.to_owned();
        cx.spawn_background(async move {
            let (review, outcome) = task.await;
            if !matches!(outcome, PullRequestActionResult::Rejected(_)) {
                let uncertain_review = matches!(outcome, PullRequestActionResult::Uncertain)
                    || matches!(&outcome, PullRequestActionResult::Partial { failure, .. }
                        if **failure == PullRequestActionResult::Uncertain);
                let opened = match &outcome {
                    PullRequestActionResult::Opened { number, url } => Some((
                        PullRequestKey::new(&key.host, &key.repository, *number),
                        url.clone(),
                    )),
                    _ => None,
                };
                let _ = host
                    .enqueue_and_wait(move |state, cx| {
                        if let Some((opened, url)) = opened
                            && let Err(error) = state.apply_pull_request_link(
                                &id,
                                opened,
                                url,
                                PullRequestSource::Opened,
                                true,
                                cx,
                            )
                        {
                            log::warn!("the revert pull request was not linked: {error}");
                        }
                        if let Some((account, sent)) = review {
                            state.pull_requests.submissions += 1;
                            if let Some(mut meta) = state.find_meta(&id) {
                                let changed = pull_request::submitted_review(
                                    &mut meta.pull_request_reviews,
                                    &key,
                                    &account,
                                    sent.as_ref()
                                        .map(|(ids, body)| (ids.as_slice(), body.as_str())),
                                );
                                if uncertain_review {
                                    pull_request::submitted_review(
                                        &mut meta.pull_request_reviews,
                                        &key,
                                        &account,
                                        None,
                                    );
                                }
                                if changed {
                                    state.save_pull_request_meta(meta, cx);
                                }
                            }
                        }
                        state.request_pull_request_sync(key, cx);
                    })
                    .await;
            }
            Ok(CommandResponse::PullRequestAction(outcome))
        })
    }
    fn review_drafts(&self, session_id: &str) -> Vec<pull_request::PullRequestReviewDraft> {
        self.find_meta(session_id)
            .map(|meta| meta.pull_request_reviews)
            .unwrap_or_default()
    }
    /// A new comment is taken only on lines the host would accept at the draft's head, and moving
    /// the draft reads where each comment's lines are now; both read the diff first.
    pub fn edit_pull_request_review_draft(
        &mut self,
        session_id: &str,
        key: PullRequestKey,
        edit: pull_request::PullRequestReviewDraftEdit,
        cx: &mut HostCx,
    ) -> HostTask<Result<CommandResponse, ProtocolError>> {
        use pull_request::PullRequestReviewDraftEdit as Edit;
        use tcode_services::forge::Anchoring;
        if let Err(error) = self.linked_pull_request(session_id, &key) {
            return cx.spawn_background(async move { Err(error) });
        }
        let forge = self.pull_requests.forge.clone();
        let drafts = self.review_drafts(session_id);
        let id = session_id.to_owned();
        let host = cx.clone();
        cx.spawn_background(async move {
            let anchor_error = |code: &str, message: &str| ProtocolError {
                code: code.into(),
                message: message.into(),
            };
            let head_changed = || ProtocolError {
                code: "pull_request_head_changed".into(),
                message: "The pull request's head changed.".into(),
            };
            let account = {
                let (forge, key) = (forge.clone(), key.clone());
                host.unblock(move || forge.account(&key))
                    .await
                    .map_err(read_error)?
            };
            let moved = match &edit {
                Edit::AddComment {
                    head,
                    path,
                    side,
                    start_line,
                    end_line,
                    ..
                } => {
                    let (reading, read_key, head, path, side) = (
                        forge.clone(),
                        key.clone(),
                        head.clone(),
                        path.clone(),
                        *side,
                    );
                    let lines = (*start_line, *end_line);
                    match host
                        .unblock(move || reading.commentable(&read_key, &head, &path, side, lines))
                        .await
                        .map_err(read_error)?
                    {
                        Anchoring::InDiff => None,
                        Anchoring::OutsideDiff => {
                            return Err(anchor_error(
                                "pull_request_not_in_diff",
                                &format!(
                                    "{} only accepts comments on lines in the diff.",
                                    forge.terms(&key).name
                                ),
                            ));
                        }
                        Anchoring::Moved => return Err(head_changed()),
                    }
                }
                Edit::MoveToHead => {
                    let (forge, key) = (forge.clone(), key.clone());
                    let comments = pull_request::review_draft(&drafts, &key, &account)
                        .map(|draft| draft.comments.clone())
                        .unwrap_or_default();
                    Some(
                        host.unblock(move || forge.reanchor(&key, &comments))
                            .await
                            .map_err(read_error)?,
                    )
                }
                _ => None,
            };
            host.enqueue_and_wait(move |state, cx| {
                let Some(mut meta) = state.find_meta(&id) else {
                    return Ok(());
                };
                // A comment read at another head than the draft's would be sent where its lines
                // may read differently; the draft moves to the new head first.
                if let Edit::AddComment { head, .. } = &edit
                    && pull_request::review_draft(&meta.pull_request_reviews, &key, &account)
                        .is_some_and(|draft| !draft.comments.is_empty() && draft.head != *head)
                {
                    return Err(head_changed());
                }
                let changed = match moved {
                    Some((head, moved)) => pull_request::reanchor_review(
                        &mut meta.pull_request_reviews,
                        &key,
                        &account,
                        &head,
                        |comment| {
                            moved
                                .iter()
                                .find(|(id, _)| *id == comment.id)
                                .and_then(|(_, revision)| revision.clone())
                        },
                    ),
                    None => pull_request::edit_review_draft(
                        &mut meta.pull_request_reviews,
                        &key,
                        &account,
                        edit,
                    ),
                };
                if changed {
                    state.save_pull_request_meta(meta, cx);
                }
                Ok(())
            })
            .await
            .map_err(|_| failure("Host closed."))??;
            Ok(CommandResponse::Unit)
        })
    }
    fn pull_request_project_cwd(&self, meta: &SessionMeta) -> PathBuf {
        self.projects
            .iter()
            .find(|project| Some(&project.id) == meta.project_id.as_ref())
            .map(|project| project.root.clone())
            .unwrap_or_else(|| meta.cwd.clone())
    }
    pub fn link_pull_request(
        &mut self,
        session_id: &str,
        reference: String,
        source: PullRequestSource,
        cx: &mut HostCx,
    ) -> HostTask<Result<CommandResponse, ProtocolError>> {
        let meta = self.find_meta(session_id);
        let cwd = meta
            .as_ref()
            .map(|meta| self.pull_request_project_cwd(meta));
        let id = session_id.to_owned();
        let forge = self.pull_requests.forge.clone();
        let hosts = pull_request::host_names(&self.pull_request_hosts());
        let host = cx.clone();
        cx.spawn_background(async move {
            let cwd = cwd.ok_or_else(|| failure("Unknown thread."))?;
            let target = host
                .unblock(move || resolve_reference(forge.as_ref(), &hosts, &cwd, &reference))
                .await?;
            let key = target.0.clone();
            let already_linked = host
                .enqueue_and_wait(move |state, cx| {
                    let already_linked = state.find_meta(&id).is_some_and(|meta| {
                        meta.pull_requests
                            .iter()
                            .any(|link| link.visible() && link.key == target.0)
                    });
                    state.apply_pull_request_link(&id, target.0, target.1, source, true, cx)?;
                    Ok::<_, String>(already_linked)
                })
                .await
                .map_err(|_| failure("Host closed."))?
                .map_err(failure)?;
            Ok(CommandResponse::PullRequestLinked {
                key,
                already_linked,
            })
        })
    }
    pub(super) fn save_pull_request_meta(&mut self, meta: SessionMeta, cx: &mut HostCx) {
        if let Some(resident) = self.meta_mut(&meta.id) {
            resident.pull_requests.clone_from(&meta.pull_requests);
            resident.pull_request_watches_paused = meta.pull_request_watches_paused;
            resident
                .pull_request_reviews
                .clone_from(&meta.pull_request_reviews);
            resident
                .pull_request_operations
                .clone_from(&meta.pull_request_operations);
        }
        self.persist_meta(&meta, cx);
    }
    fn apply_pull_request_link(
        &mut self,
        id: &str,
        key: PullRequestKey,
        url: String,
        source: PullRequestSource,
        explicit: bool,
        cx: &mut HostCx,
    ) -> Result<(), String> {
        let mut meta = self
            .find_meta(id)
            .ok_or_else(|| "Unknown thread.".to_owned())?;
        if meta.native_subagent.is_some() {
            return Err("This thread is read-only.".into());
        }
        if pull_request::link_pull_request(
            &mut meta.pull_requests,
            key.clone(),
            url,
            source,
            now_secs(),
            explicit,
        ) {
            self.save_pull_request_meta(meta, cx);
            self.request_pull_request_sync(key, cx);
        }
        Ok(())
    }
    pub fn unlink_pull_request(&mut self, id: &str, key: &PullRequestKey, cx: &mut HostCx) {
        let Some(mut meta) = self.find_meta(id) else {
            return;
        };
        if meta.native_subagent.is_some() {
            return;
        }
        if pull_request::unlink_pull_request(&mut meta.pull_requests, key) {
            self.discard_pull_request_wakes(id, Some(key));
            self.save_pull_request_meta(meta, cx);
        }
    }
    /// Requests a read of the thread's open links, as after an agent merged or closed one.
    fn refresh_open_pull_requests(&mut self, id: &str, cx: &mut HostCx) {
        if let Some(meta) = self.find_meta(id) {
            for link in meta.pull_requests.into_iter().filter(|link| {
                link.visible()
                    && link
                        .snapshot
                        .as_ref()
                        .is_some_and(|snapshot| snapshot.state == PullRequestState::Open)
            }) {
                self.request_pull_request_sync(link.key, cx);
            }
        }
    }
    pub(super) fn request_pull_request_sync(&mut self, key: PullRequestKey, cx: &mut HostCx) {
        self.pull_requests.generation += 1;
        self.pull_requests
            .requested
            .insert(key, self.pull_requests.generation);
        if self.pull_requests.syncing || self.pull_requests.sync_scheduled {
            return;
        }
        self.pull_requests.sync_scheduled = true;
        let host = cx.clone();
        cx.spawn_detached(async move {
            smol::Timer::after(Duration::from_millis(10)).await;
            host.enqueue(move |state, cx| {
                state.pull_requests.sync_scheduled = false;
                state.sweep_pull_requests(true, cx).detach();
            });
        });
    }
    pub(super) fn stop_pull_request_workers(&mut self) {
        self.pull_requests.workers.clear();
        self.stop_pull_request_watch_worker();
    }
    pub(crate) fn start_pull_request_workers(&mut self, cx: &mut HostCx) {
        let host = cx.clone();
        let task = cx.spawn_background(async move {
            loop {
                let task = host
                    .enqueue_and_wait(|state, cx| state.sweep_pull_requests(false, cx))
                    .await;
                let Ok(task) = task else { break };
                task.await;
                smol::Timer::after(Duration::from_secs(60)).await;
            }
        });
        self.pull_requests.workers.push(task);
        self.resume_stack_operations(cx);
        self.start_pull_request_watch_worker(cx);
    }
    pub(super) fn sweep_pull_requests(
        &mut self,
        requested_only: bool,
        cx: &mut HostCx,
    ) -> HostTask<()> {
        if self.pull_requests.syncing {
            return cx.spawn_background(async {});
        }
        self.pull_requests.syncing = true;
        self.reconcile_unconfirmed_merges(None, cx);
        if !requested_only {
            self.drop_ended_rebases(cx);
        }
        let sweep_generation = self.pull_requests.generation;
        let now = now_secs();
        let mut groups: HashMap<PullRequestKey, SyncGroup> = HashMap::new();
        let metas: Vec<_> = self
            .sessions
            .iter()
            .filter_map(|meta| self.find_meta(&meta.id))
            .filter(|meta| meta.archived_at.is_none())
            .collect();
        for meta in metas {
            let links = meta
                .pull_requests
                .iter()
                .filter(|link| link.visible())
                .map(|link| (&link.key, &link.snapshot, link.stack.clone()));
            for (key, snapshot, stack) in links {
                let group = groups.entry(key.clone()).or_insert_with(|| SyncGroup {
                    key: key.clone(),
                    projects: Vec::new(),
                    threads: Vec::new(),
                    observations: Vec::new(),
                    request: self.pull_requests.requested.get(key).copied(),
                });
                group.threads.push(meta.id.clone());
                if !group.projects.contains(&meta.project_id) {
                    group.projects.push(meta.project_id.clone());
                }
                // Settled threads share updates without independently scheduling reads, except
                // for a link never read, which nothing else would read after a restart.
                if !meta.is_settled() || snapshot.is_none() {
                    group.observations.push((snapshot.clone(), stack));
                }
            }
        }
        self.pull_requests
            .last_synced
            .retain(|key, _| groups.contains_key(key));
        self.pull_requests
            .requested
            .retain(|key, _| groups.contains_key(key));
        let paused_until = |project: &Option<String>, host: &str| {
            self.pull_requests
                .paused
                .get(&(project.clone(), host.to_owned()))
                .is_some_and(|until| *until > SystemTime::now())
        };
        let due: Vec<_> = groups
            .into_values()
            .filter(|group| {
                if requested_only && group.request.is_none() {
                    return false;
                }
                if group
                    .projects
                    .iter()
                    .all(|project| paused_until(project, &group.key.host))
                {
                    return false;
                }
                let state = |wanted| {
                    group.observations.iter().any(|(snapshot, _)| {
                        snapshot
                            .as_ref()
                            .is_some_and(|snapshot| snapshot.state == wanted)
                    })
                };
                group.request.is_some()
                    || group
                        .observations
                        .iter()
                        .any(|(snapshot, _)| snapshot.is_none())
                    || state(PullRequestState::Open)
                    || (state(PullRequestState::Closed)
                        && self
                            .pull_requests
                            .last_synced
                            .get(&group.key)
                            .is_none_or(|last| now.saturating_sub(*last) >= 900))
            })
            .collect();
        let forge = self.pull_requests.forge.clone();
        let host = cx.clone();
        cx.spawn_background(async move {
            let mut failures = HashMap::<String, usize>::new();
            for chunk in due.chunks(25) {
                let tasks: Vec<_> = chunk
                    .iter()
                    .map(|group| {
                        let key = group.key.clone();
                        let observations = group.observations.clone();
                        let forced = group.request.is_some();
                        let forge = forge.clone();
                        let read = host.unblock(move || {
                            let summary = forge.summary(&key)?;
                            let changed = observations.iter().any(|(snapshot, stack)| {
                                let known = match stack {
                                    PullRequestStackState::Native(stack) => Some(stack.number),
                                    _ => None,
                                };
                                snapshot
                                    .as_ref()
                                    .is_none_or(|s| !s.same_observation(&summary.snapshot))
                                    || summary.stack_number.is_some_and(|number| number != known)
                            });
                            let stack = if summary.stack_number == Some(None) {
                                Some(PullRequestStackState::None)
                            } else if forced || changed {
                                Some(forge.stack(&key)?)
                            } else {
                                None
                            };
                            Ok::<_, ForgeError>((summary, stack))
                        });
                        (group, read)
                    })
                    .collect();
                for (group, read) in tasks {
                    let result = read.await;
                    if let Err(error) = &result {
                        *failures.entry(error.to_string()).or_default() += 1;
                    }
                    let key = group.key.clone();
                    let projects = group.projects.clone();
                    let threads = group.threads.clone();
                    let generation = group.request;
                    let _ = host
                        .enqueue_and_wait(move |state, cx| {
                            state.finish_pull_request_sync(
                                key, projects, threads, generation, result, cx,
                            )
                        })
                        .await;
                }
            }
            for (reason, count) in failures {
                log::warn!("pull request sweep skipped {count} reads: {reason}");
            }
            let _ = host
                .enqueue_and_wait(move |state, cx| {
                    state.pull_requests.syncing = false;
                    state.refresh_host_credentials(cx);
                    if state
                        .pull_requests
                        .requested
                        .values()
                        .any(|generation| *generation > sweep_generation)
                    {
                        state.sweep_pull_requests(true, cx).detach();
                    }
                })
                .await;
        })
    }
    fn finish_pull_request_sync(
        &mut self,
        key: PullRequestKey,
        projects: Vec<Option<String>>,
        threads: Vec<String>,
        generation: Option<u64>,
        result: Result<(Summary, Option<PullRequestStackState>), ForgeError>,
        cx: &mut HostCx,
    ) {
        if let Err(error) = &result {
            let reason = match &error.kind {
                ForgeErrorKind::HostDisabled => PullRequestSyncError::HostDisabled,
                ForgeErrorKind::NoCredential | ForgeErrorKind::Unauthorized => {
                    PullRequestSyncError::NoCredential
                }
                ForgeErrorKind::RateLimited { retry_at } | ForgeErrorKind::Paused { retry_at } => {
                    PullRequestSyncError::RateLimited {
                        retry_at: retry_at
                            .duration_since(UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_secs(),
                    }
                }
                ForgeErrorKind::NotFound => PullRequestSyncError::NotFound,
                _ => PullRequestSyncError::Failed,
            };
            for id in &threads {
                if let Some(mut meta) = self.find_meta(id)
                    && let Some(link) = meta
                        .pull_requests
                        .iter_mut()
                        .find(|link| link.key == key && link.visible() && link.snapshot.is_none())
                    && link.sync_error.as_ref() != Some(&reason)
                {
                    link.sync_error = Some(reason.clone());
                    self.save_pull_request_meta(meta, cx);
                }
            }
        }
        let (summary, stack) = match result {
            Ok(result) => result,
            Err(error) if let Some(retry_at) = error.retry_at() => {
                for project in projects {
                    let until = self
                        .pull_requests
                        .paused
                        .entry((project, key.host.clone()))
                        .or_insert(retry_at);
                    *until = (*until).max(retry_at);
                }
                return;
            }
            Err(error) => {
                log::debug!("pull request summary skipped: {error}");
                return;
            }
        };
        self.pull_requests
            .last_synced
            .insert(key.clone(), now_secs());
        self.reconcile_unconfirmed_merges(Some((&key, summary.snapshot.state)), cx);
        if self.pull_requests.requested.get(&key).copied() == generation {
            self.pull_requests.requested.remove(&key);
        }
        for id in threads {
            let Some(mut meta) = self
                .find_meta(&id)
                .filter(|meta| meta.archived_at.is_none())
            else {
                continue;
            };
            let Some(link) = meta
                .pull_requests
                .iter()
                .find(|link| link.key == key && link.visible())
            else {
                continue;
            };
            let stack = stack.clone().unwrap_or_else(|| link.stack.clone());
            let observation_changed = link
                .snapshot
                .as_ref()
                .is_none_or(|s| !s.same_observation(&summary.snapshot))
                || link.stack != stack;
            let mut changed = observation_changed;
            if observation_changed {
                self.pull_requests.forge.invalidate(&key);
            }
            if !meta.is_settled()
                && let PullRequestStackState::Native(topology) = &stack
            {
                for layer in &topology.layers {
                    let sibling = PullRequestKey::new(&key.host, &key.repository, layer.number);
                    let Some(url) = self.pull_requests.forge.url(&sibling) else {
                        continue;
                    };
                    if pull_request::link_pull_request(
                        &mut meta.pull_requests,
                        sibling.clone(),
                        url,
                        PullRequestSource::Stack,
                        now_secs(),
                        false,
                    ) {
                        changed = true;
                        if let Some(link) = meta
                            .pull_requests
                            .iter_mut()
                            .find(|link| link.key == sibling)
                        {
                            link.stack = stack.clone();
                        }
                        self.pull_requests.generation += 1;
                        self.pull_requests
                            .requested
                            .insert(sibling, self.pull_requests.generation);
                    }
                }
            }
            if !changed {
                continue;
            }
            if observation_changed
                && let Some(link) = meta
                    .pull_requests
                    .iter_mut()
                    .find(|link| link.key == key && link.visible())
            {
                link.sync_error = None;
                link.snapshot = Some(summary.snapshot.clone());
                link.stack = stack;
            }
            self.save_pull_request_meta(meta, cx);
            self.evaluate_thread_settlement(&id, cx);
        }
    }
    pub(super) fn observe_pull_request_event(
        &mut self,
        id: &str,
        event: &AgentEvent,
        cx: &mut HostCx,
    ) {
        if let AgentEvent::ItemStarted(item) | AgentEvent::ItemCompleted(item) = event
            && let ItemContent::CommandExecution { command, .. } = &item.content
            && pull_request::merges_or_closes(command)
        {
            self.pull_requests.merge_commands.insert(id.to_owned());
        }
        if matches!(
            event,
            AgentEvent::TurnCompleted { .. } | AgentEvent::TurnCheckpoint { .. }
        ) && self.pull_requests.merge_commands.remove(id)
        {
            self.refresh_open_pull_requests(id, cx);
        }
    }
}

#[cfg(test)]
#[path = "pull_requests_tests.rs"]
mod tests;
