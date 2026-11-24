//! What a client reads of a linked pull request: its files, file text at a revision, the
//! conversation, the account's viewed files, and media the conversation points at.

use super::{
    GitHubApi, GitHubError, RequestOptions, RestRequest,
    api::{Authentication, DEADLINE_CAP},
    graphql::{self, AliasItem, Document, Variables},
    media,
    read_cache::{Fresh, ReadCache, ReadKey},
    repository::Repository,
};
use agent::FileChangeKind;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
    time::Duration,
};
use tcode_core::{pull_request::PullRequestKey, session::ReviewSide};
use tcode_protocol::{
    PullRequestActor, PullRequestCapabilities, PullRequestComment, PullRequestConversation,
    PullRequestFile, PullRequestFileText, PullRequestLabel, PullRequestLabelCandidate,
    PullRequestLabelCandidates, PullRequestMedia, PullRequestPatch, PullRequestPermissions,
    PullRequestReaction, PullRequestReactionContent, PullRequestReviewAnchor,
    PullRequestReviewState, PullRequestReviewThread, PullRequestReviewVerdict, PullRequestReviewer,
    PullRequestReviewerCandidate, PullRequestReviewerCandidates, PullRequestReviewerKind,
    PullRequestReviewerState, PullRequestThreadReplies, PullRequestViewedFiles,
    PullRequestViewedState,
};

pub(super) const READ_TTL: Duration = Duration::from_secs(60);
/// A merged pull request changes no more, short of an edited comment.
const MERGED_TTL: Duration = Duration::from_secs(600);
const VIEWED_TTL: Duration = Duration::from_secs(15);
/// Text at a commit never changes; the bound is only on how long it holds memory.
const TEXT_TTL: Duration = Duration::from_secs(600);
const DIFF_BYTES: usize = 8 * 1024 * 1024;
const FILE_BYTES: usize = 1024 * 1024;
const FILES_PER_PAGE: usize = 100;
/// Past this many requests a list is reported incomplete rather than read on.
pub(super) const MAX_PAGES: usize = 10;
const VIEWED_PAGES: usize = 5;
const EMPTY_BLOB: &str = "e69de29bb2d1d6434b8b29ae775ad8c2e48c5391";
/// Pull requests whose node id is remembered; an id never changes, so this bounds memory only.
const NODE_IDS: usize = 128;

const ACTOR: &str = "author { login avatarUrl(size: 64) }";
const REACTIONS: &str = "reactionGroups { content viewerHasReacted reactors { totalCount } }";
const VIEWER: &str = "viewerCanUpdate viewerCanReact";

#[derive(Debug, Clone)]
pub(super) struct Revisions {
    base: String,
    pub(super) head: String,
    changed_files: u64,
    /// The pull request's GraphQL id, which the viewed mutation names.
    node_id: String,
}

/// The changed files between two commits: the whole diff, or a page of GitHub's files listing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Files {
    pub base: String,
    pub head: String,
    pub files: Vec<PullRequestFile>,
    /// GitHub refused or cut the whole diff and pages its changed files instead.
    pub next_page: Option<u32>,
    /// Every changed file has been listed once the pages before this one were read too.
    pub complete: bool,
    /// GitHub's count of changed files, which a listing may fall short of.
    pub changed_files: u64,
}

/// The one owner of what the host has read of pull requests; the writes in
/// [`super::pull_request_actions`] drop what they change through it.
pub struct PullRequestReads {
    pub(super) api: Arc<GitHubApi>,
    pub(super) revisions: ReadCache<Revisions>,
    files: ReadCache<Files>,
    texts: ReadCache<PullRequestFileText>,
    pub(super) conversations: ReadCache<PullRequestConversation>,
    pub(super) replies: ReadCache<PullRequestThreadReplies>,
    viewed: ReadCache<PullRequestViewedFiles>,
    pub(super) labels: ReadCache<PullRequestLabelCandidates>,
    pub(super) reviewers: ReadCache<PullRequestReviewerCandidates>,
    pub(super) action_states: ReadCache<tcode_protocol::PullRequestActionState>,
    /// Filled only by a successful read, oldest first.
    node_ids: Mutex<Vec<(PullRequestKey, String)>>,
}

pub(super) struct Reader<'a> {
    pub(super) api: &'a GitHubApi,
    pub(super) key: &'a PullRequestKey,
    pub(super) repository: Repository,
    account: String,
    pub(super) options: RequestOptions,
}

