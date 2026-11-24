//! What Tcode reads of a GitLab merge request, in Tcode's terms. The merge request itself is
//! read over REST; its discussions over GraphQL, which answers them for a public project
//! without a token, where REST's notes and discussions refuse one.

use super::{
    api::{Api, Request, error},
    repository,
};
use crate::forge::{ForgeError, ForgeErrorKind};
use serde_json::{Value, json};
use tcode_core::{
    pull_request::{
        ChecksState, Mergeability, PullRequestAuthor, PullRequestKey, PullRequestMergeMethod,
        PullRequestSnapshot, PullRequestState,
    },
    pull_request_watch::{CheckStatus, PullRequestCheck, PullRequestRemark},
    session::ReviewSide,
};
use tcode_protocol::{
    PullRequestActor, PullRequestCapabilities, PullRequestComment, PullRequestFile,
    PullRequestLabel, PullRequestMergeState, PullRequestPatch, PullRequestPermissions,
    PullRequestReaction, PullRequestReactionContent, PullRequestReviewAnchor,
    PullRequestReviewState, PullRequestReviewThread, PullRequestReviewVerdict, PullRequestReviewer,
    PullRequestReviewerKind, PullRequestReviewerState,
};

/// Past this many pages a list is reported incomplete rather than read on.
pub(super) const MAX_PAGES: usize = 10;
/// The merge request itself, as reactions and edits name it.
pub(super) const PULL_REQUEST: &str = "pr";

pub(super) fn capabilities() -> PullRequestCapabilities {
    PullRequestCapabilities {
        reply: true,
        resolve: true,
        reactions: true,
        labels: true,
        // GitLab's REST API approves or does not; it has no verdict that asks for changes.
        request_changes: false,
        // A `Draft:` title prefix, which the API edits as the title.
        draft: true,
        reopen: true,
        // `merge_when_pipeline_succeeds` reads back on the merge request, and cancels.
        auto_merge: true,
        update_branch: true,
        // GitLab only rebases a source branch onto its target.
        update_merge: false,
        revert: false,
        host_viewed_marks: false,
        // GraphQL reads the message GitLab would write, which the merge can replace.
        merge_message: true,
    }
}

/// The merge request, its discussions and the account reading them, with the permissions
/// GitLab gives that account on each.
const CONVERSATION: &str = "query($path: ID!, $iid: String!, $after: String) {
  currentUser { username }
  project(fullPath: $path) {
    mergeRequest(iid: $iid) {
      iid description createdAt webUrl
      author { username avatarUrl }
      userPermissions { updateMergeRequest adminMergeRequest createNote canApprove }
      labels { nodes { title color description } }
      reviewers { nodes { id username avatarUrl mergeRequestInteraction { reviewState } } }
      approvedBy { nodes { id username avatarUrl } }
      awardEmoji { nodes { name user { username } } }
      discussions(first: 50, after: $after) {
        pageInfo { hasNextPage endCursor }
        nodes {
          id resolvable resolved
          notes { nodes {
            id body system url createdAt lastEditedAt lastEditedBy { username }
            author { username avatarUrl }
            awardEmoji { nodes { name user { username } } }
            userPermissions { adminNote awardEmoji resolveNote }
            position { positionType newPath oldPath newLine oldLine diffRefs { baseSha headSha } }
          } }
        }
      }
    }
  }
}";

/// One page of the labels the project may take, its groups' included. REST's label list
/// refuses an anonymous reader on gitlab.com; this answers one.
const LABELS: &str = "query($path: ID!) {
  project(fullPath: $path) { labels(first: 100, includeAncestorGroups: true) { pageInfo { hasNextPage } nodes { title color description } } }
}";

/// Each path's blob at a revision; a path the revision lacks is left out.
const BLOBS: &str = "query($path: ID!, $ref: String!, $paths: [String!]!) {
  project(fullPath: $path) { repository { blobs(ref: $ref, paths: $paths) { nodes { path oid } } } }
}";

/// Lines added and removed, which REST's merge request does not count.
const DIFF_STATS: &str = "query($path: ID!, $iid: String!) {
  project(fullPath: $path) { mergeRequest(iid: $iid) { diffStatsSummary { additions deletions } } }
}";

/// What a merge or squash commit would say, which a merge may replace.
const MERGE_MESSAGE: &str = "query($path: ID!, $iid: String!) {
  project(fullPath: $path) { mergeRequest(iid: $iid) { diffHeadSha defaultMergeCommitMessage defaultSquashCommitMessage } }
}";

