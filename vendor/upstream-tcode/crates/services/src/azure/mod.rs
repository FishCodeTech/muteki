//! Azure DevOps Services behind the pull request host boundary, over REST 7.1. Each
//! organization is a host, `dev.azure.com/{organization}`, with its own credential. Azure
//! DevOps Server is not read. Call blocking entries via HostCx::unblock.

mod actions;
mod api;
mod diff;
mod reads;
mod repository;

use crate::{
    forge::{
        Anchoring, Forge, ForgeError, ForgeErrorKind, Moved, Repository, Summary, Tails,
        ViewedMarks,
    },
    settings::SettingsStore,
};
use api::{Api, PAGE_LIMIT, Request, error};
use diff::Change;
use reads::{Iteration, Pr, text};
use serde_json::Value;
use std::{
    collections::{BTreeMap, HashMap},
    sync::{Arc, Mutex},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tcode_core::{
    pull_request::{
        AZURE, HostKind, HostTerms, Mergeability, PullRequestKey, PullRequestMergeMethod,
        PullRequestReviewDraftComment, PullRequestStackState, PullRequestState,
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
/// Past this many pages of an iteration's changes the list is reported incomplete.
const MAX_PAGES: usize = 10;
/// One files read builds hunks for at most this many files, or this much hunk text, and names
/// where the next read carries on: each file is two reads of its text.
const SLICE_FILES: usize = 30;
const SLICE_BYTES: usize = 256 * 1024;
/// Files whose texts are read at once.
const CONCURRENCY: usize = 6;
/// The revision a deleted file's viewed mark holds.
const DELETED: &str = "0000000000000000000000000000000000000000";

type Slot = (Instant, PullRequestReadResponse);
/// An iteration's changed files, and whether the list is whole, by pull request and iteration.
type Changes = HashMap<(PullRequestKey, u64), (Instant, Arc<(Vec<Change>, bool)>)>;

pub struct Azure {
    api: Arc<Api>,
    viewed: Arc<ViewedMarks>,
    reads: Mutex<HashMap<(PullRequestKey, String), Slot>>,
    /// An iteration's changes never change; the files, viewed marks and anchoring reads page
    /// through them.
    changes: Mutex<Changes>,
}

/// Files of the latest push, the push's id, and Azure's change entries of those files, which a
/// thread on their lines names.
struct Latest {
    files: PullRequestFiles,
    iteration: u64,
    changes: Vec<Change>,
}

/// Where a files read carries on: the iteration it reads, so a push in between does not
/// renumber the list under it, and the index of the next file.
fn cursor(raw: &str) -> Option<(u64, usize)> {
    let (iteration, index) = raw.split_once(':')?;
    let digits = |part: &str| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit());
    (digits(iteration) && digits(index))
        .then(|| Some((iteration.parse().ok()?, index.parse().ok()?)))?
}

impl Azure {
    pub(crate) fn new(
        store: SettingsStore,
        environment: impl IntoIterator<Item = (String, String)>,
        viewed: Arc<ViewedMarks>,
    ) -> Arc<Self> {
        Arc::new(Self {
            viewed,
            api: Api::new(store, environment),
            reads: Mutex::default(),
            changes: Mutex::default(),
        })
    }

    fn pr<'a>(&'a self, key: &'a PullRequestKey) -> Pr<'a> {
        Pr {
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

    /// The reading account's identity id, `None` anonymously.
    fn viewer_id(&self, key: &PullRequestKey) -> Result<Option<String>, ForgeError> {
        Ok(self
            .pr(key)
            .viewer()?
            .and_then(|viewer| text(&viewer, "id")))
    }

    fn account_of(key: &PullRequestKey, viewer: Option<&str>) -> String {
        format!("{}:{}", key.host, viewer.unwrap_or(ANONYMOUS))
    }

    /// The trial merge; a pull request no longer open has none to read.
    fn mergeability(pr: &Value) -> Mergeability {
        if reads::state(pr) != Some(PullRequestState::Open) {
            return Mergeability::Unknown;
        }
        reads::mergeability(pr)
    }

    /// Every push, oldest first. Azure keeps them from anonymous reads.
    fn iterations(&self, key: &PullRequestKey) -> Result<Vec<Iteration>, ForgeError> {
        let mut iterations: Vec<_> = self
            .pr(key)
            .iterations()?
            .iter()
            .filter_map(reads::iteration)
            .collect();
        iterations.sort_by_key(|iteration| iteration.id);
        Ok(iterations)
    }

    fn latest(&self, key: &PullRequestKey) -> Result<Iteration, ForgeError> {
        self.iterations(key)?
            .pop()
            .ok_or_else(|| error(ForgeErrorKind::Uncertain, "Azure DevOps names no push"))
    }

    /// The files an iteration changed from the target, page by page up to [`MAX_PAGES`], and
    /// whether that was all of them.
    fn changes(
        &self,
        key: &PullRequestKey,
        iteration: u64,
    ) -> Result<Arc<(Vec<Change>, bool)>, ForgeError> {
        let slot = (key.clone(), iteration);
        if let Some((at, held)) = self.changes.lock().unwrap().get(&slot)
            && at.elapsed() < TEXT_TTL
        {
            return Ok(held.clone());
        }
        let pr = self.pr(key);
        let mut changes = Vec::new();
        let mut skip = Some(0);
        for _ in 0..MAX_PAGES {
            let Some(at) = skip.take() else {
                break;
            };
            let page = pr.get(
                pr.path(&format!(
                    "/iterations/{iteration}/changes?$top={PAGE_LIMIT}&$skip={at}&$compareTo=0"
                )),
                "IterationChanges",
            )?;
            let entries = page["changeEntries"]
                .as_array()
                .cloned()
                .unwrap_or_default();
            changes.extend(entries.iter().filter_map(diff::change));
            skip = page["nextSkip"]
                .as_u64()
                .filter(|next| *next > at as u64 && !entries.is_empty())
                .map(|next| next as usize);
        }
        let held = Arc::new((changes, skip.is_none()));
        let mut all = self.changes.lock().unwrap();
        all.retain(|_, (at, _)| at.elapsed() < TEXT_TTL);
        all.insert(slot, (Instant::now(), held.clone()));
        Ok(held)
    }

    /// The listed changes' files with their hunks, their texts read a few at once.
    fn built(
        &self,
        key: &PullRequestKey,
        iteration: &Iteration,
        changes: &[Change],
    ) -> Result<Vec<tcode_protocol::PullRequestFile>, ForgeError> {
        let read = |change: &Change| -> Result<_, ForgeError> {
            let old = match change.kind {
                agent::FileChangeKind::Create => None,
                _ => Some(self.file_text(
                    key,
                    &iteration.base,
                    change.previous_path.as_deref().unwrap_or(&change.path),
                )?),
            };
            let new = match change.kind {
                agent::FileChangeKind::Delete => None,
                _ => Some(self.file_text(key, &iteration.head, &change.path)?),
            };
            Ok(diff::file(change, old, new))
        };
        let mut files = Vec::with_capacity(changes.len());
        for chunk in changes.chunks(CONCURRENCY) {
            let built: Vec<_> = std::thread::scope(|scope| {
                let reads: Vec<_> = chunk
                    .iter()
                    .map(|change| scope.spawn(move || read(change)))
                    .collect();
                reads
                    .into_iter()
                    .map(|handle| {
                        handle.join().unwrap_or_else(|_| {
                            Err(error(
                                ForgeErrorKind::Uncertain,
                                "Azure DevOps diff unbuilt",
                            ))
                        })
                    })
                    .collect()
            });
            for file in built {
                files.push(file?);
            }
        }
        Ok(files)
    }

    /// From `cursor` on, or the latest push's first files: as many as one slice holds, with
    /// where the next slice starts.
    fn files(
        &self,
        key: &PullRequestKey,
        cursor: Option<(u64, usize)>,
    ) -> Result<PullRequestFiles, ForgeError> {
        let iterations = self.iterations(key)?;
        let iteration = match cursor {
            Some((id, _)) => iterations.into_iter().find(|iteration| iteration.id == id),
            None => iterations.into_iter().last(),
        }
        .ok_or_else(|| error(ForgeErrorKind::InvalidInput, "no such Azure DevOps push"))?;
        let start = cursor.map_or(0, |(_, index)| index);
        let changes = self.changes(key, iteration.id)?;
        let (all, whole) = (&changes.0, changes.1);
        let mut files = Vec::new();
        let mut bytes = 0;
        let mut next = start.min(all.len());
        while next < all.len() && files.len() < SLICE_FILES && bytes < SLICE_BYTES {
            let end = (next + CONCURRENCY).min(all.len());
            for file in self.built(key, &iteration, &all[next..end])? {
                if let tcode_protocol::PullRequestPatch::Hunks(hunks) = &file.patch {
                    bytes += hunks.len();
                }
                files.push(file);
            }
            next = end;
        }
        let more = next < all.len();
        Ok(PullRequestFiles {
            base: iteration.base.clone(),
            head: iteration.head.clone(),
            files,
            next_cursor: more.then(|| format!("{}:{next}", iteration.id)),
            complete: !more && whole,
            changed_files: all.len() as u64,
        })
    }

    /// The latest push's files at `paths`, with their hunks.
    fn files_at(&self, key: &PullRequestKey, paths: &[&str]) -> Result<Latest, ForgeError> {
        let iteration = self.latest(key)?;
        let changes = self.changes(key, iteration.id)?;
        let wanted: Vec<_> = changes
            .0
            .iter()
            .filter(|change| paths.contains(&change.path.as_str()))
            .cloned()
            .collect();
        Ok(Latest {
            files: PullRequestFiles {
                files: self.built(key, &iteration, &wanted)?,
                next_cursor: None,
                complete: changes.1,
                changed_files: changes.0.len() as u64,
                base: iteration.base,
                head: iteration.head,
            },
            iteration: iteration.id,
            changes: wanted,
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
        let pr = self.pr(key);
        let answer = self.api.send(
            pr.authority(),
            Request {
                accept: "application/octet-stream",
                limit: FILE_BYTES,
                ..Request::get(
                    pr.repository_path(&format!(
                        "/items?path={}&versionDescriptor.version={revision}&versionDescriptor.versionType=commit&$format=octetStream",
                        crate::github::pull_request_reads::percent_encode(&format!("/{path}"))
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
        let pr = self.pr(key);
        let read = pr.pr()?;
        let threads = pr.threads()?;
        let viewer = self.viewer_id(key)?;
        // Anonymously Azure keeps the pushes and the labels to itself.
        let (iterations, labels) = if viewer.is_some() {
            (self.iterations(key)?, pr.labels()?)
        } else {
            (Vec::new(), Vec::new())
        };
        let url = repository::url(key);
        let conversation =
            reads::conversation(&read, &threads, &iterations, viewer.as_deref(), &url);
        Ok(PullRequestConversation {
            description: conversation.description,
            comments: conversation.comments,
            threads: conversation.threads,
            complete: true,
            account: Self::account_of(key, viewer.as_deref()),
            permissions: conversation.permissions,
            labels,
            reviewers: conversation.reviewers,
            capabilities: self.capabilities(key),
        })
    }

    /// Each changed path's blob at the latest push, a deleted one's as a missing side, and
    /// whether every changed path is there.
    fn revisions(
        &self,
        key: &PullRequestKey,
    ) -> Result<(BTreeMap<String, String>, bool), ForgeError> {
        let iteration = self.latest(key)?;
        let changes = self.changes(key, iteration.id)?;
        let revisions = changes
            .0
            .iter()
            .filter_map(|change| match (&change.object, change.kind) {
                (_, agent::FileChangeKind::Delete) => {
                    Some((change.path.clone(), DELETED.to_owned()))
                }
                (Some(object), _) => Some((change.path.clone(), object.clone())),
                (None, _) => None,
            })
            .collect::<BTreeMap<_, _>>();
        let whole = changes.1 && revisions.len() == changes.0.len();
        Ok((revisions, whole))
    }

    fn viewed_files(&self, key: &PullRequestKey) -> Result<PullRequestViewedFiles, ForgeError> {
        let account = self.account(key)?;
        let (revisions, complete) = self.revisions(key)?;
        Ok(PullRequestViewedFiles {
            files: self.viewed.states(&account, key, &revisions),
            complete,
        })
    }

    /// The labels on the pull request, then the project's other tags, which labels share.
    fn label_candidates(
        &self,
        key: &PullRequestKey,
    ) -> Result<PullRequestLabelCandidates, ForgeError> {
        let pr = self.pr(key);
        let applied = pr.labels()?;
        let tags = self
            .api
            .send(
                pr.authority(),
                Request {
                    version: api::PREVIEW,
                    ..Request::get(pr.project_path("/_apis/wit/tags"), "Tags")
                },
            )
            .and_then(|response| response.json::<Value>());
        let (tags, complete) = match tags {
            Ok(tags) => (tags["value"].as_array().cloned().unwrap_or_default(), true),
            Err(ForgeError {
                kind: ForgeErrorKind::Refused { .. } | ForgeErrorKind::NotFound,
                ..
            }) => (Vec::new(), false),
            Err(failure) => return Err(failure),
        };
        let mut labels: Vec<_> = applied
            .into_iter()
            .map(|label| PullRequestLabelCandidate {
                id: label.id,
                name: label.name,
                color: None,
                description: None,
                applied: true,
            })
            .collect();
        for name in tags.iter().filter_map(|tag| tag["name"].as_str()) {
            if !labels
                .iter()
                .any(|label| label.name.eq_ignore_ascii_case(name))
            {
                labels.push(PullRequestLabelCandidate {
                    id: name.to_owned(),
                    name: name.to_owned(),
                    color: None,
                    description: None,
                    applied: false,
                });
            }
        }
        Ok(PullRequestLabelCandidates { labels, complete })
    }

    /// The reviewers on the pull request, then the members of the project's default team, one
    /// page of them.
    fn reviewer_candidates(
        &self,
        key: &PullRequestKey,
    ) -> Result<PullRequestReviewerCandidates, ForgeError> {
        let pr = self.pr(key);
        let read = pr.pr()?;
        let author = read["createdBy"]["id"].as_str();
        let project = pr.get(
            format!(
                "/_apis/projects/{}",
                crate::github::pull_request_reads::percent_encode(repository::names(key).0)
            ),
            "Project",
        )?;
        let members = match text(&project["defaultTeam"], "id") {
            Some(team) => pr
                .get(
                    format!(
                        "/_apis/projects/{}/teams/{team}/members?$top={PAGE_LIMIT}",
                        text(&project, "id").unwrap_or_default()
                    ),
                    "TeamMembers",
                )
                .map(|members| members["value"].as_array().cloned().unwrap_or_default()),
            None => Ok(Vec::new()),
        };
        let (members, complete) = match members {
            Ok(members) => {
                let complete = members.len() < PAGE_LIMIT;
                (members, complete)
            }
            Err(ForgeError {
                kind: ForgeErrorKind::Refused { .. } | ForgeErrorKind::NotFound,
                ..
            }) => (Vec::new(), false),
            Err(failure) => return Err(failure),
        };
        let mut reviewers: Vec<_> = read["reviewers"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|identity| {
                Some(PullRequestReviewerCandidate {
                    reviewer: reads::reviewer(identity)?,
                    name: text(identity, "displayName"),
                    avatar_url: text(identity, "imageUrl"),
                    requested: true,
                })
            })
            .collect();
        for identity in members.iter().map(|member| &member["identity"]) {
            let Some(reviewer) = reads::reviewer(identity) else {
                continue;
            };
            if Some(reviewer.id.as_str()) == author
                || reviewers
                    .iter()
                    .any(|known| known.reviewer.id == reviewer.id)
            {
                continue;
            }
            reviewers.push(PullRequestReviewerCandidate {
                reviewer,
                name: text(identity, "displayName"),
                avatar_url: text(identity, "imageUrl"),
                requested: false,
            });
        }
        Ok(PullRequestReviewerCandidates {
            reviewers,
            complete,
        })
    }

    fn action_state(&self, key: &PullRequestKey) -> Result<PullRequestActionState, ForgeError> {
        let pr = self.pr(key);
        let read = pr.pr()?;
        let checks = pr.checks(&read)?;
        let mergeability = Self::mergeability(&read);
        let signed_in = self.api.credential(&key.host)?.is_some();
        Ok(PullRequestActionState {
            merge_state: reads::merge_state(&read, mergeability, &checks),
            head: reads::head(&read).unwrap_or_default(),
            // Azure says nothing of how far the target has moved on.
            behind_by: None,
            merge_queue: false,
            // A branch policy may limit the strategies; Azure refuses one it does not allow.
            merge_methods: vec![
                PullRequestMergeMethod::Merge,
                PullRequestMergeMethod::Squash,
                PullRequestMergeMethod::Rebase,
            ],
            auto_merge_allowed: true,
            auto_merge: reads::auto_complete(&read),
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
            // Azure states no permission a read reaches: whoever signs in may try.
            can_update: signed_in,
            can_update_branch: false,
            can_merge: signed_in,
            capabilities: self.capabilities(key),
        })
    }

    /// Attachments anywhere in the organization that the conversation names, read with the
    /// organization's credential; anything else, avatars included, the client draws by its URL.
    fn media(
        &self,
        key: &PullRequestKey,
        url: &str,
        validator: Option<&str>,
    ) -> Result<PullRequestMedia, ForgeError> {
        let Ok(parsed) = url::Url::parse(url) else {
            return Ok(PullRequestMedia::Unsupported);
        };
        let path = parsed.path().to_ascii_lowercase();
        let own = parsed.scheme() == "https"
            && parsed.host_str() == Some(tcode_core::pull_request::AZURE_HOST)
            && parsed.port().is_none()
            && path.starts_with(&format!("/{}/", repository::organization(key)))
            && path.contains("/_apis/git/repositories/")
            && path.contains("/attachments/");
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

impl Forge for Azure {
    fn terms(&self, _: &PullRequestKey) -> &'static HostTerms {
        &AZURE
    }

    fn capabilities(&self, _: &PullRequestKey) -> PullRequestCapabilities {
        reads::capabilities()
    }

    fn configure(&self, hosts: BTreeMap<String, HostSettings>) {
        self.api.configure(
            hosts
                .into_iter()
                .filter(|(_, choice)| choice.kind == HostKind::Azure)
                .collect(),
        );
    }

    /// Each organization settings name, and the one `AZURE_DEVOPS_ORG` names for
    /// `AZURE_DEVOPS_EXT_PAT`. az's login names no organization, so it lists none.
    fn credential_status(&self) -> BTreeMap<String, HostStatus> {
        let configured = self.api.configured();
        let az = self.api.az_program().is_some();
        let mut hosts: BTreeMap<String, bool> =
            configured.keys().map(|host| (host.clone(), true)).collect();
        if let Some(host) = self.api.environment_host() {
            hosts.insert(host, false);
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
                    None if az => Some(HostProblem::NotSignedIn {
                        tool: "az".into(),
                        command: Some("az login".into()),
                    }),
                    None => Some(HostProblem::NoCredential {
                        tools_missing: vec!["az".into()],
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
                        kind: HostKind::Azure,
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
                            CredentialSource::Cli { tool: "az".into() },
                        ],
                        problem,
                        token_url: Some(format!("https://{host}/_usersSettings/tokens")),
                        access_token_url: None,
                        token_scopes: vec![
                            "Code (Read & write)".into(),
                            "Pull Request Threads (Read & manage)".into(),
                        ],
                    },
                )
            })
            .collect()
    }

    fn forget_credential(&self, host: &str) {
        self.api.forget(host);
    }

    fn pull_request_url(&self, url: &str) -> Option<(PullRequestKey, String)> {
        repository::pull_request_url(url)
    }

    fn checkout_repository(&self, checkout: &crate::forge::Checkout<'_>) -> Option<Repository> {
        repository::resolve(checkout)
    }

    fn repository(&self, name: &str, host: &str) -> Option<Repository> {
        repository::selector(name, host)
    }

    fn url(&self, key: &PullRequestKey) -> Option<String> {
        Some(repository::url(key))
    }

    /// The pull request with its statuses and policy evaluations.
    fn summary(&self, key: &PullRequestKey) -> Result<Summary, ForgeError> {
        let pr = self.pr(key);
        let read = pr.pr()?;
        let checks = pr.checks(&read)?;
        Ok(Summary {
            snapshot: reads::snapshot(&read, &checks, Self::mergeability(&read), Self::now())
                .ok_or_else(|| {
                    error(
                        ForgeErrorKind::Uncertain,
                        "Azure DevOps pull request unreadable",
                    )
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
            PullRequestRead::Files { cursor: raw } => {
                // A cursor Tcode did not write is refused rather than read from the top.
                let at = match raw.as_deref() {
                    Some(raw) => Some(cursor(raw).ok_or_else(|| {
                        error(ForgeErrorKind::InvalidInput, "invalid Azure DevOps cursor")
                    })?),
                    None => None,
                };
                self.cached(key, format!("files:{raw:?}"), READ_TTL, || {
                    self.files(key, at).map(PullRequestReadResponse::Files)
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

    fn account(&self, key: &PullRequestKey) -> Result<String, ForgeError> {
        Ok(Self::account_of(key, self.viewer_id(key)?.as_deref()))
    }

    fn set_viewed(
        &self,
        key: &PullRequestKey,
        paths: &[String],
        viewed: bool,
    ) -> Result<(), ForgeError> {
        let account = self.account(key)?;
        let (revisions, _) = self.revisions(key)?;
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
        let latest = if comments.is_empty() {
            None
        } else {
            let paths: Vec<_> = comments
                .iter()
                .map(|comment| comment.path.as_str())
                .collect();
            match self.files_at(key, &paths) {
                Ok(latest) => Some(latest),
                Err(error) => return Outcome::Rejected(error.rejection()),
            }
        };
        let outcome = actions::submit_review(
            &self.pr(key),
            latest.as_ref(),
            verdict,
            head,
            body,
            comments,
        );
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
            &self.files_at(key, &[path])?.files,
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
        let paths: Vec<_> = comments
            .iter()
            .map(|comment| comment.path.as_str())
            .collect();
        crate::forge::anchors::reanchor(
            self.files_at(key, &paths)?.files,
            comments,
            |revision, path| self.file_text(key, revision, path),
        )
    }

    /// The pull request with its statuses and policy evaluations.
    fn watch_detail(&self, key: &PullRequestKey) -> Result<PullRequestWatchRead, ForgeError> {
        let pr = self.pr(key);
        let read = pr.pr()?;
        Ok(PullRequestWatchRead {
            state: reads::state(&read)
                .ok_or_else(|| error(ForgeErrorKind::Uncertain, "Azure DevOps state unreadable"))?,
            checks: pr.checks(&read)?,
            mergeability: Self::mergeability(&read),
            head_sha: reads::head(&read),
            base_branch: read["targetRefName"]
                .as_str()
                .map(reads::branch)
                .unwrap_or_default(),
            viewer: pr.viewer()?.as_ref().and_then(reads::login),
            author: reads::login(&read["createdBy"]),
        })
    }

    /// Every thread's written comments and the votes among them: Azure answers threads whole.
    fn activity(
        &self,
        key: &PullRequestKey,
        _: &mut Tails,
    ) -> Result<Option<Vec<PullRequestRemark>>, ForgeError> {
        let threads = self.pr(key).threads()?;
        Ok(Some(reads::remarks(&threads, &repository::url(key))))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An organization is listed once settings name it, or `AZURE_DEVOPS_ORG` names it for
    /// `AZURE_DEVOPS_EXT_PAT`; dev.azure.com itself never is, and the token alone names none.
    #[test]
    fn an_organization_is_listed_once_something_names_it() {
        let root = std::env::temp_dir().join(format!("tcode-azure-hosts-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let host = |environment: Vec<(String, String)>| {
            Azure::new(
                SettingsStore::new(root.clone()),
                environment,
                Arc::new(ViewedMarks::new(root.join("viewed-marks.json"))),
            )
        };
        let token = (api::ENV_TOKEN.to_owned(), "secret".to_owned());
        assert!(host(vec![token.clone()]).credential_status().is_empty());
        let quiet = host(Vec::new());
        quiet.configure(BTreeMap::from([(
            "dev.azure.com/fabrikam".to_owned(),
            HostSettings::new(HostKind::Azure),
        )]));
        assert!(quiet.credential_status()["dev.azure.com/fabrikam"].added);
        let named = host(vec![
            token,
            (api::ENV_ORGANIZATION.to_owned(), "contoso".to_owned()),
        ]);
        let status = named.credential_status();
        assert_eq!(status.keys().collect::<Vec<_>>(), ["dev.azure.com/contoso"]);
        assert!(!status["dev.azure.com/contoso"].added);
        let _ = std::fs::remove_dir_all(root);
    }

    /// A files cursor names the push it reads and the next file, in plain digits; anything else
    /// is not Tcode's.
    #[test]
    fn a_cursor_is_a_push_and_a_file() {
        assert_eq!(cursor("3:30"), Some((3, 30)));
        for foreign in [
            "3",
            "3:",
            ":30",
            "0x3:1",
            "3:-1",
            "3:1:2",
            "https://evil.test",
        ] {
            assert_eq!(cursor(foreign), None, "{foreign}");
        }
    }
    #[test]
    fn saved_verification_survives_a_fresh_host() {
        crate::forge::verification_tests::verification_survives_restart(
            HostKind::Azure,
            "dev.azure.com/fabrikam",
            "project/repo",
            |store, agent| {
                let viewed = Arc::new(ViewedMarks::new(store.data_file("viewed-marks.json")));
                let mut host = Azure::new(store, Vec::new(), viewed);
                Arc::get_mut(&mut Arc::get_mut(&mut host).unwrap().api)
                    .unwrap()
                    .agent = agent;
                host
            },
        );
    }
}