impl Reader<'_> {
    pub(super) fn read_key(&self, read: impl Into<String>) -> ReadKey {
        ReadKey {
            pull_request: self.key.clone(),
            account: self.account.clone(),
            read: read.into(),
        }
    }

    fn account_name(&self) -> String {
        super::digest(&self.account)[..16].to_owned()
    }

    pub(super) fn rest_path(&self, rest: &str) -> String {
        format!(
            "/repos/{}/{}/{rest}",
            self.repository.owner, self.repository.name
        )
    }

    fn long_read(&self, operation: &'static str, body_limit: usize) -> RequestOptions {
        RequestOptions {
            operation,
            timeout: DEADLINE_CAP,
            body_limit,
            ..self.options.clone()
        }
    }

    pub(super) fn query(
        &self,
        operation: &'static str,
        query: String,
        extra: impl IntoIterator<Item = (&'static str, Value)>,
    ) -> Result<Value, GitHubError> {
        let mut variables = BTreeMap::from([
            ("owner".to_owned(), json!(self.repository.owner)),
            ("name".to_owned(), json!(self.repository.name)),
            ("number".to_owned(), json!(self.key.number)),
        ]);
        variables.extend(
            extra
                .into_iter()
                .map(|(name, value)| (name.to_owned(), value)),
        );
        let options = RequestOptions {
            operation,
            ..self.options.clone()
        };
        self.api
            .graphql(&self.key.host, &Document { query, variables }, &options)?
            .json()
    }

    fn revisions(&self) -> Result<(Revisions, Duration), GitHubError> {
        let raw: Value = self
            .api
            .rest(
                &self.key.host,
                RestRequest::get(&self.rest_path(&format!("pulls/{}", self.key.number))),
                &RequestOptions {
                    operation: "PullRequestRevisions",
                    ..self.options.clone()
                },
            )?
            .json()?;
        let sha = |side: &str| {
            raw[side]["sha"]
                .as_str()
                .filter(|sha| is_revision(sha))
                .map(str::to_owned)
                .ok_or(GitHubError::InvalidResponse)
        };
        let revisions = Revisions {
            base: sha("base")?,
            head: sha("head")?,
            changed_files: raw["changed_files"].as_u64().unwrap_or(0),
            node_id: raw["node_id"]
                .as_str()
                .ok_or(GitHubError::InvalidResponse)?
                .to_owned(),
        };
        let ttl = if raw["merged_at"].is_string() {
            MERGED_TTL
        } else {
            READ_TTL
        };
        Ok((revisions, ttl))
    }

    /// GitHub refuses a whole diff past its own limits, and this read cuts one past 8 MiB; both
    /// read the files a page at a time instead. A failing page reports the original refusal.
    fn whole_diff(&self, revisions: &Revisions) -> Result<Files, GitHubError> {
        let path = self.rest_path(&format!("pulls/{}", self.key.number));
        let answer = self.api.rest(
            &self.key.host,
            RestRequest {
                accept: Some("application/vnd.github.diff"),
                ..RestRequest::get(&path)
            },
            &self.long_read("PullRequestDiff", DIFF_BYTES),
        );
        match answer {
            Ok(response) if !response.truncated => Ok(Files {
                base: revisions.base.clone(),
                head: revisions.head.clone(),
                files: diff_files(&String::from_utf8_lossy(&response.body))
                    .ok_or(GitHubError::InvalidResponse)?,
                next_page: None,
                complete: true,
                changed_files: revisions.changed_files,
            }),
            Ok(_) => self.files_page(revisions, 1),
            Err(refusal @ GitHubError::Response { .. }) => {
                self.files_page(revisions, 1).map_err(|_| refusal)
            }
            Err(error) => Err(error),
        }
    }

    fn files_page(&self, revisions: &Revisions, page: u32) -> Result<Files, GitHubError> {
        let path = self.rest_path(&format!(
            "pulls/{}/files?per_page={FILES_PER_PAGE}&page={page}",
            self.key.number
        ));
        let rows: Vec<Value> = self
            .api
            .rest(
                &self.key.host,
                RestRequest::get(&path),
                &self.long_read("PullRequestFiles", DIFF_BYTES),
            )?
            .json()?;
        let next_page = (rows.len() >= FILES_PER_PAGE).then_some(page + 1);
        let listed = (page as u64 - 1) * FILES_PER_PAGE as u64 + rows.len() as u64;
        Ok(Files {
            base: revisions.base.clone(),
            head: revisions.head.clone(),
            files: rows.iter().filter_map(listed_file).collect(),
            next_page,
            complete: next_page.is_none() && listed >= revisions.changed_files,
            changed_files: revisions.changed_files,
        })
    }

    fn file_text(&self, revision: &str, path: &str) -> Result<PullRequestFileText, GitHubError> {
        let encoded = path
            .split('/')
            .map(percent_encode)
            .collect::<Vec<_>>()
            .join("/");
        let answer = self.api.rest(
            &self.key.host,
            RestRequest {
                accept: Some("application/vnd.github.raw"),
                ..RestRequest::get(&self.rest_path(&format!("contents/{encoded}?ref={revision}")))
            },
            &self.long_read("PullRequestFileText", FILE_BYTES),
        );
        Ok(match answer {
            Ok(response) if response.truncated => PullRequestFileText::Oversized,
            Ok(response) if response.body.contains(&0) => PullRequestFileText::Binary,
            Ok(response) => String::from_utf8(response.body)
                .map_or(PullRequestFileText::Binary, PullRequestFileText::Text),
            Err(GitHubError::NotFound) => PullRequestFileText::Missing,
            Err(error) => return Err(error),
        })
    }

    fn conversation(&self) -> Result<(PullRequestConversation, Duration), GitHubError> {
        let comment = format!("id body createdAt lastEditedAt url {ACTOR} {REACTIONS} {VIEWER}");
        let query = format!(
            "query PullRequestConversation($owner: String!, $name: String!, $number: Int!, $head: Boolean!, $withComments: Boolean!, $commentsAfter: String, $withReviews: Boolean!, $reviewsAfter: String) {{ repository(owner: $owner, name: $name) {{ viewerPermission @include(if: $head) pullRequest(number: $number) {{ ... on PullRequest @include(if: $head) {{ {comment} mergedAt viewerDidAuthor labels(first: 100) {{ nodes {{ name color description }} }} reviewRequests(first: 100) {{ nodes {{ requestedReviewer {{ ... on User {{ login avatarUrl(size: 64) }} ... on Bot {{ login avatarUrl(size: 64) }} ... on Team {{ slug }} }} }} }} latestReviews(first: 100) {{ nodes {{ state {ACTOR} }} }} }} comments(first: 100, after: $commentsAfter) @include(if: $withComments) {{ pageInfo {{ hasNextPage endCursor }} nodes {{ {comment} }} }} reviews(first: 100, after: $reviewsAfter) @include(if: $withReviews) {{ pageInfo {{ hasNextPage endCursor }} nodes {{ {comment} state submittedAt }} }} }} }} }}"
        );
        let mut description = None;
        let mut merged = false;
        let mut head = Value::Null;
        let mut comments = Vec::new();
        let (mut more_comments, mut more_reviews) = (true, true);
        let (mut comments_after, mut reviews_after) = (Value::Null, Value::Null);
        for page in 0..MAX_PAGES {
            let response = self.query(
                "PullRequestConversation",
                query.clone(),
                [
                    ("head", json!(page == 0)),
                    ("withComments", json!(more_comments)),
                    ("commentsAfter", comments_after.clone()),
                    ("withReviews", json!(more_reviews)),
                    ("reviewsAfter", reviews_after.clone()),
                ],
            )?;
            let pr = &response["data"]["repository"]["pullRequest"];
            if pr.is_null() {
                return Err(GitHubError::NotFound);
            }
            if page == 0 {
                merged = pr["mergedAt"].is_string();
                description = Some(comment_from(pr, None).ok_or(GitHubError::InvalidResponse)?);
                head = response["data"]["repository"].clone();
            }
            if more_comments {
                comments.extend(nodes(&pr["comments"]).filter_map(|raw| comment_from(raw, None)));
                comments_after = next_cursor(&pr["comments"]).map_or(Value::Null, Value::String);
                more_comments = !comments_after.is_null();
            }
            if more_reviews {
                comments.extend(nodes(&pr["reviews"]).filter_map(review_from));
                reviews_after = next_cursor(&pr["reviews"]).map_or(Value::Null, Value::String);
                more_reviews = !reviews_after.is_null();
            }
            if !more_comments && !more_reviews {
                break;
            }
        }
        let mut complete = !more_comments && !more_reviews;
        comments.sort_by(|left, right| left.created_at.cmp(&right.created_at));

        let thread_query = format!(
            "query PullRequestReviewThreads($owner: String!, $name: String!, $number: Int!, $cursor: String) {{ repository(owner: $owner, name: $name) {{ pullRequest(number: $number) {{ reviewThreads(first: 100, after: $cursor) {{ pageInfo {{ hasNextPage endCursor }} nodes {{ id isResolved isOutdated path line startLine originalLine originalStartLine diffSide startDiffSide viewerCanReply viewerCanResolve viewerCanUnresolve comments(first: 10) {{ totalCount pageInfo {{ hasNextPage endCursor }} nodes {{ {comment} diffHunk commit {{ oid }} originalCommit {{ oid }} }} }} }} }} }} }} }}"
        );
        let mut threads = Vec::new();
        let mut cursor = Value::Null;
        for page in 1..=MAX_PAGES {
            let response = self.query(
                "PullRequestReviewThreads",
                thread_query.clone(),
                [("cursor", cursor.clone())],
            )?;
            let connection = &response["data"]["repository"]["pullRequest"]["reviewThreads"];
            threads.extend(nodes(connection).filter_map(thread_from));
            match next_cursor(connection) {
                Some(next) if page < MAX_PAGES => cursor = Value::String(next),
                Some(_) => complete = false,
                None => break,
            }
        }
        let description = description.ok_or(GitHubError::InvalidResponse)?;
        let pr = &head["pullRequest"];
        // Triage may label; write may also ask for reviews (study: provider permission map).
        let role = head["viewerPermission"].as_str().unwrap_or_default();
        let writes = matches!(role, "ADMIN" | "MAINTAIN" | "WRITE");
        let permissions = PullRequestPermissions {
            update: description.viewer_can_update,
            // GitHub refuses an approval or a change request from the author.
            verdicts: if pr["viewerDidAuthor"].as_bool() == Some(true) {
                vec![PullRequestReviewVerdict::Comment]
            } else {
                vec![
                    PullRequestReviewVerdict::Comment,
                    PullRequestReviewVerdict::Approve,
                    PullRequestReviewVerdict::RequestChanges,
                ]
            },
            label: writes || role == "TRIAGE",
            request_reviewers: writes,
        };
        let labels = nodes(&pr["labels"])
            .filter_map(|label| {
                let name = text(label, "name")?;
                Some(PullRequestLabel {
                    id: name.clone(),
                    name,
                    color: text(label, "color"),
                    description: text(label, "description"),
                })
            })
            .collect();
        Ok((
            PullRequestConversation {
                description,
                comments,
                threads,
                complete,
                account: self.account_name(),
                permissions,
                labels,
                reviewers: reviewer_states(pr),
                capabilities: PullRequestCapabilities::ALL,
            },
            if merged { MERGED_TTL } else { READ_TTL },
        ))
    }

    fn thread_replies(
        &self,
        thread: &str,
        after: &str,
    ) -> Result<PullRequestThreadReplies, GitHubError> {
        let query = format!(
            "query PullRequestThreadReplies($owner: String!, $name: String!, $number: Int!, $thread: ID!, $cursor: String) {{ repository(owner: $owner, name: $name) {{ pullRequest(number: $number) {{ id }} }} node(id: $thread) {{ ... on PullRequestReviewThread {{ pullRequest {{ id }} comments(first: 100, after: $cursor) {{ pageInfo {{ hasNextPage endCursor }} nodes {{ id body createdAt lastEditedAt url {ACTOR} {REACTIONS} {VIEWER} }} }} }} }} }}"
        );
        let response = self.query(
            "PullRequestThreadReplies",
            query,
            [("thread", json!(thread)), ("cursor", json!(after))],
        )?;
        let data = &response["data"];
        // A thread id names any thread on GitHub; only this pull request's are read here.
        let owner = data["repository"]["pullRequest"]["id"].as_str();
        if owner.is_none() || data["node"]["pullRequest"]["id"].as_str() != owner {
            return Err(GitHubError::NotFound);
        }
        let connection = &data["node"]["comments"];
        Ok(PullRequestThreadReplies {
            comments: nodes(connection)
                .filter_map(|raw| comment_from(raw, None))
                .collect(),
            after: next_cursor(connection),
        })
    }

    fn viewed_files(&self) -> Result<PullRequestViewedFiles, GitHubError> {
        let query = "query PullRequestViewedFiles($owner: String!, $name: String!, $number: Int!, $after: String) { repository(owner: $owner, name: $name) { pullRequest(number: $number) { files(first: 100, after: $after) { pageInfo { hasNextPage endCursor } nodes { path viewerViewedState } } } } }";
        let read = graphql::pages(
            None,
            Some(VIEWED_PAGES),
            |after, _| {
                self.query(
                    "PullRequestViewedFiles",
                    query.to_owned(),
                    [("after", json!(after))],
                )
            },
            |page| next_cursor(&page["data"]["repository"]["pullRequest"]["files"]),
            |_| false,
        )?;
        if read
            .pages
            .first()
            .is_none_or(|page| page["data"]["repository"]["pullRequest"].is_null())
        {
            return Err(GitHubError::NotFound);
        }
        let files = read
            .pages
            .iter()
            .flat_map(|page| nodes(&page["data"]["repository"]["pullRequest"]["files"]))
            .filter_map(|node| {
                let state = match node["viewerViewedState"].as_str()? {
                    "VIEWED" => PullRequestViewedState::Viewed,
                    "DISMISSED" => PullRequestViewedState::Dismissed,
                    _ => PullRequestViewedState::Unviewed,
                };
                Some((node["path"].as_str()?.to_owned(), state))
            })
            .collect();
        Ok(PullRequestViewedFiles {
            files,
            complete: !read.truncated,
        })
    }

    fn label_candidates(&self) -> Result<PullRequestLabelCandidates, GitHubError> {
        let response = self.query(
            "PullRequestLabelCandidates",
            "query PullRequestLabelCandidates($owner: String!, $name: String!, $number: Int!) { repository(owner: $owner, name: $name) { labels(first: 100, orderBy: { field: NAME, direction: ASC }) { pageInfo { hasNextPage } nodes { name color description } } pullRequest(number: $number) { labels(first: 100) { nodes { name } } } } }".to_owned(),
            [],
        )?;
        let repository = &response["data"]["repository"];
        if repository["pullRequest"].is_null() {
            return Err(GitHubError::NotFound);
        }
        let applied: Vec<_> = nodes(&repository["pullRequest"]["labels"])
            .filter_map(|label| text(label, "name"))
            .collect();
        let listed: Vec<_> = nodes(&repository["labels"])
            .filter_map(|label| {
                let name = text(label, "name")?;
                Some(PullRequestLabelCandidate {
                    id: name.clone(),
                    applied: applied.contains(&name),
                    color: text(label, "color"),
                    description: text(label, "description"),
                    name,
                })
            })
            .collect();
        // A label that cannot be seen cannot be taken off, so one the repository no longer lists
        // leads anyway.
        let missing = applied
            .iter()
            .filter(|name| listed.iter().all(|label| &label.name != *name))
            .map(|name| PullRequestLabelCandidate {
                id: name.clone(),
                name: name.clone(),
                color: None,
                description: None,
                applied: true,
            });
        Ok(PullRequestLabelCandidates {
            labels: missing.chain(listed.iter().cloned()).collect(),
            complete: repository["labels"]["pageInfo"]["hasNextPage"].as_bool() != Some(true),
        })
    }

    /// `assignableUsers` is the list GitHub's own picker offers; `collaborators` is refused to
    /// anyone without push access.
    fn reviewer_candidates(&self) -> Result<PullRequestReviewerCandidates, GitHubError> {
        let response = self.query(
            "PullRequestReviewerCandidates",
            "query PullRequestReviewerCandidates($owner: String!, $name: String!, $number: Int!) { repository(owner: $owner, name: $name) { assignableUsers(first: 100) { pageInfo { hasNextPage } nodes { login name avatarUrl } } pullRequest(number: $number) { author { login } reviewRequests(first: 100) { nodes { requestedReviewer { ... on User { login name avatarUrl } ... on Team { slug name avatarUrl } ... on Bot { login avatarUrl } } } } } } }".to_owned(),
            [],
        )?;
        let repository = &response["data"]["repository"];
        let pull_request = &repository["pullRequest"];
        if pull_request.is_null() {
            return Err(GitHubError::NotFound);
        }
        let author = text(&pull_request["author"], "login");
        let mut reviewers: Vec<PullRequestReviewerCandidate> = Vec::new();
        let requests = nodes(&pull_request["reviewRequests"]).map(|node| (node, true));
        let assignable = nodes(&repository["assignableUsers"]).map(|node| (node, false));
        for (node, requested) in requests.chain(assignable) {
            let raw = if requested {
                &node["requestedReviewer"]
            } else {
                node
            };
            let reviewer = match (text(raw, "slug"), text(raw, "login")) {
                (Some(slug), _) => reviewer(slug, PullRequestReviewerKind::Team),
                (None, Some(login)) => reviewer(login, PullRequestReviewerKind::User),
                (None, None) => continue,
            };
            if (!requested && Some(&reviewer.login) == author.as_ref())
                || reviewers.iter().any(|known| known.reviewer == reviewer)
            {
                continue;
            }
            reviewers.push(PullRequestReviewerCandidate {
                reviewer,
                name: text(raw, "name"),
                avatar_url: text(raw, "avatarUrl"),
                requested,
            });
        }
        Ok(PullRequestReviewerCandidates {
            reviewers,
            complete: repository["assignableUsers"]["pageInfo"]["hasNextPage"].as_bool()
                != Some(true),
        })
    }
}