/// What the reading account may do to the merge request's state and branch.
const VIEWER_PERMISSIONS: &str = "query($path: ID!, $iid: String!) {
  project(fullPath: $path) { mergeRequest(iid: $iid) { userPermissions { updateMergeRequest pushToSourceBranch } } }
}";

/// A merge request's place on its server and the requests that read it.
pub(super) struct Mr<'a> {
    pub(super) api: &'a Api,
    pub(super) key: &'a PullRequestKey,
}

impl Mr<'_> {
    pub(super) fn authority(&self) -> &str {
        &self.key.host
    }
    /// Below the project.
    pub(super) fn project_path(&self, rest: &str) -> String {
        format!(
            "/projects/{}{rest}",
            repository::project_id(&self.key.repository)
        )
    }
    /// Below the merge request.
    pub(super) fn path(&self, rest: &str) -> String {
        self.project_path(&format!("/merge_requests/{}{rest}", self.key.number))
    }
    pub(super) fn get(&self, path: String, operation: &'static str) -> Result<Value, ForgeError> {
        self.api
            .send(self.authority(), Request::get(path, operation))?
            .json()
    }
    pub(super) fn mr(&self) -> Result<Value, ForgeError> {
        let mr = self.get(
            self.path("?include_diverged_commits_count=true"),
            "MergeRequest",
        )?;
        if mr["iid"].as_u64() != Some(self.key.number) {
            return Err(error(
                ForgeErrorKind::Uncertain,
                "GitLab answered another merge request",
            ));
        }
        Ok(mr)
    }
    pub(super) fn graphql(
        &self,
        query: &str,
        mut variables: Value,
        operation: &'static str,
    ) -> Result<Value, ForgeError> {
        variables["path"] = json!(self.key.repository);
        variables["iid"] = json!(self.key.number.to_string());
        self.api
            .graphql(self.authority(), query, variables, operation)
    }
    /// The signed-in account, or `None` when reading anonymously.
    pub(super) fn viewer(&self) -> Result<Option<String>, ForgeError> {
        if self.api.credential(self.authority())?.is_none() {
            return Ok(None);
        }
        let user = self.get("/user".into(), "Viewer")?;
        Ok(text(&user, "username"))
    }
    /// The merge request's GraphQL fields and its discussions, page by page up to
    /// [`MAX_PAGES`]; `false` when more remained.
    pub(super) fn discussions(
        &self,
    ) -> Result<(Value, Option<String>, Vec<Value>, bool), ForgeError> {
        let mut after: Option<String> = None;
        let mut first = None;
        let mut viewer = None;
        let mut discussions = Vec::new();
        for _ in 0..MAX_PAGES {
            let data = self.graphql(CONVERSATION, json!({ "after": after }), "Discussions")?;
            let mr = data["project"]["mergeRequest"].clone();
            if mr.is_null() {
                return Err(error(
                    ForgeErrorKind::NotFound,
                    "GitLab merge request not found",
                ));
            }
            viewer = viewer.or_else(|| text(&data["currentUser"], "username"));
            let page = &mr["discussions"];
            discussions.extend(nodes(page).cloned());
            let next = page["pageInfo"]["hasNextPage"].as_bool() == Some(true);
            after = text(&page["pageInfo"], "endCursor");
            first.get_or_insert(mr);
            if !next || after.is_none() {
                return Ok((first.unwrap_or_default(), viewer, discussions, true));
            }
        }
        Ok((first.unwrap_or_default(), viewer, discussions, false))
    }
    /// Lines added and removed, or `None` when GitLab has no count for the merge request.
    pub(super) fn diff_stats(&self) -> Result<Option<(u64, u64)>, ForgeError> {
        let data = self.graphql(DIFF_STATS, json!({}), "DiffStats")?;
        let stats = &data["project"]["mergeRequest"]["diffStatsSummary"];
        Ok(stats["additions"].as_u64().zip(stats["deletions"].as_u64()))
    }
    /// Whether the reading account may change the merge request's state, and push to its source
    /// branch.
    pub(super) fn may_update(&self) -> Result<(bool, bool), ForgeError> {
        if self.api.credential(self.authority())?.is_none() {
            return Ok((false, false));
        }
        let data = self.graphql(VIEWER_PERMISSIONS, json!({}), "ViewerPermissions")?;
        let permissions = &data["project"]["mergeRequest"]["userPermissions"];
        Ok((
            permissions["updateMergeRequest"].as_bool() == Some(true),
            permissions["pushToSourceBranch"].as_bool() == Some(true),
        ))
    }
    /// The merge or squash message GitLab would write at `head`.
    pub(super) fn merge_message(
        &self,
        squash: bool,
    ) -> Result<(Option<String>, Option<String>), ForgeError> {
        let data = self.graphql(MERGE_MESSAGE, json!({}), "MergeMessage")?;
        let mr = &data["project"]["mergeRequest"];
        let field = if squash {
            "defaultSquashCommitMessage"
        } else {
            "defaultMergeCommitMessage"
        };
        Ok((text(mr, "diffHeadSha"), text(mr, field)))
    }
    /// One page of the project's labels, and whether it held them all.
    pub(super) fn labels(&self) -> Result<(Vec<Value>, bool), ForgeError> {
        let data = self.api.graphql(
            self.authority(),
            LABELS,
            json!({ "path": self.key.repository }),
            "Labels",
        )?;
        let labels = &data["project"]["labels"];
        Ok((
            nodes(labels).cloned().collect(),
            labels["pageInfo"]["hasNextPage"].as_bool() != Some(true),
        ))
    }
    /// Each path's blob at `revision`, 100 paths a query; `None` when GitLab did not answer
    /// for the project at all.
    pub(super) fn blobs(
        &self,
        revision: &str,
        paths: &[String],
    ) -> Result<Option<std::collections::BTreeMap<String, String>>, ForgeError> {
        let mut blobs = std::collections::BTreeMap::new();
        for chunk in paths.chunks(100) {
            let data = self.api.graphql(
                self.authority(),
                BLOBS,
                json!({ "path": self.key.repository, "ref": revision, "paths": chunk }),
                "Blobs",
            )?;
            let Some(found) = data["project"]["repository"]["blobs"]["nodes"].as_array() else {
                return Ok(None);
            };
            for blob in found {
                if let (Some(path), Some(oid)) = (text(blob, "path"), text(blob, "oid")) {
                    blobs.insert(path, oid);
                }
            }
        }
        Ok(Some(blobs))
    }
}

