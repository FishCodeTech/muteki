//! Bitbucket Cloud behind the pull request host boundary: bitbucket.org alone, over REST 2.0.
//! Bitbucket Data Center is not read. Call blocking entries via HostCx::unblock.

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
use api::{Api, HOST, PAGE_LIMIT, Request, error};
use reads::{MAX_PAGES, Pr, text};
use serde_json::Value;
use std::{
    collections::{BTreeMap, HashMap},
    sync::{Arc, Mutex},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tcode_core::{
    pull_request::{
        BITBUCKET, HostKind, HostTerms, Mergeability, PullRequestKey, PullRequestMergeMethod,
        PullRequestReviewDraftComment, PullRequestStackState,
    },
    pull_request_watch::{CheckStatus, PullRequestRemark, PullRequestWatchRead},
    session::ReviewSide,
    settings::{CredentialSource, HostProblem, HostSettings, HostStatus},
};
use tcode_protocol::{
    PullRequestAction, PullRequestActionResult as Outcome, PullRequestActionState,
    PullRequestCapabilities, PullRequestConversation, PullRequestFileText, PullRequestFiles,
    PullRequestMedia, PullRequestPatch, PullRequestRead, PullRequestReadResponse,
    PullRequestReviewVerdict, PullRequestReviewerCandidate, PullRequestReviewerCandidates,
    PullRequestThreadReplies, PullRequestViewedFiles,
};

const READ_TTL: Duration = Duration::from_secs(60);
/// How long a summary is kept while reading anonymously: Bitbucket allows 60 anonymous
/// requests an hour, and a summary each minute (two requests) for one open pull request spends
/// them in thirty minutes.
const ANONYMOUS_TTL: Duration = Duration::from_secs(600);
/// Text at a commit never changes; the bound is only on how long it holds memory.
const TEXT_TTL: Duration = Duration::from_secs(600);
const FILE_BYTES: usize = 1024 * 1024;
const ANONYMOUS: &str = "anonymous";

type Slot = (Instant, PullRequestReadResponse);
/// Each pull request's whole diff, `None` past the read limit, by when and at which head it
/// was read.
type Diffs = HashMap<PullRequestKey, (Instant, String, Option<String>)>;
/// Each changed path's blob at the head, with the files read beside it.
type Revisions = (PullRequestFiles, BTreeMap<String, String>);

pub struct Bitbucket {
    api: Arc<Api>,
    viewed: Arc<ViewedMarks>,
    reads: Mutex<HashMap<(PullRequestKey, String), Slot>>,
    /// The whole diff, which the files, viewed marks and anchoring reads all page from.
    diffs: Mutex<Diffs>,
    /// Each pull request's last summary, kept for [`Bitbucket::sync_ttl`].
    summaries: Mutex<HashMap<PullRequestKey, (Instant, Summary)>>,
}

impl Bitbucket {
    pub(crate) fn new(
        store: SettingsStore,
        environment: impl IntoIterator<Item = (String, String)>,
        viewed: Arc<ViewedMarks>,
    ) -> Arc<Self> {
        Arc::new(Self {
            viewed,
            api: Api::new(store, environment),
            reads: Mutex::default(),
            diffs: Mutex::default(),
            summaries: Mutex::default(),
        })
    }

    fn pr<'a>(&'a self, key: &'a PullRequestKey) -> Pr<'a> {
        Pr {
            api: &self.api,
            key,
        }
    }

    /// How long the reads a sync repeats are kept: longer while no credential reads.
    fn sync_ttl(&self) -> Duration {
        match self.api.credential(HOST) {
            Ok(None) => ANONYMOUS_TTL,
            _ => READ_TTL,
        }
    }

    fn now() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
    }

    /// A read kept for `ttl`; a write drops every read of its pull request.
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

    /// The reading account's uuid and nickname, `None` anonymously or with an access token
    /// Bitbucket names no account for.
    fn viewer(&self) -> Result<Option<(String, Option<String>)>, ForgeError> {
        Ok(self
            .api
            .viewer()?
            .and_then(|user| Some((text(&user, "uuid")?, reads::login(&user)))))
    }

    /// Opaque, and different for each account: the viewer's uuid, else the credential's own
    /// fingerprint.
    fn account_of(&self, viewer: Option<&str>) -> Result<String, ForgeError> {
        Ok(match (viewer, self.api.credential(HOST)?) {
            (Some(uuid), _) => format!("{HOST}:{uuid}"),
            (None, Some(credential)) => format!("{HOST}:token:{}", credential.fingerprint()),
            (None, None) => format!("{HOST}:{ANONYMOUS}"),
        })
    }

    /// `/conflicts` while open; Bitbucket has no other word on mergeability.
    fn mergeability(&self, key: &PullRequestKey, pr: &Value) -> Result<Mergeability, ForgeError> {
        if reads::state(pr) != Some(tcode_core::pull_request::PullRequestState::Open) {
            return Ok(Mergeability::Unknown);
        }
        // `/conflicts` is not in Bitbucket's published API (it is what its own page reads), so
        // an answer it stops giving leaves mergeability unknown rather than failing the read; a
        // rate limit, a refused credential or a host turned off still say so.
        match self.pr(key).conflicts() {
            Ok(read) => Ok(read),
            Err(ForgeError {
                kind:
                    ForgeErrorKind::NotFound
                    | ForgeErrorKind::Refused { .. }
                    | ForgeErrorKind::Uncertain,
                ..
            }) => Ok(Mergeability::Unknown),
            Err(failure) => Err(failure),
        }
    }

    /// The whole diff at the pull request's head, `None` past the read limit.
    fn diff(&self, key: &PullRequestKey, head: &str) -> Result<Option<String>, ForgeError> {
        if let Some((at, held, diff)) = self.diffs.lock().unwrap().get(key)
            && held == head
            && at.elapsed() < READ_TTL
        {
            return Ok(diff.clone());
        }
        let response = self.api.send(Request {
            accept: "text/plain",
            ..Request::get(self.pr(key).path("/diff"), "Diff")
        })?;
        let diff =
            (!response.truncated).then(|| String::from_utf8_lossy(&response.body).into_owned());
        let mut diffs = self.diffs.lock().unwrap();
        diffs.retain(|_, (at, _, _)| at.elapsed() < READ_TTL);
        diffs.insert(key.clone(), (Instant::now(), head.to_owned(), diff.clone()));
        Ok(diff)
    }

    /// The commit the diff's old side reads at: where the source left the destination.
    fn merge_base(&self, key: &PullRequestKey, pr: &Value) -> Result<String, ForgeError> {
        let (Some(head), Some(destination)) =
            (reads::head(pr), text(&pr["destination"]["commit"], "hash"))
        else {
            return Err(error(
                ForgeErrorKind::Uncertain,
                "Bitbucket pull request names no commits",
            ));
        };
        // A fork's head is not in the destination repository, so it is named in its own, as
        // Bitbucket's comment links name a comparison.
        let source = text(&pr["source"]["repository"], "full_name")
            .filter(|source| !source.eq_ignore_ascii_case(&key.repository))
            .map(|source| format!("{source}:"))
            .unwrap_or_default();
        let base = self.pr(key).get(
            self.pr(key).repository_path(&format!(
                "/merge-base/{source}{head}..{destination}?fields=hash"
            )),
            "MergeBase",
        )?;
        text(&base, "hash")
            .ok_or_else(|| error(ForgeErrorKind::Uncertain, "Bitbucket merge base unreadable"))
    }

    /// The whole diff when it is within the read limit; past it, the diffstat's files from
    /// `cursor`, a page Bitbucket named, without their hunks.
    fn files(
        &self,
        key: &PullRequestKey,
        cursor: Option<String>,
    ) -> Result<PullRequestFiles, ForgeError> {
        let pr = self.pr(key).pr()?;
        let head = reads::head(&pr).unwrap_or_default();
        let base = self.merge_base(key, &pr)?;
        if cursor.is_none()
            && let Some(diff) = self.diff(key, &head)?
        {
            let files = crate::github::pull_request_reads::diff_files(&diff)
                .ok_or_else(|| error(ForgeErrorKind::Uncertain, "Bitbucket diff unreadable"))?;
            return Ok(PullRequestFiles {
                base,
                head,
                changed_files: files.len() as u64,
                files,
                next_cursor: None,
                complete: true,
            });
        }
        let at = cursor.unwrap_or_else(|| {
            self.pr(key)
                .path(&format!("/diffstat?pagelen={PAGE_LIMIT}"))
        });
        let page: Value = self.api.send(Request::get(at, "Diffstat"))?.json()?;
        let next = text(&page, "next");
        Ok(PullRequestFiles {
            base,
            head,
            files: page["values"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|row| {
                    let path = text(&row["new"], "path").or_else(|| text(&row["old"], "path"))?;
                    let previous = text(&row["old"], "path").filter(|old| *old != path);
                    Some(tcode_protocol::PullRequestFile {
                        kind: match row["status"].as_str().unwrap_or_default() {
                            "added" => agent::FileChangeKind::Create,
                            "removed" => agent::FileChangeKind::Delete,
                            "renamed" => agent::FileChangeKind::Rename,
                            _ => agent::FileChangeKind::Modify,
                        },
                        previous_path: previous,
                        additions: row["lines_added"].as_u64().unwrap_or(0),
                        deletions: row["lines_removed"].as_u64().unwrap_or(0),
                        patch: PullRequestPatch::Withheld,
                        path,
                    })
                })
                .collect(),
            complete: next.is_none(),
            next_cursor: next,
            changed_files: page["size"].as_u64().unwrap_or(0),
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
        let encoded: Vec<_> = path
            .split('/')
            .map(crate::github::pull_request_reads::percent_encode)
            .collect();
        let answer = self.api.send(Request {
            accept: "application/octet-stream",
            limit: FILE_BYTES,
            ..Request::get(
                self.pr(key)
                    .repository_path(&format!("/src/{revision}/{}", encoded.join("/"))),
                "FileText",
            )
        });
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
        let pr = self.pr(key);
        let read = pr.pr()?;
        let (comments, complete) = pr.comments()?;
        let viewer = self.viewer()?;
        let uuid = viewer.as_ref().map(|(uuid, _)| uuid.as_str());
        let signed_in = self.api.credential(HOST)?.is_some();
        let conversation = reads::conversation(&read, &comments, uuid, signed_in);
        Ok(PullRequestConversation {
            description: conversation.description,
            comments: conversation.comments,
            threads: conversation.threads,
            complete,
            account: self.account_of(uuid)?,
            permissions: conversation.permissions,
            labels: Vec::new(),
            reviewers: conversation.reviewers,
            capabilities: self.capabilities(key),
        })
    }

    /// Each changed path's blob at the head, from the whole diff's index lines; `None` past
    /// the diff's read limit.
    fn revisions(&self, key: &PullRequestKey) -> Result<Option<Revisions>, ForgeError> {
        let files = self.files(key, None)?;
        let Some(diff) = self.diff(key, &files.head)? else {
            return Ok(None);
        };
        Ok(Some((files, crate::forge::revisions(&diff))))
    }

    fn viewed_files(&self, key: &PullRequestKey) -> Result<PullRequestViewedFiles, ForgeError> {
        let account = self.account(key)?;
        let Some((files, revisions)) = self.revisions(key)? else {
            return Ok(PullRequestViewedFiles {
                files: Vec::new(),
                complete: false,
            });
        };
        Ok(PullRequestViewedFiles {
            files: self.viewed.states(&account, key, &revisions),
            complete: files.complete && revisions.len() == files.files.len(),
        })
    }

    /// The pull request's reviewers, then the repository's default reviewers and the
    /// workspace's members, one page of each. A workspace that hides its members from the
    /// account leaves the list incomplete.
    fn reviewer_candidates(
        &self,
        key: &PullRequestKey,
    ) -> Result<PullRequestReviewerCandidates, ForgeError> {
        let pr = self.pr(key);
        let read = pr.pr()?;
        let author = read["author"]["uuid"].as_str();
        let (defaults, more_defaults) = self.api.list(
            &pr.repository_path(&format!(
                "/effective-default-reviewers?pagelen={PAGE_LIMIT}"
            )),
            "DefaultReviewers",
            1,
        )?;
        let workspace = key.repository.split('/').next().unwrap_or_default();
        let members = self.api.list(
            &format!("/workspaces/{workspace}/members?pagelen={PAGE_LIMIT}"),
            "WorkspaceMembers",
            1,
        );
        let (members, more_members) = match members {
            Ok(members) => (members.0, members.1.is_some()),
            Err(ForgeError {
                kind: ForgeErrorKind::Refused { .. } | ForgeErrorKind::Unauthorized,
                ..
            }) => (Vec::new(), true),
            Err(failure) => return Err(failure),
        };
        let candidate = |user: &Value, requested: bool| {
            Some(PullRequestReviewerCandidate {
                reviewer: reads::user_reviewer(user)?,
                name: text(user, "display_name").filter(|name| !name.is_empty()),
                avatar_url: text(&user["links"]["avatar"], "href"),
                requested,
            })
        };
        let mut reviewers: Vec<_> = read["reviewers"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|user| candidate(user, true))
            .collect();
        let others = defaults
            .iter()
            .map(|row| &row["user"])
            .chain(members.iter().map(|row| &row["user"]));
        for user in others {
            let Some(uuid) = user["uuid"].as_str() else {
                continue;
            };
            if Some(uuid) == author || reviewers.iter().any(|known| known.reviewer.id == uuid) {
                continue;
            }
            reviewers.extend(candidate(user, false));
        }
        Ok(PullRequestReviewerCandidates {
            reviewers,
            complete: more_defaults.is_none() && !more_members,
        })
    }

    fn action_state(&self, key: &PullRequestKey) -> Result<PullRequestActionState, ForgeError> {
        let pr = self.pr(key);
        let read = pr.pr()?;
        let head = reads::head(&read).unwrap_or_default();
        let checks = pr.checks(&head)?;
        let mergeability = self.mergeability(key, &read)?;
        let signed_in = self.api.credential(HOST)?.is_some();
        Ok(PullRequestActionState {
            merge_state: reads::merge_state(&read, mergeability, &checks),
            head,
            // Bitbucket says nothing of how far the destination has moved on.
            behind_by: None,
            merge_queue: false,
            // Bitbucket publishes no repository's allowed strategies; it refuses one it
            // does not allow.
            merge_methods: vec![
                PullRequestMergeMethod::Merge,
                PullRequestMergeMethod::Squash,
                PullRequestMergeMethod::Rebase,
            ],
            auto_merge_allowed: false,
            auto_merge: None,
            queued: false,
            queue_position: None,
            failing_checks: checks
                .iter()
                .filter(|check| check.status.failed())
                .map(|check| check.name.clone())
                .collect(),
            pending_checks: checks
                .iter()
                .filter(|check| check.status == CheckStatus::Pending)
                .count() as u32,
            // Bitbucket's permission read is gone (it answers 410), so whoever signs in may
            // try, and Bitbucket refuses what they may not do.
            can_update: signed_in,
            can_update_branch: false,
            can_merge: signed_in,
            capabilities: self.capabilities(key),
        })
    }

    /// Attachments on bitbucket.org that the conversation names, read with the credential;
    /// anything else, avatars included, the client draws by its URL.
    fn media(
        &self,
        key: &PullRequestKey,
        url: &str,
        validator: Option<&str>,
    ) -> Result<PullRequestMedia, ForgeError> {
        let Ok(parsed) = url::Url::parse(url) else {
            return Ok(PullRequestMedia::Unsupported);
        };
        let own = parsed.scheme() == "https"
            && parsed.host_str() == Some(HOST)
            && parsed.port().is_none()
            && (parsed.path().starts_with("/repo/")
                || parsed
                    .path()
                    .to_ascii_lowercase()
                    .starts_with(&format!("/{}/", key.repository)));
        if !own {
            return Ok(PullRequestMedia::Unsupported);
        }
        let conversation = self.conversation_read(key)?;
        let named = std::iter::once(&conversation.description)
            .chain(&conversation.comments)
            .chain(
                conversation
                    .threads
                    .iter()
                    .flat_map(|thread| &thread.comments),
            )
            .any(|comment| comment.body.contains(url));
        if !named {
            return Err(error(
                ForgeErrorKind::InvalidInput,
                "media not in the conversation",
            ));
        }
        self.api.media(&parsed, validator)
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
        self.diffs.lock().unwrap().remove(key);
        self.summaries.lock().unwrap().remove(key);
    }
}

impl Forge for Bitbucket {
    fn terms(&self, _: &PullRequestKey) -> &'static HostTerms {
        &BITBUCKET
    }

    fn capabilities(&self, _: &PullRequestKey) -> PullRequestCapabilities {
        reads::capabilities()
    }

    fn configure(&self, hosts: BTreeMap<String, HostSettings>) {
        self.api.configure(
            hosts
                .into_iter()
                .filter(|(_, choice)| choice.kind == HostKind::Bitbucket)
                .collect(),
        );
    }

    /// bitbucket.org, once settings or `BITBUCKET_TOKEN` name it.
    fn credential_status(&self) -> BTreeMap<String, HostStatus> {
        let configured = self.api.configured();
        if !configured.contains_key(HOST) && !self.api.environment_names_host() {
            return BTreeMap::new();
        }
        let enabled = configured.get(HOST).is_none_or(|choice| choice.enabled);
        let credential = self.api.credential(HOST).ok().flatten();
        let source = credential
            .as_ref()
            .map(|credential| credential.source.clone());
        let problem = match &credential {
            None => enabled.then(|| HostProblem::NoCredential {
                tools_missing: Vec::new(),
            }),
            Some(credential)
                if enabled
                    && credential.source == CredentialSource::SavedBearer
                    && !self.api.verified(credential) =>
            {
                Some(HostProblem::Unverified)
            }
            Some(_) => None,
        };
        BTreeMap::from([(
            HOST.to_owned(),
            HostStatus {
                kind: HostKind::Bitbucket,
                added: !self.api.environment_names_host(),
                token_set: self.api.token_saved(),
                source,
                accounts: Vec::new(),
                env_overrides_account: false,
                order: vec![
                    CredentialSource::Saved,
                    CredentialSource::Env {
                        name: api::ENV_TOKEN.into(),
                    },
                ],
                problem,
                token_url: Some(
                    "https://id.atlassian.com/manage-profile/security/api-tokens".into(),
                ),
                access_token_url: Some(
                    "https://support.atlassian.com/bitbucket-cloud/docs/access-tokens/".into(),
                ),
                token_scopes: Vec::new(),
            },
        )])
    }

    // The credential is read from the store on every request; what was read with the one
    // before is dropped, as reads kept longer while anonymous would otherwise outlive a token.
    fn forget_credential(&self, _: &str) {
        self.api.forget();
        self.summaries.lock().unwrap().clear();
    }

    fn pull_request_url(&self, url: &str) -> Option<(PullRequestKey, String)> {
        repository::pull_request_url(url)
    }

    fn checkout_repository(&self, checkout: &crate::forge::Checkout<'_>) -> Option<Repository> {
        repository::resolve(checkout)
    }

    fn repository(&self, name: &str, _: &str) -> Option<Repository> {
        repository::selector(name)
    }

    fn url(&self, key: &PullRequestKey) -> Option<String> {
        Some(repository::url(key))
    }

    /// The pull request and its head's build statuses.
    fn summary(&self, key: &PullRequestKey) -> Result<Summary, ForgeError> {
        let ttl = self.sync_ttl();
        if let Some((at, summary)) = self.summaries.lock().unwrap().get(key)
            && at.elapsed() < ttl
        {
            return Ok(summary.clone());
        }
        let pr = self.pr(key);
        let read = pr.pr()?;
        let checks = pr.checks(&reads::head(&read).unwrap_or_default())?;
        let summary = Summary {
            snapshot: reads::snapshot(&read, &checks, Self::now()).ok_or_else(|| {
                error(
                    ForgeErrorKind::Uncertain,
                    "Bitbucket pull request unreadable",
                )
            })?,
            stack_number: None,
        };
        let mut summaries = self.summaries.lock().unwrap();
        summaries.retain(|_, (at, _)| at.elapsed() < ANONYMOUS_TTL);
        summaries.insert(key.clone(), (Instant::now(), summary.clone()));
        Ok(summary)
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
                // A cursor is a page Bitbucket named, which the request checks is its own.
                self.cached(key, format!("files:{cursor:?}"), READ_TTL, || {
                    self.files(key, cursor).map(PullRequestReadResponse::Files)
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
            PullRequestRead::LabelCandidates => {
                Err(error(ForgeErrorKind::NotFound, "no labels on this host"))
            }
            PullRequestRead::ReviewerCandidates => {
                self.cached(key, "reviewers".into(), READ_TTL, || {
                    self.reviewer_candidates(key)
                        .map(PullRequestReadResponse::ReviewerCandidates)
                })
            }
            // Read fresh: a merge decides on it.
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

    fn account(&self, _: &PullRequestKey) -> Result<String, ForgeError> {
        let viewer = self.viewer()?;
        self.account_of(viewer.as_ref().map(|(uuid, _)| uuid.as_str()))
    }

    fn set_viewed(
        &self,
        key: &PullRequestKey,
        paths: &[String],
        viewed: bool,
    ) -> Result<(), ForgeError> {
        let account = self.account(key)?;
        let (_, revisions) = self
            .revisions(key)?
            .ok_or_else(|| error(ForgeErrorKind::Uncertain, "Bitbucket file revisions unread"))?;
        self.viewed
            .set(&account, key, &revisions, paths, viewed)
            .map_err(|_| error(ForgeErrorKind::Uncertain, "viewed marks not saved"))
    }

    fn act(&self, key: &PullRequestKey, action: &PullRequestAction) -> Outcome {
        self.drop_reads(key);
        let outcome = actions::act(&self.pr(key), action);
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
        let outcome = actions::submit_review(&self.pr(key), &files, verdict, head, body, comments);
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

    /// The pull request, its head's statuses and, while open, its conflicts.
    fn watch_detail(&self, key: &PullRequestKey) -> Result<PullRequestWatchRead, ForgeError> {
        let pr = self.pr(key);
        let read = pr.pr()?;
        let head = reads::head(&read);
        Ok(PullRequestWatchRead {
            state: reads::state(&read)
                .ok_or_else(|| error(ForgeErrorKind::Uncertain, "Bitbucket state unreadable"))?,
            checks: pr.checks(head.as_deref().unwrap_or_default())?,
            mergeability: self.mergeability(key, &read)?,
            head_sha: head,
            base_branch: text(&read["destination"]["branch"], "name").unwrap_or_default(),
            viewer: self.viewer()?.and_then(|(_, login)| login),
            author: reads::login(&read["author"]),
        })
    }

    fn activity(
        &self,
        key: &PullRequestKey,
        _: &mut Tails,
    ) -> Result<Option<Vec<PullRequestRemark>>, ForgeError> {
        let (entries, next) = self.api.list(
            &self
                .pr(key)
                .path(&format!("/activity?pagelen={PAGE_LIMIT}")),
            "Activity",
            MAX_PAGES,
        )?;
        Ok(next.is_none().then(|| reads::remarks(&entries)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// bitbucket.org is listed once settings or `BITBUCKET_TOKEN` name it, never by default.
    #[test]
    fn bitbucket_org_is_listed_once_something_names_it() {
        let root =
            std::env::temp_dir().join(format!("tcode-bitbucket-hosts-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let host = |environment: Vec<(String, String)>| {
            Bitbucket::new(
                SettingsStore::new(root.clone()),
                environment,
                Arc::new(ViewedMarks::new(root.join("viewed-marks.json"))),
            )
        };
        let quiet = host(Vec::new());
        assert!(quiet.credential_status().is_empty());
        quiet.configure(BTreeMap::from([(
            HOST.to_owned(),
            HostSettings::new(HostKind::Bitbucket),
        )]));
        assert!(quiet.credential_status()[HOST].added);
        let named = host(vec![(api::ENV_TOKEN.into(), "secret".into())]);
        let status = named.credential_status();
        assert_eq!(status.keys().collect::<Vec<_>>(), [HOST]);
        assert!(!status[HOST].added);
        let _ = std::fs::remove_dir_all(root);
    }
    #[test]
    fn saved_verification_survives_a_fresh_host() {
        crate::forge::verification_tests::verification_survives_restart(
            HostKind::Bitbucket,
            "bitbucket.org",
            "team/repo",
            |store, agent| {
                let viewed = Arc::new(ViewedMarks::new(store.data_file("viewed-marks.json")));
                let mut host = Bitbucket::new(store, Vec::new(), viewed);
                Arc::get_mut(&mut Arc::get_mut(&mut host).unwrap().api)
                    .unwrap()
                    .agent = agent;
                host
            },
        );
    }
}