impl PullRequestReads {
    pub fn new(api: Arc<GitHubApi>) -> Arc<Self> {
        Arc::new(Self {
            api,
            revisions: ReadCache::new(1024 * 1024),
            files: ReadCache::new(32 * 1024 * 1024),
            texts: ReadCache::new(16 * 1024 * 1024),
            conversations: ReadCache::new(16 * 1024 * 1024),
            replies: ReadCache::new(4 * 1024 * 1024),
            viewed: ReadCache::new(4 * 1024 * 1024),
            labels: ReadCache::new(1024 * 1024),
            reviewers: ReadCache::new(1024 * 1024),
            action_states: ReadCache::new(1024 * 1024),
            node_ids: Mutex::new(Vec::new()),
        })
    }

    /// Captures the credential once, so every request of a read is the same account's.
    pub(super) fn reader<'a>(&'a self, key: &'a PullRequestKey) -> Result<Reader<'a>, GitHubError> {
        let repository = Repository::from_key(key).ok_or(GitHubError::InvalidInput)?;
        let credential = self.api.credentials().get(&key.host)?;
        Ok(Reader {
            api: &self.api,
            key,
            repository,
            account: credential.fingerprint.clone(),
            options: RequestOptions {
                authentication: Authentication::Pinned(credential),
                interactive: true,
                ..Default::default()
            },
        })
    }