pub(super) fn text(raw: &Value, field: &str) -> Option<String> {
    raw[field].as_str().map(str::to_owned)
}

/// A GraphQL connection's nodes.
pub(super) fn nodes(connection: &Value) -> impl Iterator<Item = &Value> {
    connection["nodes"].as_array().into_iter().flatten()
}

/// The numeric id at the end of a global id, `gid://gitlab/DiffNote/42` → `42`.
pub(super) fn rest_id(global: &str) -> Option<String> {
    let id = global.rsplit('/').next()?;
    (!id.is_empty() && id.bytes().all(|b| b.is_ascii_digit())).then(|| id.to_owned())
}

pub(super) fn state(mr: &Value) -> Option<PullRequestState> {
    if mr["merged_at"].as_str().is_some_and(|at| !at.is_empty()) {
        return Some(PullRequestState::Merged);
    }
    match mr["state"].as_str()? {
        "merged" => Some(PullRequestState::Merged),
        "closed" => Some(PullRequestState::Closed),
        // A locked merge request is an open one whose discussion is locked.
        "opened" | "locked" => Some(PullRequestState::Open),
        _ => None,
    }
}

/// What one read says of conflicts; a check GitLab has yet to finish is pending.
pub(super) fn mergeability(mr: &Value) -> Mergeability {
    let detailed = mr["detailed_merge_status"].as_str().unwrap_or_default();
    let status = mr["merge_status"].as_str().unwrap_or_default();
    if mr["has_conflicts"].as_bool() == Some(true)
        || detailed == "conflict"
        || status == "cannot_be_merged"
    {
        Mergeability::Conflicting
    } else if matches!(
        detailed,
        "checking" | "unchecked" | "preparing" | "approvals_syncing"
    ) || matches!(
        status,
        "checking" | "unchecked" | "cannot_be_merged_recheck"
    ) {
        Mergeability::Pending
    } else {
        Mergeability::Clean
    }
}

/// What merging now would meet, from `detailed_merge_status`, `mergeability` as the watch
/// reads it, and the pipeline.
pub(super) fn merge_state(
    mr: &Value,
    mergeability: Mergeability,
    checks: &[PullRequestCheck],
) -> PullRequestMergeState {
    if mr["draft"].as_bool() == Some(true) {
        return PullRequestMergeState::Draft;
    }
    if mergeability == Mergeability::Conflicting {
        return PullRequestMergeState::Dirty;
    }
    match mr["detailed_merge_status"].as_str().unwrap_or_default() {
        "mergeable"
            if checks
                .iter()
                .any(|check| check.status != CheckStatus::Success) =>
        {
            PullRequestMergeState::Unstable
        }
        "mergeable" => PullRequestMergeState::Clean,
        "draft_status" => PullRequestMergeState::Draft,
        "need_rebase" => PullRequestMergeState::Behind,
        "conflict" | "checking" | "unchecked" | "preparing" | "approvals_syncing" | "not_open"
        | "" => PullRequestMergeState::Unknown,
        // Approvals, discussions, a pipeline that must pass, and every rule GitLab adds.
        _ => PullRequestMergeState::Blocked,
    }
}

