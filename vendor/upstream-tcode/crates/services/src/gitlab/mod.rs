//! GitLab behind the pull request host boundary: gitlab.com and self-managed servers, whose
//! merge requests Tcode calls pull requests. Call blocking entries via HostCx::unblock.

mod actions;
mod api;
mod reads;
mod repository;

use crate::{
    forge::{
        Anchoring, Forge, ForgeError, ForgeErrorKind, Moved, Repository, Summary, Tails,
        ViewedMarks,
    },
    settings::SettingsStore,
};
use api::{Api, Request, error};
use reads::{MAX_PAGES, Mr, text};
use serde_json::Value;
use std::{
    collections::{BTreeMap, HashMap},
    sync::{Arc, Mutex, RwLock},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tcode_core::{
    pull_request::{
        GITLAB, HostKind, HostTerms, PullRequestKey, PullRequestMergeMethod,
        PullRequestReviewDraftComment, PullRequestStackState,
    },
    pull_request_watch::{CheckStatus, PullRequestRemark, PullRequestWatchRead},
    session::ReviewSide,
    settings::{CredentialSource, HostProblem, HostSettings, HostStatus},
};
use tcode_protocol::{
    PullRequestAction, PullRequestActionResult as Outcome, PullRequestActionState,
    PullRequestCapabilities, PullRequestConversation, PullRequestFileText, PullRequestFiles,
    PullRequestLabelCandidate, PullRequestLabelCandidates, PullRequestMedia, PullRequestRead,
    PullRequestReadResponse, PullRequestReviewVerdict, PullRequestReviewerCandidate,
    PullRequestReviewerCandidates, PullRequestThreadReplies, PullRequestViewedFiles,
};

const READ_TTL: Duration = Duration::from_secs(60);
/// Text at a commit never changes; the bound is only on how long it holds memory.
const TEXT_TTL: Duration = Duration::from_secs(600);
const FILE_BYTES: usize = 1024 * 1024;
const ANONYMOUS: &str = "anonymous";
/// The revision Tcode's viewed marks hold for a file the merge request deletes, as a diff's
/// index line names a missing side.
const DELETED: &str = "0000000000000000000000000000000000000000";

type Slot = (Instant, PullRequestReadResponse);

pub struct GitLab {
    api: Arc<Api>,
    viewed: Arc<ViewedMarks>,
    /// The servers known by settings, a glab login or the environment.
    known: RwLock<Vec<String>>,
    reads: Mutex<HashMap<(PullRequestKey, String), Slot>>,
}

impl GitLab {
    pub(crate) fn new(
        store: SettingsStore,
        environment: impl IntoIterator<Item = (String, String)>,
        viewed: Arc<ViewedMarks>,
    ) -> Arc<Self> {
        Arc::new(Self {
            viewed,
            api: Api::new(store, environment),
            known: RwLock::default(),
            reads: Mutex::default(),
        })
    }

    fn known(&self) -> Vec<String> {
        self.known.read().unwrap().clone()
    }

    fn mr<'a>(&'a self, key: &'a PullRequestKey) -> Mr<'a> {
        Mr {
            api: &self.api,
            key,
        }
    }

    fn now() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
    }

    /// A read kept for `ttl`; a write drops every read of its merge request.
    fn cached(
        &self,
        key: &PullRequestKey,
        name: String,
        ttl: Duration,
        read: impl FnOnce() -> Result<PullRequestReadResponse, ForgeError>,
    ) -> Result<(PullRequestReadResponse, SystemTime), ForgeError> {
        let slot = (key.clone(), name);
        if let Some((at, response)) = self.reads.lock().unwrap().get(&slot)
            && at.elapsed() < ttl
        {
            return Ok((response.clone(), SystemTime::now() + (ttl - at.elapsed())));
        }
        let response = read()?;
        let mut reads = self.reads.lock().unwrap();
        reads.retain(|_, (at, _)| at.elapsed() < TEXT_TTL);
        reads.insert(slot, (Instant::now(), response.clone()));
        Ok((response, SystemTime::now() + ttl))
    }

    fn account_of(&self, viewer: Option<&str>, key: &PullRequestKey) -> String {
        format!("{}:{}", key.host, viewer.unwrap_or(ANONYMOUS))
    }

    /// The diffs from `page` on: one page past the first, else as many as [`MAX_PAGES`]
    /// read, with where the next starts.
    fn files(
        &self,
        key: &PullRequestKey,
        page: Option<u32>,
    ) -> Result<PullRequestFiles, ForgeError> {
        let mr = self.mr(key);
        let read = mr.mr()?;
        let refs = &read["diff_refs"];
        let changed_files = read["changes_count"]
            .as_str()
            .unwrap_or_default()
            .trim_end_matches('+')
            .parse()
            .unwrap_or(0);
        let mut next = Some(page.unwrap_or(1));
        let mut files = Vec::new();
        let pages = if page.is_some() { 1 } else { MAX_PAGES };
        for _ in 0..pages {
            let Some(at) = next else {
                break;
            };
            let response = self.api.send(
                key.host.as_str(),
                Request::get(
                    mr.path(&format!("/diffs?per_page={}&page={at}", api::PAGE_LIMIT)),
                    "Diffs",
                ),
            )?;
            let rows: Vec<Value> = response.json()?;
            files.extend(rows.iter().filter_map(reads::file));
            next = response.next_page.filter(|next| *next > at);
        }
        Ok(PullRequestFiles {
            base: text(refs, "base_sha").unwrap_or_default(),
            head: text(refs, "head_sha")
                .or_else(|| text(&read, "sha"))
                .unwrap_or_default(),
            files,
            next_cursor: next.map(|page| page.to_string()),
            complete: next.is_none(),
            changed_files,
        })
    }

    fn file_text(
        &self,
        key: &PullRequestKey,
        revision: &str,
        path: &str,
    ) -> Result<PullRequestFileText, ForgeError> {
        if !revision.bytes().all(|b| b.is_ascii_hexdigit())
            || revision.is_empty()
            || path.split('/').any(|part| part.is_empty() || part == "..")
        {
            return Err(error(
                ForgeErrorKind::InvalidInput,
                "invalid revision or path",
            ));
        }
        let mr = self.mr(key);
        let answer = self.api.send(
            mr.authority(),
            Request {
                accept: "application/octet-stream",
                limit: FILE_BYTES,
                ..Request::get(
                    mr.project_path(&format!(
                        "/repository/files/{}/raw?ref={revision}",
                        crate::github::pull_request_reads::percent_encode(path)
                    )),
                    "FileText",
                )
            },
        );
        Ok(match answer {
            Ok(response) if response.truncated => PullRequestFileText::Oversized,
            Ok(response) if response.body.contains(&0) => PullRequestFileText::Binary,
            Ok(response) => String::from_utf8(response.body)
                .map_or(PullRequestFileText::Binary, PullRequestFileText::Text),
            Err(ForgeError {
                kind: ForgeErrorKind::NotFound,
                ..
            }) => PullRequestFileText::Missing,
            Err(error) => return Err(error),
        })
    }

    fn conversation(&self, key: &PullRequestKey) -> Result<PullRequestConversation, ForgeError> {
        let (mr, viewer, discussions, complete) = self.mr(key).discussions()?;
        let conversation = reads::conversation(
            &key.host,
            &key.repository,
            &mr,
            viewer.as_deref(),
            &discussions,
        )
        .ok_or_else(|| error(ForgeErrorKind::Uncertain, "GitLab conversation unreadable"))?;
        Ok(PullRequestConversation {
            description: conversation.description,
            comments: conversation.comments,
            threads: conversation.threads,
            complete,
            account: self.account_of(viewer.as_deref(), key),
            permissions: conversation.permissions,
            labels: conversation.labels,
            reviewers: conversation.reviewers,
            capabilities: self.capabilities(key),
        })
    }

    /// Each changed path's blob at the head, a deleted one's as a missing side; `None` when
    /// GitLab could not say.
    fn revisions(
        &self,
        key: &PullRequestKey,
        files: &PullRequestFiles,
    ) -> Result<Option<BTreeMap<String, String>>, ForgeError> {
        let (deleted, present): (Vec<_>, Vec<_>) = files
            .files
            .iter()
            .partition(|file| file.kind == agent::FileChangeKind::Delete);
        let paths: Vec<_> = present.iter().map(|file| file.path.clone()).collect();
        let Some(mut revisions) = self.mr(key).blobs(&files.head, &paths)? else {
            return Ok(None);
        };
        revisions.extend(
            deleted
                .into_iter()
                .map(|file| (file.path.clone(), DELETED.to_owned())),
        );
        Ok(Some(revisions))
    }

    fn viewed_files(&self, key: &PullRequestKey) -> Result<PullRequestViewedFiles, ForgeError> {
        let viewer = self.mr(key).viewer()?;
        let files = self.files(key, None)?;
        let Some(revisions) = self.revisions(key, &files)? else {
            return Ok(PullRequestViewedFiles {
                files: Vec::new(),
                complete: false,
            });
        };
        Ok(PullRequestViewedFiles {
            files: self
                .viewed
                .states(&self.account_of(viewer.as_deref(), key), key, &revisions),
            complete: files.complete && revisions.len() == files.files.len(),
        })
    }

    fn label_candidates(
        &self,
        key: &PullRequestKey,
    ) -> Result<PullRequestLabelCandidates, ForgeError> {
        let mr = self.mr(key);
        let read = mr.mr()?;
        let (rows, complete) = mr.labels()?;
        let applied: Vec<String> = read["labels"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|label| label.as_str().map(str::to_owned))
            .collect();
        let row_of = |name: &str| rows.iter().find(|row| row["title"].as_str() == Some(name));
        let candidate =
            |name: &str, row: Option<&Value>, applied: bool| PullRequestLabelCandidate {
                id: name.to_owned(),
                name: name.to_owned(),
                color: row
                    .and_then(|row| text(row, "color"))
                    .map(|color| color.trim_start_matches('#').to_owned()),
                description: row
                    .and_then(|row| text(row, "description"))
                    .filter(|text| !text.is_empty()),
                applied,
            };
        let mut labels: Vec<_> = applied
            .iter()
            .map(|name| candidate(name, row_of(name), true))
            .collect();
        for row in &rows {
            let Some(name) = row["title"].as_str() else {
                continue;
            };
            if !applied.iter().any(|known| known == name) {
                labels.push(candidate(name, Some(row), false));
            }
        }
        Ok(PullRequestLabelCandidates { labels, complete })
    }

    fn reviewer_candidates(
        &self,
        key: &PullRequestKey,
    ) -> Result<PullRequestReviewerCandidates, ForgeError> {
        let mr = self.mr(key);
        let read = mr.mr()?;
        let author = read["author"]["id"].as_u64();
        let (rows, complete) = self.api.list(
            mr.authority(),
            &mr.project_path("/users"),
            "ProjectUsers",
            1,
        )?;
        let candidate = |user: &Value, requested: bool| {
            Some(PullRequestReviewerCandidate {
                reviewer: reads::user_reviewer(
                    user["id"].as_u64()?.to_string(),
                    text(user, "username")?,
                ),
                name: text(user, "name").filter(|name| !name.is_empty()),
                avatar_url: text(user, "avatar_url"),
                requested,
            })
        };
        let mut reviewers: Vec<_> = read["reviewers"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|user| candidate(user, true))
            .collect();
        for user in &rows {
            if user["id"].as_u64() == author
                || reviewers.iter().any(|known| {
                    user["id"]
                        .as_u64()
                        .is_some_and(|id| known.reviewer.id == id.to_string())
                })
            {
                continue;
            }
            reviewers.extend(candidate(user, false));
        }
        Ok(PullRequestReviewerCandidates {
            reviewers,
            complete,
        })
    }

    fn action_state(&self, key: &PullRequestKey) -> Result<PullRequestActionState, ForgeError> {
        let mr = self.mr(key);
        let read = mr.mr()?;
        let project = mr.get(mr.project_path(""), "Project")?;
        let (can_update, can_push) = mr.may_update()?;
        let checks = reads::checks(&read);
        let failing_checks: Vec<_> = checks
            .iter()
            .filter(|check| check.status.failed())
            .map(|check| check.name.clone())
            .collect();
        let pending_checks = checks
            .iter()
            .filter(|check| check.status == CheckStatus::Pending)
            .count() as u32;
        let merge_methods = reads::merge_methods(&project);
        // GitLab keeps the field set on a merge request it has merged.
        let armed = reads::state(&read) == Some(tcode_core::pull_request::PullRequestState::Open)
            && read["merge_when_pipeline_succeeds"].as_bool() == Some(true);
        // GitLab keeps the squash choice beside a scheduled merge, and the project's one method.
        let armed_with = if read["squash_on_merge"].as_bool() == Some(true)
            || read["squash"].as_bool() == Some(true)
        {
            PullRequestMergeMethod::Squash
        } else {
            merge_methods
                .iter()
                .copied()
                .find(|method| *method != PullRequestMergeMethod::Squash)
                .unwrap_or(PullRequestMergeMethod::Merge)
        };
        Ok(PullRequestActionState {
            head: text(&read, "sha").unwrap_or_default(),
            merge_state: reads::merge_state(&read, reads::mergeability(&read), &checks),
            behind_by: read["diverged_commits_count"].as_u64(),
            merge_queue: false,
            auto_merge_allowed: true,
            auto_merge: armed.then_some(armed_with),
            merge_methods,
            queued: false,
            queue_position: None,
            failing_checks,
            pending_checks,
            can_update,
            can_update_branch: can_push,
            can_merge: read["user"]["can_merge"].as_bool() == Some(true),
            capabilities: self.capabilities(key),
        })
    }

    fn media(
        &self,
        key: &PullRequestKey,
        url: &str,
        validator: Option<&str>,
    ) -> Result<PullRequestMedia, ForgeError> {
        let Ok(parsed) = url::Url::parse(url) else {
            return Ok(PullRequestMedia::Unsupported);
        };
        let host_port = match (parsed.host_str(), parsed.port()) {
            (Some(host), Some(port)) => format!("{host}:{port}"),
            (Some(host), None) => host.to_owned(),
            _ => return Ok(PullRequestMedia::Unsupported),
        };
        let path = parsed.path();
        let project = format!("/{}/uploads/", key.repository);
        // Uploads and avatars on the merge request's own server, which a private project serves
        // only with its token; anything else the client draws by its URL.
        let own = parsed.scheme() == "https"
            && host_port.eq_ignore_ascii_case(&key.host)
            && (path.starts_with("/uploads/") || path.to_ascii_lowercase().starts_with(&project));
        if !own {
            return Ok(PullRequestMedia::Unsupported);
        }
        // Bodies name their uploads in full (see reads::absolute_uploads).
        let conversation = self.conversation_read(key)?;
        let named = std::iter::once(&conversation.description)
            .chain(&conversation.comments)
            .chain(
                conversation
                    .threads
                    .iter()
                    .flat_map(|thread| &thread.comments),
            )
            .any(|comment| {
                comment.body.contains(url)
                    || comment
                        .author
                        .as_ref()
                        .and_then(|author| author.avatar_url.as_deref())
                        == Some(url)
            })
            || conversation
                .reviewers
                .iter()
                .any(|reviewer| reviewer.avatar_url.as_deref() == Some(url));
        if !named {
            return Err(error(
                ForgeErrorKind::InvalidInput,
                "media not in the conversation",
            ));
        }
        self.api.media(&key.host, &parsed, validator)
    }

    fn conversation_read(
        &self,
        key: &PullRequestKey,
    ) -> Result<PullRequestConversation, ForgeError> {
        match self.read(key, PullRequestRead::Conversation)?.0 {
            PullRequestReadResponse::Conversation(conversation) => Ok(*conversation),
            _ => Err(error(ForgeErrorKind::Uncertain, "unexpected read")),
        }
    }

    fn drop_reads(&self, key: &PullRequestKey) {
        self.reads
            .lock()
            .unwrap()
            .retain(|(held, name), _| held != key || name.starts_with("text:"));
    }
}