    /// The whole diff, or with `page` one page of the changed files once the whole diff was
    /// refused.
    pub fn files(
        &self,
        key: &PullRequestKey,
        page: Option<u32>,
    ) -> Result<Fresh<Files>, GitHubError> {
        if page == Some(0) {
            return Err(GitHubError::InvalidInput);
        }
        let reader = self.reader(key)?;
        let revisions = self.revisions(&reader)?;
        self.files.read(
            // Files are a head's: a moved head is a different read.
            reader.read_key(format!("files {} {page:?}", revisions.head)),
            || {
                let files = match page {
                    None => reader.whole_diff(&revisions)?,
                    Some(page) => reader.files_page(&revisions, page)?,
                };
                // Files that disagree with the count read beside the revisions were listed
                // after a push the revisions predate; the next read asks for them again.
                let listed = files.files.len() as u64
                    + page.map_or(0, |page| (page as u64 - 1) * FILES_PER_PAGE as u64);
                if listed > revisions.changed_files
                    || (files.next_page.is_none() && listed != revisions.changed_files)
                {
                    self.revisions.invalidate(key);
                }
                Ok((files, READ_TTL))
            },
            |files| {
                files
                    .files
                    .iter()
                    .map(|file| {
                        file.path.len()
                            + match &file.patch {
                                PullRequestPatch::Hunks(hunks) => hunks.len(),
                                _ => 0,
                            }
                    })
                    .sum()
            },
        )
    }