/// Everything not finished is pending, a pipeline waiting on a person's manual job or on its
/// schedule included: it has not passed.
pub(super) fn pipeline_status(status: &str) -> CheckStatus {
    match status {
        "success" => CheckStatus::Success,
        "failed" => CheckStatus::Failure,
        "canceled" | "canceling" => CheckStatus::Cancelled,
        "skipped" => CheckStatus::Skipped,
        _ => CheckStatus::Pending,
    }
}

/// The head pipeline as the one check GitLab reports for a merge request.
pub(super) fn checks(mr: &Value) -> Vec<PullRequestCheck> {
    let pipeline = &mr["head_pipeline"];
    let Some(status) = pipeline["status"].as_str() else {
        return Vec::new();
    };
    vec![PullRequestCheck {
        name: "Pipeline".into(),
        status: pipeline_status(status),
        url: text(pipeline, "web_url"),
        required: None,
    }]
}

pub(super) fn checks_state(checks: &[PullRequestCheck]) -> Option<ChecksState> {
    let check = checks.first()?;
    Some(if check.status.failed() {
        ChecksState::Failing
    } else if check.status == CheckStatus::Pending {
        ChecksState::Pending
    } else {
        ChecksState::Passing
    })
}

/// "12", or "1000+" past GitLab's counting limit, whose number is the floor.
fn changed_files(mr: &Value) -> u64 {
    let count = mr["changes_count"].as_str().unwrap_or_default();
    count.trim_end_matches('+').parse().unwrap_or(0)
}

/// `stats` is lines added and removed, where GitLab counted them.
pub(super) fn snapshot(
    mr: &Value,
    stats: Option<(u64, u64)>,
    mergeability: Mergeability,
    synced_at: u64,
) -> Option<PullRequestSnapshot> {
    Some(PullRequestSnapshot {
        state: state(mr)?,
        title: text(mr, "title")?,
        head_branch: text(mr, "source_branch")?,
        base_branch: text(mr, "target_branch")?,
        is_draft: mr["draft"].as_bool().unwrap_or(false),
        updated_at: text(mr, "updated_at")?,
        synced_at,
        closed_at: text(mr, "closed_at"),
        merged_at: text(mr, "merged_at"),
        author: text(&mr["author"], "username").map(|login| PullRequestAuthor {
            login,
            avatar_url: text(&mr["author"], "avatar_url"),
        }),
        additions: stats.map(|(additions, _)| additions),
        deletions: stats.map(|(_, deletions)| deletions),
        changed_files: Some(changed_files(mr)),
        review_decision: None,
        checks_state: checks_state(&checks(mr)),
        mergeability,
    })
}

/// The methods the project merges by: its one merge method, which a rebasing project reaches
/// by rebase, and a squash where the project allows one. An unknown setting offers nothing.
pub(super) fn merge_methods(project: &Value) -> Vec<PullRequestMergeMethod> {
    let base = match project["merge_method"].as_str() {
        Some("merge") => Some(PullRequestMergeMethod::Merge),
        Some("rebase_merge" | "ff") => Some(PullRequestMergeMethod::Rebase),
        _ => None,
    };
    match project["squash_option"].as_str() {
        Some("always") => vec![PullRequestMergeMethod::Squash],
        Some("default_on" | "default_off") => base
            .into_iter()
            .chain([PullRequestMergeMethod::Squash])
            .collect(),
        _ => base.into_iter().collect(),
    }
}

/// An avatar URL as the server's own page would load it: GitLab names its own uploads below
/// the server.
fn absolute(authority: &str, url: Option<String>) -> Option<String> {
    url.map(|url| {
        if url.starts_with('/') {
            format!("https://{authority}{url}")
        } else {
            url
        }
    })
}

pub(super) fn actor(authority: &str, raw: &Value) -> Option<PullRequestActor> {
    Some(PullRequestActor {
        login: text(raw, "username")?,
        avatar_url: absolute(
            authority,
            text(raw, "avatarUrl").or_else(|| text(raw, "avatar_url")),
        ),
    })
}

/// When the text changed after it was written. GitLab stamps `lastEditedAt` on resolving a
/// thread too, and names who edited only when the text changed.
fn edited_at(raw: &Value) -> Option<String> {
    raw["lastEditedBy"]
        .is_object()
        .then(|| text(raw, "lastEditedAt"))
        .flatten()
        .filter(|at| raw["createdAt"].as_str() != Some(at))
}

