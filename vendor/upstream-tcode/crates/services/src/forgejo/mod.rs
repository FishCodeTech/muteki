//! Forgejo and Gitea behind the pull request host boundary: one implementation for both, which
//! share an API. Where the two differ, the server's `/api/v1/version` decides, here and nowhere
//! else. Call blocking entries via HostCx::unblock.

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
use reads::{MAX_PAGES, Pull, text};
use serde_json::Value;
use std::{
    collections::{BTreeMap, HashMap},
    sync::{Arc, Mutex, RwLock},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tcode_core::{
    pull_request::{
        FORGEJO, GITEA, HostKind, HostTerms, PullRequestKey, PullRequestReviewDraftComment,
        PullRequestStackState,
    },
    pull_request_watch::{PullRequestRemark, PullRequestWatchRead},
    session::ReviewSide,
    settings::{CredentialSource, HostProblem, HostSettings, HostStatus},
};
use tcode_protocol::{
    PullRequestAction, PullRequestActionResult as Outcome, PullRequestActionState,
    PullRequestCapabilities, PullRequestComment, PullRequestConversation, PullRequestFileText,
    PullRequestFiles, PullRequestLabelCandidate, PullRequestLabelCandidates, PullRequestMedia,
    PullRequestMergeState, PullRequestPatch, PullRequestPermissions, PullRequestRead,
    PullRequestReadResponse, PullRequestReviewVerdict, PullRequestReviewerCandidate,
    PullRequestReviewerCandidates, PullRequestThreadReplies, PullRequestViewedFiles,
};

const READ_TTL: Duration = Duration::from_secs(60);
/// Text at a commit never changes; the bound is only on how long it holds memory.
const TEXT_TTL: Duration = Duration::from_secs(600);
const FILE_BYTES: usize = 1024 * 1024;
/// Issue comments whose reactions a conversation read asks for, one request each.
const REACTION_READS: usize = 100;
const ANONYMOUS: &str = "anonymous";

type Slot = (Instant, PullRequestReadResponse);

pub struct Forgejo {
    api: Arc<Api>,
    viewed: Arc<ViewedMarks>,
    /// The servers known by settings, a CLI login or the environment, with their kind.
    known: RwLock<BTreeMap<String, HostKind>>,
    reads: Mutex<HashMap<(PullRequestKey, String), Slot>>,
}

impl Forgejo {
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
        self.known.read().unwrap().keys().cloned().collect()
    }

    fn pull<'a>(&'a self, key: &'a PullRequestKey) -> Pull<'a> {
        Pull {
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

    fn account_of(&self, viewer: Option<&str>, key: &PullRequestKey) -> String {
        format!("{}:{}", key.host, viewer.unwrap_or(ANONYMOUS))
    }

    /// The whole diff with each path's head-side revision, or `None` past the read limit.
    fn diff(&self, key: &PullRequestKey) -> Result<Option<String>, ForgeError> {
        let pull = self.pull(key);
        let response = self.api.send(
            pull.authority(),
            Request {
                accept: "text/plain",
                ..Request::get(pull.path(&format!("pulls/{}.diff", key.number)), "Diff")
            },
        )?;
        Ok((!response.truncated).then(|| String::from_utf8_lossy(&response.body).into_owned()))
    }

    /// `page` is the page and the size the first page came back with, which a later one is
    /// full at.
    fn files(
        &self,
        key: &PullRequestKey,
        page: Option<(u32, usize)>,
    ) -> Result<PullRequestFiles, ForgeError> {
        let pull = self.pull(key);
        let pr = pull.pull()?;
        let head = text(&pr["head"], "sha").unwrap_or_default();
        let base = text(&pr, "merge_base")
            .or_else(|| text(&pr["base"], "sha"))
            .unwrap_or_default();
        let changed_files = pr["changed_files"].as_u64().unwrap_or(0);
        if page.is_none()
            && let Some(diff) = self.diff(key)?
        {
            return Ok(PullRequestFiles {
                base,
                head,
                files: reads::files(&diff)
                    .ok_or_else(|| error(ForgeErrorKind::Uncertain, "Forgejo diff unreadable"))?,
                next_cursor: None,
                complete: true,
                changed_files,
            });
        }
        // Past the read limit the files listing pages the changes, without their hunks.
        let page_number = page.map_or(1, |(page, _)| page);
        let rows: Vec<Value> = pull
            .get(
                &format!(
                    "pulls/{}/files?limit={}&page={page_number}",
                    key.number,
                    api::PAGE_LIMIT
                ),
                "Files",
            )?
            .as_array()
            .cloned()
            .unwrap_or_default();
        let size = page.map_or(rows.len(), |(_, size)| size);
        let listed = (page_number as u64 - 1) * size as u64 + rows.len() as u64;
        let next = (api::page_full(rows.len(), &size) && listed < changed_files)
            .then_some((page_number + 1, size));
        Ok(PullRequestFiles {
            base,
            head,
            files: rows
                .iter()
                .filter_map(|row| {
                    let status = row["status"].as_str().unwrap_or_default();
                    Some(tcode_protocol::PullRequestFile {
                        path: text(row, "filename")?,
                        previous_path: text(row, "previous_filename")
                            .filter(|path| !path.is_empty()),
                        kind: match status {
                            "added" => agent::FileChangeKind::Create,
                            "deleted" | "removed" => agent::FileChangeKind::Delete,
                            "renamed" => agent::FileChangeKind::Rename,
                            _ => agent::FileChangeKind::Modify,
                        },
                        additions: row["additions"].as_u64().unwrap_or(0),
                        deletions: row["deletions"].as_u64().unwrap_or(0),
                        patch: PullRequestPatch::Withheld,
                    })
                })
                .collect(),
            next_cursor: next.map(|(page, size)| format!("{page}:{size}")),
            complete: next.is_none() && listed >= changed_files,
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
        let pull = self.pull(key);
        let encoded: Vec<_> = path
            .split('/')
            .map(crate::github::pull_request_reads::percent_encode)
            .collect();
        let answer = self.api.send(
            pull.authority(),
            Request {
                accept: "application/octet-stream",
                limit: FILE_BYTES,
                ..Request::get(
                    pull.path(&format!("raw/{}?ref={revision}", encoded.join("/"))),
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
        let pull = self.pull(key);
        let pr = pull.pull()?;
        let repository = pull.repository()?;
        let viewer = pull.viewer()?;
        let capabilities = self.capabilities(key);
        let issue_comments = pull.issue_comments()?;
        let (reviews, reviews_complete) = self.api.list(
            pull.authority(),
            &pull.path(&format!("pulls/{}/reviews", key.number)),
            "Reviews",
            MAX_PAGES,
        )?;
        let mut line_comments = Vec::new();
        for review in &reviews {
            if review["comments_count"].as_u64().unwrap_or(0) == 0 {
                continue;
            }
            let Some(id) = review["id"].as_u64() else {
                continue;
            };
            let rows = pull.get(
                &format!("pulls/{}/reviews/{id}/comments", key.number),
                "ReviewComments",
            )?;
            line_comments.extend(rows.as_array().cloned().unwrap_or_default());
        }
        let author = text(&pr["user"], "login");
        let viewing = viewer.as_deref();
        let is_author = viewing.is_some() && viewing == author.as_deref();
        let permissions = &repository["permissions"];
        let writes = permissions["push"].as_bool() == Some(true)
            || permissions["admin"].as_bool() == Some(true);
        let archived = repository["archived"].as_bool() == Some(true);
        let signed_in = viewing.is_some() && !archived;
        // Reactions are one read per comment here; the newest comments are the ones read.
        let reacted = |path: String| -> Result<Vec<Value>, ForgeError> {
            Ok(pull
                .get(&path, "Reactions")?
                .as_array()
                .cloned()
                .unwrap_or_default())
        };
        let pr_reactions = reacted(format!("issues/{}/reactions", key.number))?;
        let reaction_ids: Vec<u64> = issue_comments
            .iter()
            .rev()
            .take(REACTION_READS)
            .filter_map(|comment| comment["id"].as_u64())
            .collect();
        let comment_reactions: HashMap<u64, Vec<Value>> = std::thread::scope(|scope| {
            let chunks: Vec<_> = reaction_ids
                .chunks(reaction_ids.len().div_ceil(4).max(1))
                .map(|ids| {
                    let reacted = &reacted;
                    scope.spawn(move || {
                        ids.iter()
                            .map(|id| {
                                Ok((*id, reacted(format!("issues/comments/{id}/reactions"))?))
                            })
                            .collect::<Result<Vec<_>, ForgeError>>()
                    })
                })
                .collect();
            let mut all = HashMap::new();
            for chunk in chunks {
                all.extend(chunk.join().unwrap_or_else(|_| Ok(Vec::new()))?);
            }
            Ok::<_, ForgeError>(all)
        })?;
        let comment = |raw: &Value| -> Option<PullRequestComment> {
            let id = raw["id"].as_u64()?;
            let own = viewing.is_some() && raw["user"]["login"].as_str() == viewing;
            Some(PullRequestComment {
                id: id.to_string(),
                author: reads::actor(&raw["user"]),
                body: text(raw, "body").unwrap_or_default(),
                created_at: text(raw, "created_at")?,
                edited_at: reads::edited_at(raw),
                url: text(raw, "html_url").filter(|url| !url.is_empty()),
                review_state: None,
                reactions: comment_reactions
                    .get(&id)
                    .map(|rows| reads::reactions(rows, viewing))
                    .unwrap_or_default(),
                viewer_can_update: signed_in && (own || writes),
                viewer_can_react: signed_in,
            })
        };
        let mut comments: Vec<_> = issue_comments.iter().filter_map(comment).collect();
        comments.extend(reviews.iter().filter_map(reads::review_comment));
        comments.sort_by(|left, right| left.created_at.cmp(&right.created_at));
        let description = PullRequestComment {
            id: reads::PULL_REQUEST.into(),
            author: reads::actor(&pr["user"]),
            body: text(&pr, "body").unwrap_or_default(),
            created_at: text(&pr, "created_at").unwrap_or_default(),
            edited_at: None,
            url: text(&pr, "html_url"),
            review_state: None,
            reactions: reads::reactions(&pr_reactions, viewing),
            viewer_can_update: signed_in && (is_author || writes),
            viewer_can_react: signed_in,
        };
        // Line comments are neither reacted to nor edited through the issue-comment endpoints,
        // and their reactions are not read.
        let line_comment = |raw: &Value| {
            comment(raw).map(|comment| PullRequestComment {
                reactions: Vec::new(),
                viewer_can_update: false,
                viewer_can_react: false,
                ..comment
            })
        };
        let threads = reads::threads(
            &line_comments,
            line_comment,
            signed_in && capabilities.reply,
            signed_in && capabilities.resolve && (is_author || writes),
        );
        Ok(PullRequestConversation {
            description,
            comments,
            threads,
            complete: reviews_complete,
            account: self.account_of(viewing, key),
            permissions: PullRequestPermissions {
                update: signed_in && (is_author || writes),
                // The servers refuse an approval or a change request from the author.
                verdicts: match (signed_in, is_author) {
                    (false, _) => Vec::new(),
                    (true, true) => vec![PullRequestReviewVerdict::Comment],
                    (true, false) => vec![
                        PullRequestReviewVerdict::Comment,
                        PullRequestReviewVerdict::Approve,
                        PullRequestReviewVerdict::RequestChanges,
                    ],
                },
                label: signed_in && writes,
                request_reviewers: signed_in && (is_author || writes),
            },
            labels: reads::labels(&pr),
            reviewers: reads::reviewer_states(&pr, &reviews),
            capabilities,
        })
    }

    fn viewed_files(&self, key: &PullRequestKey) -> Result<PullRequestViewedFiles, ForgeError> {
        let viewer = self.pull(key).viewer()?;
        let Some(diff) = self.diff(key)? else {
            return Ok(PullRequestViewedFiles {
                files: Vec::new(),
                complete: false,
            });
        };
        Ok(PullRequestViewedFiles {
            files: self.viewed.states(
                &self.account_of(viewer.as_deref(), key),
                key,
                &crate::forge::revisions(&diff),
            ),
            complete: true,
        })
    }

    fn label_candidates(
        &self,
        key: &PullRequestKey,
    ) -> Result<PullRequestLabelCandidates, ForgeError> {
        let pull = self.pull(key);
        let pr = pull.pull()?;
        let (rows, complete) =
            self.api
                .list(pull.authority(), &pull.path("labels"), "Labels", 1)?;
        let applied = reads::labels(&pr);
        let mut labels: Vec<_> = applied
            .iter()
            .map(|label| PullRequestLabelCandidate {
                id: label.id.clone(),
                name: label.name.clone(),
                color: label.color.clone(),
                description: label.description.clone(),
                applied: true,
            })
            .collect();
        for row in &rows {
            let Some(id) = row["id"].as_u64().map(|id| id.to_string()) else {
                continue;
            };
            if labels.iter().any(|label| label.id == id) {
                continue;
            }
            labels.push(PullRequestLabelCandidate {
                id,
                name: text(row, "name").unwrap_or_default(),
                color: text(row, "color").map(|color| color.trim_start_matches('#').to_owned()),
                description: text(row, "description").filter(|text| !text.is_empty()),
                applied: false,
            });
        }
        Ok(PullRequestLabelCandidates { labels, complete })
    }

    fn reviewer_candidates(
        &self,
        key: &PullRequestKey,
    ) -> Result<PullRequestReviewerCandidates, ForgeError> {
        let pull = self.pull(key);
        let pr = pull.pull()?;
        let author = text(&pr["user"], "login");
        let rows = pull.get("assignees", "Assignees")?;
        let mut reviewers: Vec<_> = pr["requested_reviewers"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|user| {
                Some(PullRequestReviewerCandidate {
                    reviewer: reads::user_reviewer(text(user, "login")?),
                    name: text(user, "full_name").filter(|name| !name.is_empty()),
                    avatar_url: text(user, "avatar_url"),
                    requested: true,
                })
            })
            .collect();
        for user in rows.as_array().into_iter().flatten() {
            let Some(login) = text(user, "login") else {
                continue;
            };
            if Some(&login) == author.as_ref()
                || reviewers.iter().any(|known| known.reviewer.login == login)
            {
                continue;
            }
            reviewers.push(PullRequestReviewerCandidate {
                reviewer: reads::user_reviewer(login),
                name: text(user, "full_name").filter(|name| !name.is_empty()),
                avatar_url: text(user, "avatar_url"),
                requested: false,
            });
        }
        Ok(PullRequestReviewerCandidates {
            reviewers,
            complete: true,
        })
    }

    fn action_state(&self, key: &PullRequestKey) -> Result<PullRequestActionState, ForgeError> {
        let pull = self.pull(key);
        let pr = pull.pull()?;
        let repository = pull.repository()?;
        let viewer = pull.viewer()?;
        let head = text(&pr["head"], "sha").unwrap_or_default();
        let checks = pull.checks(&head)?;
        let base = text(&pr["base"], "sha").unwrap_or_default();
        let behind_by = if text(&pr, "merge_base").as_deref() == Some(base.as_str()) {
            Some(0)
        } else {
            pull.get(&format!("compare/{head}...{base}"), "Compare")
                .ok()
                .and_then(|compare| compare["total_commits"].as_u64())
        };
        let permissions = &repository["permissions"];
        let writes = viewer.is_some()
            && repository["archived"].as_bool() != Some(true)
            && (permissions["push"].as_bool() == Some(true)
                || permissions["admin"].as_bool() == Some(true));
        let is_author = viewer.is_some() && viewer == text(&pr["user"], "login");
        let failing: Vec<_> = checks
            .iter()
            .filter(|check| check.status.failed())
            .map(|check| check.name.clone())
            .collect();
        let pending = checks
            .iter()
            .filter(|check| check.status == tcode_core::pull_request_watch::CheckStatus::Pending)
            .count() as u32;
        let merge_state = if pr["draft"].as_bool() == Some(true) {
            PullRequestMergeState::Draft
        } else {
            match reads::mergeability(&pr) {
                tcode_core::pull_request::Mergeability::Clean
                    if !failing.is_empty() || pending > 0 =>
                {
                    PullRequestMergeState::Unstable
                }
                tcode_core::pull_request::Mergeability::Clean => PullRequestMergeState::Clean,
                _ => PullRequestMergeState::Unknown,
            }
        };
        let allowed = |field: &str| repository[field].as_bool() == Some(true);
        let merge_methods = [
            (
                "allow_merge_commits",
                tcode_core::pull_request::PullRequestMergeMethod::Merge,
            ),
            (
                "allow_squash_merge",
                tcode_core::pull_request::PullRequestMergeMethod::Squash,
            ),
            (
                "allow_rebase",
                tcode_core::pull_request::PullRequestMergeMethod::Rebase,
            ),
        ]
        .into_iter()
        .filter(|(field, _)| allowed(field))
        .map(|(_, method)| method)
        .collect();
        let capabilities = self.capabilities(key);
        Ok(PullRequestActionState {
            head,
            merge_state,
            behind_by,
            merge_queue: false,
            merge_methods,
            auto_merge_allowed: false,
            // The servers never say whether a merge is scheduled.
            auto_merge: None,
            queued: false,
            queue_position: None,
            failing_checks: failing,
            pending_checks: pending,
            can_update: is_author || writes,
            can_update_branch: writes,
            can_merge: writes,
            capabilities,
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
        let (server, mount) = key.host.split_once('/').unwrap_or((&key.host, ""));
        let path = parsed.path();
        let below = path
            .strip_prefix(&format!("/{mount}"))
            .filter(|_| !mount.is_empty())
            .unwrap_or(path);
        // Uploads and avatars on the pull request's own server, which a private repository
        // serves only with its token; anything else the client draws by its URL.
        let own = parsed.scheme() == "https"
            && host_port.eq_ignore_ascii_case(server)
            && (below.starts_with("/attachments/")
                || below.starts_with("/avatars/")
                || below.starts_with(&format!("/{}/attachments/", key.repository)));
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
            .any(|comment| {
                comment.body.contains(url)
                    || comment
                        .author
                        .as_ref()
                        .and_then(|author| author.avatar_url.as_deref())
                        == Some(url)
            });
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

impl Forge for Forgejo {
    fn terms(&self, key: &PullRequestKey) -> &'static HostTerms {
        match self
            .known
            .read()
            .unwrap()
            .get(&key.host)
            .copied()
            .or_else(|| HostKind::detect(&key.host))
        {
            Some(HostKind::Gitea) => &GITEA,
            _ => &FORGEJO,
        }
    }

    fn capabilities(&self, key: &PullRequestKey) -> PullRequestCapabilities {
        reads::capabilities(self.api.server(&key.host))
    }

    fn configure(&self, hosts: BTreeMap<String, HostSettings>) {
        let own: BTreeMap<_, _> = hosts
            .into_iter()
            .filter(|(_, choice)| choice.kind != HostKind::Github)
            .collect();
        {
            let mut known = self.known.write().unwrap();
            known.retain(|host, _| !own.contains_key(host));
            known.extend(own.iter().map(|(host, choice)| (host.clone(), choice.kind)));
        }
        self.api.configure(own);
    }

    fn credential_status(&self) -> BTreeMap<String, HostStatus> {
        let configured = self.api.configured();
        let tea = self.api.tea_program().is_some();
        // Each host's kind, and whether only settings name it.
        let mut hosts: BTreeMap<String, (HostKind, bool)> = configured
            .iter()
            .map(|(host, choice)| (host.clone(), (choice.kind, true)))
            .collect();
        let detected = self
            .api
            .tea_logins()
            .into_iter()
            .chain(self.api.environment_host());
        for host in detected {
            // tea is Gitea's CLI; a server named for Forgejo, or Codeberg, says otherwise.
            let kind = HostKind::detect(&host)
                .filter(|kind| *kind != HostKind::Github)
                .unwrap_or(HostKind::Gitea);
            hosts
                .entry(host)
                .and_modify(|(_, added)| *added = false)
                .or_insert((kind, false));
        }
        {
            let mut known = self.known.write().unwrap();
            for (host, (kind, _)) in &hosts {
                known.entry(host.clone()).or_insert(*kind);
            }
        }
        hosts
            .into_iter()
            .map(|(host, (kind, added))| {
                let enabled = configured.get(&host).is_none_or(|choice| choice.enabled);
                let source = self
                    .api
                    .credential(&host)
                    .ok()
                    .flatten()
                    .map(|credential| credential.source);
                let problem = (source.is_none() && enabled).then(|| {
                    if tea {
                        HostProblem::NotSignedIn {
                            tool: "tea".into(),
                            command: Some(format!("tea login add --url https://{host}")),
                        }
                    } else {
                        HostProblem::NoCredential {
                            tools_missing: vec!["tea".into()],
                        }
                    }
                });
                (
                    host.clone(),
                    HostStatus {
                        kind,
                        added,
                        token_set: self.api.token_saved(kind, &host),
                        source,
                        accounts: Vec::new(),
                        env_overrides_account: false,
                        order: vec![
                            CredentialSource::Saved,
                            CredentialSource::Env {
                                name: api::ENV_TOKEN.into(),
                            },
                            CredentialSource::Cli { tool: "tea".into() },
                        ],
                        problem,
                        token_url: Some(format!("https://{host}/user/settings/applications")),
                        access_token_url: None,
                        token_scopes: vec![
                            "write:repository".into(),
                            "write:issue".into(),
                            "read:user".into(),
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
        let pull = self.pull(key);
        let pr = pull.pull()?;
        let head = text(&pr["head"], "sha").unwrap_or_default();
        let checks = reads::checks_state(&pull.checks(&head)?);
        Ok(Summary {
            snapshot: reads::snapshot(&pr, checks, reads::mergeability(&pr), Self::now())
                .ok_or_else(|| {
                    error(ForgeErrorKind::Uncertain, "Forgejo pull request unreadable")
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
                            .split_once(':')
                            .and_then(|(page, size)| {
                                Some((page.parse::<u32>().ok()?, size.parse::<usize>().ok()?))
                            })
                            .filter(|(page, size)| *page > 1 && *size > 0)
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
            // Read fresh: a merge or a branch update decides on it.
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
        Ok(self.account_of(self.pull(key).viewer()?.as_deref(), key))
    }

    fn set_viewed(
        &self,
        key: &PullRequestKey,
        paths: &[String],
        viewed: bool,
    ) -> Result<(), ForgeError> {
        let account = self.account(key)?;
        let diff = self
            .diff(key)?
            .ok_or_else(|| error(ForgeErrorKind::TooLarge, "Forgejo diff too large"))?;
        self.viewed
            .set(
                &account,
                key,
                &crate::forge::revisions(&diff),
                paths,
                viewed,
            )
            .map_err(|_| error(ForgeErrorKind::Uncertain, "viewed marks not saved"))
    }

    fn act(&self, key: &PullRequestKey, action: &PullRequestAction) -> Outcome {
        self.drop_reads(key);
        let outcome = actions::act(&self.pull(key), action);
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
        let outcome = actions::submit_review(&self.pull(key), verdict, head, body, comments);
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
        let pull = self.pull(key);
        let pr = pull.pull()?;
        let head = text(&pr["head"], "sha");
        let checks = match &head {
            Some(head) => pull.checks(head)?,
            None => Vec::new(),
        };
        Ok(PullRequestWatchRead {
            state: reads::state(&pr)
                .ok_or_else(|| error(ForgeErrorKind::Uncertain, "Forgejo state unreadable"))?,
            head_sha: head,
            base_branch: text(&pr["base"], "ref").unwrap_or_default(),
            checks,
            mergeability: reads::mergeability(&pr),
            viewer: pull.viewer()?,
            author: text(&pr["user"], "login"),
        })
    }

    fn activity(
        &self,
        key: &PullRequestKey,
        _: &mut Tails,
    ) -> Result<Option<Vec<PullRequestRemark>>, ForgeError> {
        let pull = self.pull(key);
        let comments = pull.issue_comments()?;
        let (reviews, reviews_complete) = self.api.list(
            pull.authority(),
            &pull.path(&format!("pulls/{}/reviews", key.number)),
            "Reviews",
            MAX_PAGES,
        )?;
        let mut remarks: Vec<_> = comments
            .iter()
            .filter_map(|raw| reads::remark(raw, None))
            .collect();
        remarks.extend(reviews.iter().filter_map(reads::review_remark));
        for review in &reviews {
            let (Some(id), true) = (
                review["id"].as_u64(),
                review["comments_count"].as_u64().unwrap_or(0) > 0,
            ) else {
                continue;
            };
            let rows = pull.get(
                &format!("pulls/{}/reviews/{id}/comments", key.number),
                "ReviewComments",
            )?;
            remarks.extend(
                rows.as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|raw| reads::remark(raw, raw["path"].as_str())),
            );
        }
        Ok(reviews_complete.then_some(remarks))
    }
}