    pub(super) fn revisions(&self, reader: &Reader<'_>) -> Result<Arc<Revisions>, GitHubError> {
        let revisions = self
            .revisions
            .read(reader.read_key("revisions"), || reader.revisions(), |_| 256)?
            .value;
        let mut node_ids = self.node_ids.lock().unwrap();
        node_ids.retain(|(key, _)| key != reader.key);
        if node_ids.len() >= NODE_IDS {
            node_ids.remove(0);
        }
        node_ids.push((reader.key.clone(), revisions.node_id.clone()));
        Ok(revisions)
    }

    /// The pull request's GraphQL id, from its REST read the first time it is needed.
    pub(super) fn node_id(&self, reader: &Reader<'_>) -> Result<String, GitHubError> {
        let held = self
            .node_ids
            .lock()
            .unwrap()
            .iter()
            .find(|(key, _)| key == reader.key)
            .map(|(_, id)| id.clone());
        match held {
            Some(id) => Ok(id),
            None => Ok(self.revisions(reader)?.node_id.clone()),
        }
    }

    /// A file's text at a commit of the pull request's repository.
    pub fn file_text(
        &self,
        key: &PullRequestKey,
        revision: &str,
        path: &str,
    ) -> Result<Fresh<PullRequestFileText>, GitHubError> {
        if !is_revision(revision) || !is_repository_path(path) {
            return Err(GitHubError::InvalidInput);
        }
        let reader = self.reader(key)?;
        self.texts.read(
            reader.read_key(format!("text {revision} {path}")),
            || Ok((reader.file_text(revision, path)?, TEXT_TTL)),
            |text| match text {
                PullRequestFileText::Text(text) => text.len(),
                _ => 0,
            },
        )
    }

    pub fn conversation(
        &self,
        key: &PullRequestKey,
    ) -> Result<Fresh<PullRequestConversation>, GitHubError> {
        let reader = self.reader(key)?;
        self.conversations.read(
            reader.read_key("conversation"),
            || reader.conversation(),
            conversation_bytes,
        )
    }

    /// The account the host reads the pull request as, named as its conversation names it.
    pub fn account(&self, key: &PullRequestKey) -> Result<String, GitHubError> {
        Ok(self.reader(key)?.account_name())
    }