pub(super) fn award_name(content: PullRequestReactionContent) -> &'static str {
    match content {
        PullRequestReactionContent::ThumbsUp => "thumbsup",
        PullRequestReactionContent::ThumbsDown => "thumbsdown",
        PullRequestReactionContent::Laugh => "laughing",
        PullRequestReactionContent::Hooray => "tada",
        PullRequestReactionContent::Confused => "confused",
        PullRequestReactionContent::Heart => "heart",
        PullRequestReactionContent::Rocket => "rocket",
        PullRequestReactionContent::Eyes => "eyes",
    }
}

/// The eight reactions among a subject's awards; GitLab takes any emoji, and the others have
/// no reaction to show them as.
fn reactions(awards: &Value, viewer: Option<&str>) -> Vec<PullRequestReaction> {
    PullRequestReactionContent::ALL
        .into_iter()
        .filter_map(|content| {
            let given: Vec<_> = nodes(awards)
                .filter(|award| award["name"].as_str() == Some(award_name(content)))
                .collect();
            (!given.is_empty()).then(|| PullRequestReaction {
                content,
                count: given.len() as u64,
                viewer_reacted: viewer.is_some_and(|viewer| {
                    given.iter().any(|award| {
                        award["user"]["username"]
                            .as_str()
                            .is_some_and(|user| user.eq_ignore_ascii_case(viewer))
                    })
                }),
            })
        })
        .collect()
}

/// A body with its project's uploads named in full: GitLab writes them relative to the project,
/// as `/uploads/<secret>/<file>`, which no reader outside GitLab's own page can resolve. Markdown
/// links and images, reference definitions and quoted HTML `src`/`href` attributes are
/// rewritten; text in fenced code blocks and inline code spans stays as written, since it shows
/// Markdown rather than linking. Forms not listed stay relative, and their media is not read.
pub(super) fn absolute_uploads(body: &str, authority: &str, project: &str) -> String {
    let base = format!("https://{authority}/{project}/uploads/");
    let mut out = String::with_capacity(body.len());
    // The open fence's character and length, while inside a fenced block.
    let mut fence: Option<(char, usize)> = None;
    for line in body.split_inclusive('\n') {
        match (fence, fence_of(line)) {
            (None, Some((mark, len, _))) => fence = Some((mark, len)),
            (Some((open, open_len)), Some((mark, len, bare)))
                if mark == open && len >= open_len && bare =>
            {
                fence = None
            }
            (Some(_), _) => {}
            (None, None) => {
                out.push_str(&outside_code(line, &base));
                continue;
            }
        }
        out.push_str(line);
    }
    out
}

/// A fence line's character, its length, and whether nothing follows it: up to three spaces,
/// then three or more backticks or tildes.
fn fence_of(line: &str) -> Option<(char, usize, bool)> {
    let indent = line.len() - line.trim_start_matches(' ').len();
    let rest = &line[indent..];
    let mark = rest.chars().next().filter(|c| *c == '`' || *c == '~')?;
    let len = rest.chars().take_while(|c| *c == mark).count();
    (indent <= 3 && len >= 3).then(|| (mark, len, rest[len..].trim().is_empty()))
}

/// One line with its inline code spans kept: a run of backticks closes at the next run of the
/// same length; without one, the backticks are text.
fn outside_code(line: &str, base: &str) -> String {
    let rewrite = |text: &str| {
        let mut text = text.to_owned();
        for lead in ["](", "]: ", "src=\"", "src='", "href=\"", "href='"] {
            text = text.replace(&format!("{lead}/uploads/"), &format!("{lead}{base}"));
        }
        text
    };
    let mut out = String::new();
    let mut rest = line;
    while let Some(start) = rest.find('`') {
        let run = rest[start..].bytes().take_while(|b| *b == b'`').count();
        let after = &rest[start + run..];
        let close = after.char_indices().map(|(at, _)| at).find(|&at| {
            after[at..].starts_with(&"`".repeat(run))
                && !after[at + run..].starts_with('`')
                && (at == 0 || !after[..at].ends_with('`'))
        });
        match close {
            Some(at) => {
                out.push_str(&rewrite(&rest[..start]));
                out.push_str(&rest[start..start + run + at + run]);
                rest = &after[at + run..];
            }
            None => {
                out.push_str(&rewrite(&rest[..start + run]));
                rest = after;
            }
        }
    }
    out.push_str(&rewrite(rest));
    out
}