impl Forge for GitLab {
    fn terms(&self, _: &PullRequestKey) -> &'static HostTerms {
        &GITLAB
    }

    fn capabilities(&self, _: &PullRequestKey) -> PullRequestCapabilities {
        reads::capabilities()
    }

    fn configure(&self, hosts: BTreeMap<String, HostSettings>) {
        let own: BTreeMap<_, _> = hosts
            .into_iter()
            .filter(|(_, choice)| choice.kind == HostKind::Gitlab)
            .collect();
        {
            let mut known = self.known.write().unwrap();
            for host in own.keys() {
                if !known.contains(host) {
                    known.push(host.clone());
                }
            }
        }
        self.api.configure(own);
    }

    fn credential_status(&self) -> BTreeMap<String, HostStatus> {
        let configured = self.api.configured();
        let glab = self.api.glab_program().is_some();
        // Each host, and whether only settings name it.
        let mut hosts: BTreeMap<String, bool> =
            configured.keys().map(|host| (host.clone(), true)).collect();
        let detected = self
            .api
            .glab_logins()
            .into_iter()
            .chain(self.api.environment_host());
        for host in detected {
            hosts.insert(host, false);
        }
        {
            let mut known = self.known.write().unwrap();
            for host in hosts.keys() {
                if !known.contains(host) {
                    known.push(host.clone());
                }
            }
        }
        hosts
            .into_iter()
            .map(|(host, added)| {
                let enabled = configured.get(&host).is_none_or(|choice| choice.enabled);
                let credential = self.api.credential(&host).ok().flatten();
                let source = credential
                    .as_ref()
                    .map(|credential| credential.source.clone());
                let problem = match &credential {
                    None if !enabled => None,
                    None if glab => Some(HostProblem::NotSignedIn {
                        tool: "glab".into(),
                        command: Some(format!("glab auth login --hostname {host}")),
                    }),
                    None => Some(HostProblem::NoCredential {
                        tools_missing: vec!["glab".into()],
                    }),
                    Some(credential)
                        if enabled
                            && credential.source == CredentialSource::Saved
                            && !self.api.verified(&host, credential) =>
                    {
                        Some(HostProblem::Unverified)
                    }
                    Some(_) => None,
                };
                (
                    host.clone(),
                    HostStatus {
                        kind: HostKind::Gitlab,
                        added,
                        token_set: self.api.token_saved(&host),
                        source,
                        accounts: Vec::new(),
                        env_overrides_account: false,
                        order: vec![
                            CredentialSource::Saved,
                            CredentialSource::Env {
                                name: api::ENV_TOKEN.into(),
                            },
                            CredentialSource::Cli {
                                tool: "glab".into(),
                            },
                        ],
                        problem,
                        token_url: Some(format!(
                            "https://{host}/-/user_settings/personal_access_tokens?name=Tcode&scopes=api"
                        )),
                        access_token_url: None,
                        token_scopes: vec!["api".into()],
                    },
                )
            })
            .collect()
    }

    fn forget_credential(&self, host: &str) {
        self.api.forget(host);
    }

    fn pull_request_url(&self, url: &str) -> Option<(PullRequestKey, String)> {
        repository::pull_request_url(&self.known(), url)
    }

    fn checkout_repository(&self, checkout: &crate::forge::Checkout<'_>) -> Option<Repository> {
        repository::resolve(&self.known(), checkout)
    }

    fn repository(&self, name: &str, host: &str) -> Option<Repository> {
        repository::selector(&self.known(), name, host)
    }

    fn url(&self, key: &PullRequestKey) -> Option<String> {
        Some(repository::url(key))
    }

    fn summary(&self, key: &PullRequestKey) -> Result<Summary, ForgeError> {
        let mr = self.mr(key);
        let read = mr.mr()?;
        // The counts only decorate the summary; without them it shows no stat.
        let stats = mr.diff_stats().unwrap_or_else(|failure| {
            log::debug!("gitlab host={} diff stats unread: {failure}", key.host);
            None
        });
        Ok(Summary {
            snapshot: reads::snapshot(&read, stats, reads::mergeability(&read), Self::now())
                .ok_or_else(|| {
                    error(ForgeErrorKind::Uncertain, "GitLab merge request unreadable")
                })?,
            stack_number: None,
        })
    }

    fn stack(&self, _: &PullRequestKey) -> Result<PullRequestStackState, ForgeError> {
        Ok(PullRequestStackState::None)
    }

    fn read(
        &self,
        key: &PullRequestKey,
        read: PullRequestRead,
    ) -> Result<(PullRequestReadResponse, SystemTime), ForgeError> {
        match read {
            PullRequestRead::Files { cursor } => {
                let page = cursor
                    .map(|cursor| {
                        cursor
                            .parse::<u32>()
                            .ok()
                            .filter(|page| *page > 1)
                            .ok_or_else(|| error(ForgeErrorKind::InvalidInput, "invalid cursor"))
                    })
                    .transpose()?;
                self.cached(key, format!("files:{page:?}"), READ_TTL, || {
                    self.files(key, page).map(PullRequestReadResponse::Files)
                })
            }
            PullRequestRead::FileText { revision, path } => {
                self.cached(key, format!("text:{revision}:{path}"), TEXT_TTL, || {
                    self.file_text(key, &revision, &path)
                        .map(PullRequestReadResponse::FileText)
                })
            }
            PullRequestRead::Conversation => {
                self.cached(key, "conversation".into(), READ_TTL, || {
                    self.conversation(key).map(|conversation| {
                        PullRequestReadResponse::Conversation(Box::new(conversation))
                    })
                })
            }
            // A thread here carries all of its comments.
            PullRequestRead::ThreadReplies { .. } => Ok((
                PullRequestReadResponse::ThreadReplies(PullRequestThreadReplies {
                    comments: Vec::new(),
                    after: None,
                }),
                SystemTime::now(),
            )),
            PullRequestRead::ViewedFiles => Ok((
                PullRequestReadResponse::ViewedFiles(self.viewed_files(key)?),
                SystemTime::now(),
            )),
            PullRequestRead::LabelCandidates => self.cached(key, "labels".into(), READ_TTL, || {
                self.label_candidates(key)
                    .map(PullRequestReadResponse::LabelCandidates)
            }),
            PullRequestRead::ReviewerCandidates => {
                self.cached(key, "reviewers".into(), READ_TTL, || {
                    self.reviewer_candidates(key)
                        .map(PullRequestReadResponse::ReviewerCandidates)
                })
            }
            // Read fresh: a merge or a rebase decides on it.
            PullRequestRead::ActionState => Ok((
                PullRequestReadResponse::ActionState(self.action_state(key)?),
                SystemTime::now(),
            )),
            PullRequestRead::StackState { .. } => Err(error(
                ForgeErrorKind::NotFound,
                "no native stacks on this host",
            )),
            PullRequestRead::Media { url, validator } => {
                let media = self.media(key, &url, validator.as_deref())?;
                let expires_at = match &media {
                    PullRequestMedia::Image { expires_at, .. }
                    | PullRequestMedia::NotModified { expires_at } => {
                        UNIX_EPOCH + Duration::from_secs(*expires_at)
                    }
                    _ => SystemTime::now(),
                };
                Ok((PullRequestReadResponse::Media(media), expires_at))
            }
        }
    }

    fn invalidate(&self, key: &PullRequestKey) {
        self.drop_reads(key);
    }

    fn account(&self, key: &PullRequestKey) -> Result<String, ForgeError> {
        Ok(self.account_of(self.mr(key).viewer()?.as_deref(), key))
    }

    fn set_viewed(
        &self,
        key: &PullRequestKey,
        paths: &[String],
        viewed: bool,
    ) -> Result<(), ForgeError> {
        let account = self.account(key)?;
        let files = self.files(key, None)?;
        let revisions = self
            .revisions(key, &files)?
            .ok_or_else(|| error(ForgeErrorKind::Uncertain, "GitLab file revisions unread"))?;
        self.viewed
            .set(&account, key, &revisions, paths, viewed)
            .map_err(|_| error(ForgeErrorKind::Uncertain, "viewed marks not saved"))
    }

    fn act(&self, key: &PullRequestKey, action: &PullRequestAction) -> Outcome {
        self.drop_reads(key);
        let outcome = actions::act(&self.mr(key), action);
        self.drop_reads(key);
        outcome
    }

    fn submit_review(
        &self,
        key: &PullRequestKey,
        verdict: PullRequestReviewVerdict,
        head: &str,
        body: &str,
        comments: &[PullRequestReviewDraftComment],
    ) -> Outcome {
        self.drop_reads(key);
        let files = if comments.is_empty() {
            Vec::new()
        } else {
            match self.files(key, None) {
                Ok(files) => files.files,
                Err(error) => return Outcome::Rejected(error.rejection()),
            }
        };
        let outcome = actions::submit_review(&self.mr(key), &files, verdict, head, body, comments);
        self.drop_reads(key);
        outcome
    }

    fn commentable(
        &self,
        key: &PullRequestKey,
        head: &str,
        path: &str,
        side: ReviewSide,
        lines: (u32, u32),
    ) -> Result<Anchoring, ForgeError> {
        Ok(crate::forge::anchors::anchoring(
            &self.files(key, None)?,
            head,
            path,
            side,
            lines,
        ))
    }

    fn reanchor(
        &self,
        key: &PullRequestKey,
        comments: &[PullRequestReviewDraftComment],
    ) -> Result<(String, Vec<Moved>), ForgeError> {
        crate::forge::anchors::reanchor(self.files(key, None)?, comments, |revision, path| {
            self.file_text(key, revision, path)
        })
    }

    fn watch_detail(&self, key: &PullRequestKey) -> Result<PullRequestWatchRead, ForgeError> {
        let mr = self.mr(key);
        let read = mr.mr()?;
        Ok(PullRequestWatchRead {
            state: reads::state(&read)
                .ok_or_else(|| error(ForgeErrorKind::Uncertain, "GitLab state unreadable"))?,
            head_sha: text(&read, "sha"),
            base_branch: text(&read, "target_branch").unwrap_or_default(),
            checks: reads::checks(&read),
            mergeability: reads::mergeability(&read),
            viewer: mr.viewer()?,
            author: text(&read["author"], "username"),
        })
    }

    fn activity(
        &self,
        key: &PullRequestKey,
        _: &mut Tails,
    ) -> Result<Option<Vec<PullRequestRemark>>, ForgeError> {
        let (_, _, discussions, complete) = self.mr(key).discussions()?;
        Ok(complete.then(|| reads::remarks(&discussions)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// gitlab.com is listed like any other server, once settings, a glab login or the
    /// environment name it, never by default.
    #[test]
    fn gitlab_com_is_listed_once_something_names_it() {
        let root =
            std::env::temp_dir().join(format!("tcode-gitlab-hosts-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let host = |environment: Vec<(String, String)>| {
            GitLab::new(
                SettingsStore::new(root.clone()),
                environment,
                Arc::new(ViewedMarks::new(root.join("viewed-marks.json"))),
            )
        };
        let quiet = host(Vec::new());
        assert!(quiet.credential_status().is_empty());
        quiet.configure(BTreeMap::from([(
            "gitlab.com".to_owned(),
            HostSettings::new(HostKind::Gitlab),
        )]));
        assert!(quiet.credential_status()["gitlab.com"].added);
        let named = host(vec![("GITLAB_TOKEN".into(), "secret".into())]);
        let status = named.credential_status();
        assert_eq!(status.keys().collect::<Vec<_>>(), ["gitlab.com"]);
        assert!(!status["gitlab.com"].added);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn saved_verification_survives_a_fresh_host() {
        crate::forge::verification_tests::verification_survives_restart(
            HostKind::Gitlab,
            "gitlab.com",
            "team/repo",
            |store, agent| {
                let viewed = Arc::new(ViewedMarks::new(store.data_file("viewed-marks.json")));
                let mut host = GitLab::new(store, Vec::new(), viewed);
                Arc::get_mut(&mut Arc::get_mut(&mut host).unwrap().api)
                    .unwrap()
                    .agent = agent;
                host
            },
        );
    }
}