    pub fn thread_replies(
        &self,
        key: &PullRequestKey,
        thread: &str,
        after: &str,
    ) -> Result<Fresh<PullRequestThreadReplies>, GitHubError> {
        let reader = self.reader(key)?;
        self.replies.read(
            reader.read_key(format!("replies {thread} {after}")),
            || Ok((reader.thread_replies(thread, after)?, READ_TTL)),
            |replies| replies.comments.iter().map(comment_bytes).sum(),
        )
    }

    pub fn viewed_files(
        &self,
        key: &PullRequestKey,
    ) -> Result<Fresh<PullRequestViewedFiles>, GitHubError> {
        let reader = self.reader(key)?;
        self.viewed.read(
            reader.read_key("viewed"),
            || Ok((reader.viewed_files()?, VIEWED_TTL)),
            |viewed| viewed.files.iter().map(|(path, _)| path.len() + 8).sum(),
        )
    }

    /// Marks or unmarks files as viewed in one request; the viewed files are read again after,
    /// whether it succeeded or not.
    pub fn set_viewed(
        &self,
        key: &PullRequestKey,
        paths: &[String],
        viewed: bool,
    ) -> Result<(), GitHubError> {
        if paths.is_empty() {
            return Ok(());
        }
        let reader = self.reader(key)?;
        self.viewed.invalidate(key);
        let result = self.mark(&reader, paths, viewed);
        self.viewed.invalidate(key);
        result
    }

    fn mark(&self, reader: &Reader<'_>, paths: &[String], viewed: bool) -> Result<(), GitHubError> {
        let node_id = self.node_id(reader)?;
        let items: Vec<_> = paths
            .iter()
            .enumerate()
            .map(|(index, path)| AliasItem {
                key: index,
                variables: Variables::from([("path".into(), ("String!".into(), json!(path)))]),
            })
            .collect();
        let field = if viewed {
            "markFileAsViewed"
        } else {
            "unmarkFileAsViewed"
        };
        let document = graphql::aliases(
            "mutation",
            "SetPullRequestFilesViewed",
            "f",
            &items,
            &Variables::from([("pullRequestId".into(), ("ID!".into(), json!(node_id)))]),
            |v| {
                format!(
                    "{field}(input: {{ pullRequestId: $pullRequestId, path: {} }}) {{ clientMutationId }}",
                    v["path"]
                )
            },
            |fields| fields,
        )
        .ok_or(GitHubError::InvalidInput)?;
        self.api.graphql(
            &reader.key.host,
            &document,
            &RequestOptions {
                operation: "SetPullRequestFilesViewed",
                ..reader.options.clone()
            },
        )?;
        Ok(())
    }

    /// Media on GitHub's hosts is read only where the pull request's conversation points at it,
    /// so this is not a way to read anything else the account can see.
    pub fn media(
        &self,
        key: &PullRequestKey,
        url: &str,
        validator: Option<&str>,
    ) -> Result<PullRequestMedia, GitHubError> {
        let Some(source) = media::classify(url) else {
            return Ok(PullRequestMedia::Unsupported);
        };
        let conversation = self.conversation(key)?.value;
        let replies = self.replies.held(key);
        let mut comments = conversation_comments(&conversation)
            .chain(replies.iter().flat_map(|replies| replies.comments.iter()));
        let named = match source {
            media::MediaSource::Avatar(_) => {
                comments.any(|comment| {
                    comment
                        .author
                        .as_ref()
                        .and_then(|author| author.avatar_url.as_deref())
                        == Some(url)
                }) || conversation
                    .reviewers
                    .iter()
                    .any(|reviewer| reviewer.avatar_url.as_deref() == Some(url))
            }
            _ => comments.any(|comment| comment.body.contains(url)),
        };
        if !named {
            return Err(GitHubError::InvalidInput);
        }
        media::fetch(&self.api, &source, validator)
    }

    pub fn label_candidates(
        &self,
        key: &PullRequestKey,
    ) -> Result<Fresh<PullRequestLabelCandidates>, GitHubError> {
        let reader = self.reader(key)?;
        self.labels.read(
            reader.read_key("labels"),
            || Ok((reader.label_candidates()?, READ_TTL)),
            |labels| {
                labels
                    .labels
                    .iter()
                    .map(|label| label.name.len() + 64)
                    .sum()
            },
        )
    }

    pub fn reviewer_candidates(
        &self,
        key: &PullRequestKey,
    ) -> Result<Fresh<PullRequestReviewerCandidates>, GitHubError> {
        let reader = self.reader(key)?;
        self.reviewers.read(
            reader.read_key("reviewers"),
            || Ok((reader.reviewer_candidates()?, READ_TTL)),
            |reviewers| {
                reviewers
                    .reviewers
                    .iter()
                    .map(|reviewer| reviewer.reviewer.login.len() + 128)
                    .sum()
            },
        )
    }

    /// Drops every answer about the pull request, for a change the sync observed.
    pub fn invalidate(&self, key: &PullRequestKey) {
        self.revisions.invalidate(key);
        self.files.invalidate(key);
        self.conversations.invalidate(key);
        self.replies.invalidate(key);
        self.viewed.invalidate(key);
        self.labels.invalidate(key);
        self.reviewers.invalidate(key);
        self.action_states.invalidate(key);
    }
}

fn conversation_comments(
    conversation: &PullRequestConversation,
) -> impl Iterator<Item = &PullRequestComment> {
    std::iter::once(&conversation.description)
        .chain(&conversation.comments)
        .chain(
            conversation
                .threads
                .iter()
                .flat_map(|thread| thread.comments.iter()),
        )
}

fn comment_bytes(comment: &PullRequestComment) -> usize {
    comment.body.len() + comment.id.len() + 256
}

fn conversation_bytes(conversation: &PullRequestConversation) -> usize {
    conversation_comments(conversation).map(comment_bytes).sum()
}