fn comment(
    authority: &str,
    project: &str,
    note: &Value,
    viewer: Option<&str>,
) -> Option<PullRequestComment> {
    let permissions = &note["userPermissions"];
    Some(PullRequestComment {
        id: rest_id(note["id"].as_str()?)?,
        author: actor(authority, &note["author"]),
        body: absolute_uploads(&text(note, "body").unwrap_or_default(), authority, project),
        created_at: text(note, "createdAt")?,
        edited_at: edited_at(note),
        url: text(note, "url"),
        review_state: None,
        reactions: reactions(&note["awardEmoji"], viewer),
        viewer_can_update: permissions["adminNote"].as_bool() == Some(true),
        viewer_can_react: permissions["awardEmoji"].as_bool() == Some(true),
    })
}

/// A discussion's own notes, without the events GitLab writes as system notes.
fn notes(discussion: &Value) -> Vec<&Value> {
    nodes(&discussion["notes"])
        .filter(|note| note["system"].as_bool() != Some(true))
        .collect()
}

/// Where a discussion sits in the diff, when its first note sits on a line: a comment on an
/// added or unchanged line carries `newLine`, one on a removed line only `oldLine`.
fn placement(position: &Value) -> Option<(String, ReviewSide, Option<u32>, Option<String>)> {
    if position["positionType"].as_str() != Some("text") {
        return None;
    }
    let line = |field: &str| {
        position[field]
            .as_u64()
            .filter(|line| *line > 0)
            .and_then(|line| u32::try_from(line).ok())
    };
    let refs = &position["diffRefs"];
    Some(match line("newLine") {
        Some(line) => (
            text(position, "newPath")?,
            ReviewSide::New,
            Some(line),
            text(refs, "headSha"),
        ),
        None => (
            text(position, "oldPath")?,
            ReviewSide::Old,
            line("oldLine"),
            text(refs, "baseSha"),
        ),
    })
}

/// The conversation: the description, the notes outside line discussions, and each line
/// discussion as a thread named by its discussion id, which replies and resolution address.
pub(super) struct Conversation {
    pub(super) description: PullRequestComment,
    pub(super) comments: Vec<PullRequestComment>,
    pub(super) threads: Vec<PullRequestReviewThread>,
    pub(super) permissions: PullRequestPermissions,
    pub(super) labels: Vec<PullRequestLabel>,
    pub(super) reviewers: Vec<PullRequestReviewerState>,
}