/// GitHub names a reviewer by login, and a team by its slug, in reads and writes alike.
fn reviewer(login: String, kind: PullRequestReviewerKind) -> PullRequestReviewer {
    PullRequestReviewer {
        id: login.clone(),
        login,
        kind,
    }
}

pub(super) fn is_revision(value: &str) -> bool {
    matches!(value.len(), 40 | 64)
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// A path the contents endpoint reads as a file in the repository; dot segments would leave it.
fn is_repository_path(path: &str) -> bool {
    !path.is_empty()
        && path
            .split('/')
            .all(|segment| !matches!(segment, "" | "." | ".."))
}

pub(crate) fn percent_encode(segment: &str) -> String {
    segment
        .bytes()
        .map(|byte| {
            if byte.is_ascii_alphanumeric() || b"-._~".contains(&byte) {
                (byte as char).to_string()
            } else {
                format!("%{byte:02X}")
            }
        })
        .collect()
}

pub(super) fn reaction_name(content: PullRequestReactionContent) -> &'static str {
    match content {
        PullRequestReactionContent::ThumbsUp => "THUMBS_UP",
        PullRequestReactionContent::ThumbsDown => "THUMBS_DOWN",
        PullRequestReactionContent::Laugh => "LAUGH",
        PullRequestReactionContent::Hooray => "HOORAY",
        PullRequestReactionContent::Confused => "CONFUSED",
        PullRequestReactionContent::Heart => "HEART",
        PullRequestReactionContent::Rocket => "ROCKET",
        PullRequestReactionContent::Eyes => "EYES",
    }
}

fn text(raw: &Value, field: &str) -> Option<String> {
    raw[field]
        .as_str()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

fn nodes(connection: &Value) -> impl Iterator<Item = &Value> {
    connection["nodes"].as_array().into_iter().flatten()
}

fn next_cursor(connection: &Value) -> Option<String> {
    connection["pageInfo"]["endCursor"]
        .as_str()
        .filter(|_| connection["pageInfo"]["hasNextPage"].as_bool() == Some(true))
        .map(str::to_owned)
}

fn comment_from(
    raw: &Value,
    review_state: Option<PullRequestReviewState>,
) -> Option<PullRequestComment> {
    let text = |field: &str| raw[field].as_str().map(str::to_owned);
    Some(PullRequestComment {
        id: text("id")?,
        author: raw["author"]["login"]
            .as_str()
            .map(|login| PullRequestActor {
                login: login.to_owned(),
                avatar_url: raw["author"]["avatarUrl"].as_str().map(str::to_owned),
            }),
        body: text("body").unwrap_or_default(),
        created_at: text("submittedAt").or_else(|| text("createdAt"))?,
        edited_at: text("lastEditedAt"),
        url: text("url").filter(|url| !url.is_empty()),
        review_state,
        viewer_can_update: raw["viewerCanUpdate"].as_bool() == Some(true),
        viewer_can_react: raw["viewerCanReact"].as_bool() == Some(true),
        reactions: raw["reactionGroups"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|group| {
                let count = group["reactors"]["totalCount"].as_u64()?;
                let content = PullRequestReactionContent::ALL
                    .into_iter()
                    .find(|content| Some(reaction_name(*content)) == group["content"].as_str())?;
                (count > 0).then(|| PullRequestReaction {
                    content,
                    count,
                    viewer_reacted: group["viewerHasReacted"].as_bool().unwrap_or(false),
                })
            })
            .collect(),
    })
}

/// A review with no body is kept only when its state is the event itself. GitHub also opens a
/// bodiless `COMMENTED` review around line comments, which the review threads carry.
fn review_from(raw: &Value) -> Option<PullRequestComment> {
    let state = match raw["state"].as_str()? {
        "APPROVED" => PullRequestReviewState::Approved,
        "CHANGES_REQUESTED" => PullRequestReviewState::ChangesRequested,
        "DISMISSED" => PullRequestReviewState::Dismissed,
        "PENDING" => PullRequestReviewState::Pending,
        _ => PullRequestReviewState::Commented,
    };
    if raw["body"].as_str().unwrap_or_default().trim().is_empty()
        && matches!(
            state,
            PullRequestReviewState::Commented | PullRequestReviewState::Pending
        )
    {
        return None;
    }
    comment_from(raw, Some(state))
}

fn thread_from(raw: &Value) -> Option<PullRequestReviewThread> {
    let path = raw["path"].as_str()?.to_owned();
    let comments: Vec<_> = nodes(&raw["comments"]).collect();
    let side = |field: &str| match raw[field].as_str() {
        Some("LEFT") => Some(ReviewSide::Old),
        Some("RIGHT") => Some(ReviewSide::New),
        _ => None,
    };
    let line = |field: &str| {
        raw[field]
            .as_u64()
            .and_then(|line| u32::try_from(line).ok())
    };
    // A current thread sits on the lines of the commit its comments are positioned on; an
    // outdated one keeps the lines as they were first commented on.
    let (end, start, commit) = match line("line") {
        Some(end) => (Some(end), line("startLine"), "commit"),
        None => (
            line("originalLine"),
            line("originalStartLine"),
            "originalCommit",
        ),
    };
    let anchor = end.and_then(|end_line| {
        Some(PullRequestReviewAnchor {
            revision: comments.first()?[commit]["oid"].as_str()?.to_owned(),
            path: path.clone(),
            side: side("diffSide")?,
            start_line: start.unwrap_or(end_line).min(end_line),
            end_line,
        })
    });
    Some(PullRequestReviewThread {
        id: raw["id"].as_str()?.to_owned(),
        path,
        resolved: raw["isResolved"].as_bool().unwrap_or(false),
        outdated: raw["isOutdated"].as_bool().unwrap_or(false),
        anchor,
        diff_hunk: comments
            .first()
            .and_then(|comment| comment["diffHunk"].as_str())
            .map(|hunk| {
                let lines: Vec<_> = hunk
                    .lines()
                    .filter(|line| !line.starts_with("@@"))
                    .collect();
                lines[lines.len().saturating_sub(4)..].join("\n")
            }),
        comments: comments
            .into_iter()
            .filter_map(|raw| comment_from(raw, None))
            .collect(),
        total_comments: raw["comments"]["totalCount"].as_u64().unwrap_or(0),
        replies_after: next_cursor(&raw["comments"]),
        viewer_can_reply: raw["viewerCanReply"].as_bool() == Some(true),
        viewer_can_resolve: raw[if raw["isResolved"].as_bool() == Some(true) {
            "viewerCanUnresolve"
        } else {
            "viewerCanResolve"
        }]
        .as_bool()
            == Some(true),
    })
}

/// Requests lead, as GitHub's sidebar lists them; a latest review by someone not asked again
/// follows with its verdict. A dismissed or pending review is no verdict.
fn reviewer_states(pr: &Value) -> Vec<PullRequestReviewerState> {
    let mut reviewers: Vec<PullRequestReviewerState> = Vec::new();
    for node in nodes(&pr["reviewRequests"]) {
        let raw = &node["requestedReviewer"];
        let reviewer = match (text(raw, "slug"), text(raw, "login")) {
            (Some(slug), _) => reviewer(slug, PullRequestReviewerKind::Team),
            (None, Some(login)) => reviewer(login, PullRequestReviewerKind::User),
            (None, None) => continue,
        };
        reviewers.push(PullRequestReviewerState {
            reviewer,
            avatar_url: text(raw, "avatarUrl"),
            verdict: None,
        });
    }
    for node in nodes(&pr["latestReviews"]) {
        let verdict = match node["state"].as_str() {
            Some("APPROVED") => PullRequestReviewState::Approved,
            Some("CHANGES_REQUESTED") => PullRequestReviewState::ChangesRequested,
            Some("COMMENTED") => PullRequestReviewState::Commented,
            _ => continue,
        };
        let Some(login) = text(&node["author"], "login") else {
            continue;
        };
        let reviewer = reviewer(login, PullRequestReviewerKind::User);
        if reviewers.iter().any(|known| known.reviewer == reviewer) {
            continue;
        }
        reviewers.push(PullRequestReviewerState {
            reviewer,
            avatar_url: text(&node["author"], "avatarUrl"),
            verdict: Some(verdict),
        });
    }
    reviewers
}

fn listed_file(row: &Value) -> Option<PullRequestFile> {
    let path = row["filename"].as_str()?.to_owned();
    let status = row["status"].as_str()?;
    let kind = match status {
        "added" | "copied" => FileChangeKind::Create,
        "removed" => FileChangeKind::Delete,
        "renamed" => FileChangeKind::Rename,
        _ => FileChangeKind::Modify,
    };
    let additions = row["additions"].as_u64().unwrap_or(0);
    let deletions = row["deletions"].as_u64().unwrap_or(0);
    // The files API gives no reason for a missing patch: changed lines without one were
    // withheld for size. A file with neither is empty, renamed alone, binary, or past GitHub's
    // diff limits, and only the first two can be told apart here.
    let patch = match row["patch"].as_str().filter(|patch| !patch.is_empty()) {
        Some(patch) => PullRequestPatch::Hunks(patch.to_owned()),
        None if additions + deletions > 0 => PullRequestPatch::Oversized,
        None if kind == FileChangeKind::Rename || row["sha"].as_str() == Some(EMPTY_BLOB) => {
            PullRequestPatch::Hunks(String::new())
        }
        None => PullRequestPatch::Withheld,
    };
    Some(PullRequestFile {
        previous_path: matches!(status, "renamed" | "copied")
            .then(|| row["previous_filename"].as_str().map(str::to_owned))
            .flatten(),
        path,
        kind,
        additions,
        deletions,
        patch,
    })
}

pub(crate) fn diff_files(diff: &str) -> Option<Vec<PullRequestFile>> {
    agent::file_changes_from_unified_diff(diff)
        .ok()?
        .into_iter()
        .map(|change| {
            let section = change.diff.unwrap_or_default();
            let mut previous_path = None;
            let mut binary = false;
            let mut hunks_at = None;
            let mut offset = 0;
            for raw in section.split_inclusive('\n') {
                if raw.starts_with("@@") {
                    hunks_at = Some(offset);
                    break;
                }
                offset += raw.len();
                let line = raw.trim_end_matches('\n');
                if let Some(path) = line.strip_prefix("rename from ") {
                    previous_path = Some(path.to_owned());
                }
                binary |= line.starts_with("Binary files ") || line == "GIT binary patch";
            }
            let hunks = hunks_at.map_or("", |at| &section[at..]);
            let count = |sign: char| {
                hunks
                    .lines()
                    .filter(|line| line.starts_with(sign) && !line.starts_with("@@"))
                    .count() as u64
            };
            Some(PullRequestFile {
                previous_path: previous_path.filter(|_| change.kind == FileChangeKind::Rename),
                path: change.path,
                kind: change.kind,
                additions: count('+'),
                deletions: count('-'),
                patch: if binary {
                    PullRequestPatch::Binary
                } else {
                    PullRequestPatch::Hunks(hunks.to_owned())
                },
            })
        })
        .collect()
}