pub(super) fn conversation(
    authority: &str,
    project: &str,
    mr: &Value,
    viewer: Option<&str>,
    discussions: &[Value],
) -> Option<Conversation> {
    let permissions = &mr["userPermissions"];
    let may = |field: &str| viewer.is_some() && permissions[field].as_bool() == Some(true);
    let mut comments = Vec::new();
    let mut threads = Vec::new();
    for discussion in discussions {
        let notes = notes(discussion);
        let Some(root) = notes.first() else {
            continue;
        };
        let Some((path, side, line, revision)) = placement(&root["position"]) else {
            comments.extend(
                notes
                    .iter()
                    .filter_map(|note| comment(authority, project, note, viewer)),
            );
            continue;
        };
        let thread_comments: Vec<_> = notes
            .iter()
            .filter_map(|note| comment(authority, project, note, viewer))
            .collect();
        let id = discussion["id"].as_str()?.rsplit('/').next()?.to_owned();
        threads.push(PullRequestReviewThread {
            id,
            anchor: line
                .zip(revision)
                .map(|(line, revision)| PullRequestReviewAnchor {
                    revision,
                    path: path.clone(),
                    side,
                    start_line: line,
                    end_line: line,
                }),
            path,
            resolved: discussion["resolved"].as_bool() == Some(true),
            outdated: false,
            diff_hunk: None,
            total_comments: thread_comments.len() as u64,
            comments: thread_comments,
            replies_after: None,
            viewer_can_reply: may("createNote"),
            viewer_can_resolve: viewer.is_some()
                && discussion["resolvable"].as_bool() == Some(true)
                && root["userPermissions"]["resolveNote"].as_bool() == Some(true),
        });
    }
    comments.sort_by(|left, right| left.created_at.cmp(&right.created_at));
    let description = PullRequestComment {
        id: PULL_REQUEST.into(),
        author: actor(authority, &mr["author"]),
        body: absolute_uploads(
            &text(mr, "description").unwrap_or_default(),
            authority,
            project,
        ),
        created_at: text(mr, "createdAt").unwrap_or_default(),
        // GitLab's GraphQL names no edit of a merge request's description.
        edited_at: None,
        url: text(mr, "webUrl"),
        review_state: None,
        reactions: reactions(&mr["awardEmoji"], viewer),
        viewer_can_update: may("updateMergeRequest"),
        viewer_can_react: viewer.is_some(),
    };
    let labels = nodes(&mr["labels"])
        .filter_map(|label| {
            let name = text(label, "title")?;
            Some(PullRequestLabel {
                id: name.clone(),
                name,
                color: text(label, "color").map(|color| color.trim_start_matches('#').to_owned()),
                description: text(label, "description").filter(|text| !text.is_empty()),
            })
        })
        .collect();
    let mut reviewers: Vec<PullRequestReviewerState> = nodes(&mr["reviewers"])
        .filter_map(|user| {
            Some(PullRequestReviewerState {
                reviewer: user_reviewer(rest_id(user["id"].as_str()?)?, text(user, "username")?),
                avatar_url: absolute(authority, text(user, "avatarUrl")),
                verdict: match user["mergeRequestInteraction"]["reviewState"].as_str() {
                    Some("APPROVED") => Some(PullRequestReviewState::Approved),
                    Some("REQUESTED_CHANGES") => Some(PullRequestReviewState::ChangesRequested),
                    Some("REVIEWED") => Some(PullRequestReviewState::Commented),
                    _ => None,
                },
            })
        })
        .collect();
    // Whoever approved without being asked.
    for user in nodes(&mr["approvedBy"]) {
        let (Some(id), Some(login)) = (
            user["id"].as_str().and_then(rest_id),
            text(user, "username"),
        ) else {
            continue;
        };
        if let Some(known) = reviewers
            .iter_mut()
            .find(|known| known.reviewer.login.eq_ignore_ascii_case(&login))
        {
            known.verdict = Some(PullRequestReviewState::Approved);
            continue;
        }
        reviewers.push(PullRequestReviewerState {
            reviewer: user_reviewer(id, login),
            avatar_url: absolute(authority, text(user, "avatarUrl")),
            verdict: Some(PullRequestReviewState::Approved),
        });
    }
    let verdicts = if may("createNote") {
        let mut verdicts = vec![PullRequestReviewVerdict::Comment];
        if may("canApprove") {
            verdicts.push(PullRequestReviewVerdict::Approve);
        }
        verdicts
    } else {
        Vec::new()
    };
    Some(Conversation {
        description,
        comments,
        threads,
        permissions: PullRequestPermissions {
            update: may("updateMergeRequest"),
            verdicts,
            label: may("adminMergeRequest"),
            request_reviewers: may("adminMergeRequest"),
        },
        labels,
        reviewers,
    })
}

/// A user as a reviewer write names them: by GitLab's numeric id.
pub(super) fn user_reviewer(id: String, login: String) -> PullRequestReviewer {
    PullRequestReviewer {
        id,
        login,
        kind: PullRequestReviewerKind::User,
    }
}

/// Every note but GitLab's own events, as news for a watch.
pub(super) fn remarks(discussions: &[Value]) -> Vec<PullRequestRemark> {
    discussions
        .iter()
        .flat_map(notes)
        .filter_map(|note| {
            let position = &note["position"];
            Some(PullRequestRemark {
                id: rest_id(note["id"].as_str()?)?,
                author: text(&note["author"], "username"),
                body: text(note, "body").unwrap_or_default(),
                created_at: text(note, "createdAt")?,
                edited_at: edited_at(note),
                url: text(note, "url"),
                path: text(position, "newPath").or_else(|| text(position, "oldPath")),
                review_state: None,
            })
        })
        .collect()
}

/// A file of the merge request's diffs: GitLab sends each file's hunks without a git header,
/// and none at all for a file too large to show or one it collapsed.
pub(super) fn file(row: &Value) -> Option<PullRequestFile> {
    let path = text(row, "new_path")?;
    let diff = row["diff"].as_str().unwrap_or_default();
    let hunks = diff.find("@@").map_or("", |at| &diff[at..]);
    let count = |sign: char| hunks.lines().filter(|line| line.starts_with(sign)).count() as u64;
    let flag = |field: &str| row[field].as_bool() == Some(true);
    Some(PullRequestFile {
        previous_path: text(row, "old_path").filter(|old| flag("renamed_file") && *old != path),
        kind: if flag("new_file") {
            agent::FileChangeKind::Create
        } else if flag("deleted_file") {
            agent::FileChangeKind::Delete
        } else if flag("renamed_file") {
            agent::FileChangeKind::Rename
        } else {
            agent::FileChangeKind::Modify
        },
        additions: count('+'),
        deletions: count('-'),
        patch: if diff.starts_with("Binary files ") {
            PullRequestPatch::Binary
        } else if hunks.is_empty() && flag("too_large") {
            PullRequestPatch::Oversized
        } else if hunks.is_empty() && flag("collapsed") {
            PullRequestPatch::Withheld
        } else {
            PullRequestPatch::Hunks(hunks.to_owned())
        },
        path,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The watch's conflict and the merge state come from GitLab's detailed status: a conflict
    /// is one even while GitLab rechecks, a check in progress is pending, and anything else
    /// that stops a merge blocks it rather than reading as a conflict.
    #[test]
    fn merge_status_reads_as_mergeability_and_merge_state() {
        let mr = |detailed: &str, conflicts: bool| {
            json!({"detailed_merge_status": detailed, "has_conflicts": conflicts,
                   "merge_status": "can_be_merged", "draft": false})
        };
        assert_eq!(
            mergeability(&mr("checking", true)),
            Mergeability::Conflicting
        );
        assert_eq!(mergeability(&mr("checking", false)), Mergeability::Pending);
        assert_eq!(
            mergeability(&mr("not_approved", false)),
            Mergeability::Clean
        );
        let passed = checks(&json!({"head_pipeline": {"status": "success"}}));
        let state =
            |detailed: &str| merge_state(&mr(detailed, false), Mergeability::Clean, &passed);
        assert_eq!(state("mergeable"), PullRequestMergeState::Clean);
        assert_eq!(state("need_rebase"), PullRequestMergeState::Behind);
        assert_eq!(
            state("discussions_not_resolved"),
            PullRequestMergeState::Blocked
        );
        assert_eq!(state("checking"), PullRequestMergeState::Unknown);
        let running = checks(&json!({"head_pipeline": {"status": "running"}}));
        assert_eq!(
            merge_state(&mr("mergeable", false), Mergeability::Clean, &running),
            PullRequestMergeState::Unstable
        );
    }

    /// GitLab names a note's uploads below its project; the conversation names them on the
    /// server, so the client can draw them and the media read finds them named. Absolute links,
    /// text that only mentions `/uploads/`, and code stay as written.
    #[test]
    fn uploads_are_named_on_their_server() {
        let body = "![shot](/uploads/ab12/shot.png) and [log](/uploads/cd34/log.txt)\n\n[ref]: /uploads/ef56/a.png\n<img src=\"/uploads/0a/b.png\">\nsee /uploads/ in docs, ![x](https://cdn.test/uploads/x.png)";
        assert_eq!(
            absolute_uploads(body, "code.acme.test:8443", "team/apps/web"),
            "![shot](https://code.acme.test:8443/team/apps/web/uploads/ab12/shot.png) and [log](https://code.acme.test:8443/team/apps/web/uploads/cd34/log.txt)\n\n[ref]: https://code.acme.test:8443/team/apps/web/uploads/ef56/a.png\n<img src=\"https://code.acme.test:8443/team/apps/web/uploads/0a/b.png\">\nsee /uploads/ in docs, ![x](https://cdn.test/uploads/x.png)"
        );
        // Code shows Markdown rather than linking: a fenced block and an inline span stay as
        // written, and the link after the span is still named.
        let code = "```md\n![shot](/uploads/ab12/shot.png)\n```\n~~~~\n[ref]: /uploads/ef56/a.png\n~~~~\nwrite `![x](/uploads/a/x.png)` for ![x](/uploads/a/x.png), `bärenhunger`";
        assert_eq!(
            absolute_uploads(code, "gitlab.com", "a/b"),
            "```md\n![shot](/uploads/ab12/shot.png)\n```\n~~~~\n[ref]: /uploads/ef56/a.png\n~~~~\nwrite `![x](/uploads/a/x.png)` for ![x](https://gitlab.com/a/b/uploads/a/x.png), `bärenhunger`"
        );
    }

    /// The head pipeline is the merge request's one check, and one waiting on a manual job or
    /// a schedule has not passed: it is pending, like everything not finished.
    #[test]
    fn a_pipeline_reads_as_one_check() {
        assert_eq!(pipeline_status("failed"), CheckStatus::Failure);
        assert!(pipeline_status("canceled").failed());
        assert_eq!(pipeline_status("manual"), CheckStatus::Pending);
        assert_eq!(pipeline_status("scheduled"), CheckStatus::Pending);
        assert_eq!(
            pipeline_status("waiting_for_resource"),
            CheckStatus::Pending
        );
        assert_eq!(checks_state(&checks(&json!({}))), None);
        assert_eq!(
            checks_state(&checks(&json!({"head_pipeline": {"status": "manual"}}))),
            Some(ChecksState::Pending)
        );
    }
}
