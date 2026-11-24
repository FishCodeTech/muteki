//! What Tcode reads of a Bitbucket Cloud pull request, in Tcode's terms. Users carry no
//! username on Bitbucket: an account shows by its nickname, and a write names it by its uuid.

use super::api::{Api, PAGE_LIMIT, Request, error};
use crate::forge::{ForgeError, ForgeErrorKind};
use serde_json::Value;
use std::collections::HashMap;
use tcode_core::{
    pull_request::{
        ChecksState, Mergeability, PullRequestAuthor, PullRequestKey, PullRequestSnapshot,
        PullRequestState,
    },
    pull_request_watch::{CheckStatus, PullRequestCheck, PullRequestRemark},
    session::ReviewSide,
};
use tcode_protocol::{
    PullRequestActor, PullRequestCapabilities, PullRequestComment, PullRequestMergeState,
    PullRequestPermissions, PullRequestReviewAnchor, PullRequestReviewState,
    PullRequestReviewThread, PullRequestReviewVerdict, PullRequestReviewer,
    PullRequestReviewerKind, PullRequestReviewerState,
};

/// Past this many pages a list is reported incomplete rather than read on.
pub(super) const MAX_PAGES: usize = 10;
/// The pull request itself, as edits name it.
pub(super) const PULL_REQUEST: &str = "pr";
/// What a comment read carries: not the rendered HTML, which is most of every comment.
const COMMENT_FIELDS: &str = "values.id,values.parent.id,values.inline,values.content.raw,values.user.uuid,values.user.nickname,values.user.display_name,values.user.links.avatar.href,values.created_on,values.updated_on,values.deleted,values.pending,values.resolution.created_on,values.links.html.href,values.links.code.href";

pub(super) fn capabilities() -> PullRequestCapabilities {
    PullRequestCapabilities {
        reply: true,
        resolve: true,
        reactions: false,
        labels: false,
        request_changes: true,
        draft: true,
        // A declined pull request stays declined.
        reopen: false,
        auto_merge: false,
        update_branch: false,
        update_merge: false,
        revert: false,
        host_viewed_marks: false,
        // A merge takes a message, but no read gives the one Bitbucket would write, so there
        // is none to clean of agents' credits.
        merge_message: false,
    }
}

/// A pull request's place on Bitbucket and the requests that read it.
pub(super) struct Pr<'a> {
    pub(super) api: &'a Api,
    pub(super) key: &'a PullRequestKey,
}

impl Pr<'_> {
    /// Below the repository.
    pub(super) fn repository_path(&self, rest: &str) -> String {
        format!("/repositories/{}{rest}", self.key.repository)
    }
    /// Below the pull request.
    pub(super) fn path(&self, rest: &str) -> String {
        self.repository_path(&format!("/pullrequests/{}{rest}", self.key.number))
    }
    pub(super) fn get(&self, path: String, operation: &'static str) -> Result<Value, ForgeError> {
        self.api.send(Request::get(path, operation))?.json()
    }
    pub(super) fn pr(&self) -> Result<Value, ForgeError> {
        let pr = self.get(self.path("?fields=-rendered"), "PullRequest")?;
        if pr["id"].as_u64() != Some(self.key.number) {
            return Err(error(
                ForgeErrorKind::Uncertain,
                "Bitbucket answered another pull request",
            ));
        }
        Ok(pr)
    }
    /// The build statuses of `head`, one page of them.
    pub(super) fn checks(&self, head: &str) -> Result<Vec<PullRequestCheck>, ForgeError> {
        if !head.bytes().all(|b| b.is_ascii_hexdigit()) || head.is_empty() {
            return Ok(Vec::new());
        }
        let page = self.get(
            self.repository_path(&format!(
                "/commit/{head}/statuses?pagelen={PAGE_LIMIT}&fields=values.key,values.name,values.state,values.url"
            )),
            "Statuses",
        )?;
        Ok(checks(&page))
    }
    /// Whether the source merges into the destination.
    pub(super) fn conflicts(&self) -> Result<Mergeability, ForgeError> {
        Ok(conflicts(&self.get(self.path("/conflicts"), "Conflicts")?))
    }
    /// Every comment, page by page up to [`MAX_PAGES`]; `false` when more remained.
    pub(super) fn comments(&self) -> Result<(Vec<Value>, bool), ForgeError> {
        let (rows, next) = self.api.list(
            &self.path(&format!(
                "/comments?pagelen={PAGE_LIMIT}&fields={COMMENT_FIELDS},next"
            )),
            "Comments",
            MAX_PAGES,
        )?;
        Ok((rows, next.is_none()))
    }
}

pub(super) fn text(raw: &Value, field: &str) -> Option<String> {
    raw[field].as_str().map(str::to_owned)
}

pub(super) fn head(pr: &Value) -> Option<String> {
    text(&pr["source"]["commit"], "hash")
}

pub(super) fn state(pr: &Value) -> Option<PullRequestState> {
    match pr["state"].as_str()? {
        "OPEN" => Some(PullRequestState::Open),
        "MERGED" => Some(PullRequestState::Merged),
        // A superseded pull request was closed for another.
        "DECLINED" | "SUPERSEDED" => Some(PullRequestState::Closed),
        _ => None,
    }
}

fn check_status(state: &str) -> CheckStatus {
    match state {
        "SUCCESSFUL" => CheckStatus::Success,
        "FAILED" => CheckStatus::Failure,
        "STOPPED" => CheckStatus::Cancelled,
        "INPROGRESS" => CheckStatus::Pending,
        _ => CheckStatus::Neutral,
    }
}

/// A statuses page as checks, one per key: a rerun keeps its key, and the later status of it
/// is the one that holds. Two pipelines of one name stay two.
pub(super) fn checks(page: &Value) -> Vec<PullRequestCheck> {
    let mut checks: Vec<(String, PullRequestCheck)> = Vec::new();
    for status in page["values"].as_array().into_iter().flatten() {
        let Some(name) = text(status, "name")
            .filter(|name| !name.is_empty())
            .or_else(|| text(status, "key"))
        else {
            continue;
        };
        let key = text(status, "key").unwrap_or_else(|| name.clone());
        let check = PullRequestCheck {
            name,
            status: check_status(status["state"].as_str().unwrap_or_default()),
            url: text(status, "url"),
            required: None,
        };
        match checks.iter_mut().find(|(held, _)| *held == key) {
            Some((_, held)) => *held = check,
            None => checks.push((key, check)),
        }
    }
    checks.into_iter().map(|(_, check)| check).collect()
}

pub(super) fn checks_state(checks: &[PullRequestCheck]) -> Option<ChecksState> {
    if checks.is_empty() {
        None
    } else if checks.iter().any(|check| check.status.failed()) {
        Some(ChecksState::Failing)
    } else if checks
        .iter()
        .any(|check| check.status == CheckStatus::Pending)
    {
        Some(ChecksState::Pending)
    } else {
        Some(ChecksState::Passing)
    }
}

/// A `/conflicts` page lists each conflicting path, so an empty one is clean.
pub(super) fn conflicts(page: &Value) -> Mergeability {
    match page["values"].as_array() {
        Some(paths) if paths.is_empty() => Mergeability::Clean,
        Some(_) => Mergeability::Conflicting,
        None => Mergeability::Unknown,
    }
}

/// What merging now would meet. Bitbucket's merge checks (approvals, tasks) have no read, so a
/// pull request they hold back reads clean, and the merge answers with the check it failed.
pub(super) fn merge_state(
    pr: &Value,
    mergeability: Mergeability,
    checks: &[PullRequestCheck],
) -> PullRequestMergeState {
    if pr["draft"].as_bool() == Some(true) {
        PullRequestMergeState::Draft
    } else if state(pr) != Some(PullRequestState::Open) {
        PullRequestMergeState::Unknown
    } else {
        match mergeability {
            Mergeability::Conflicting => PullRequestMergeState::Dirty,
            Mergeability::Pending | Mergeability::Unknown => PullRequestMergeState::Unknown,
            Mergeability::Clean
                if checks
                    .iter()
                    .any(|check| check.status != CheckStatus::Success) =>
            {
                PullRequestMergeState::Unstable
            }
            Mergeability::Clean => PullRequestMergeState::Clean,
        }
    }
}

/// An account as Tcode shows it: by nickname, else its display name.
pub(super) fn login(user: &Value) -> Option<String> {
    text(user, "nickname")
        .or_else(|| text(user, "display_name"))
        .filter(|login| !login.is_empty())
}

pub(super) fn actor(user: &Value) -> Option<PullRequestActor> {
    Some(PullRequestActor {
        login: login(user)?,
        avatar_url: text(&user["links"]["avatar"], "href"),
    })
}

/// The pull request from one read; `checks` are the head's build statuses. Bitbucket keeps no
/// closing time, so a closed or merged pull request's last update stands for it.
pub(super) fn snapshot(
    pr: &Value,
    checks: &[PullRequestCheck],
    synced_at: u64,
) -> Option<PullRequestSnapshot> {
    let state = state(pr)?;
    let updated_at = text(pr, "updated_on")?;
    let ended_at = (state != PullRequestState::Open).then(|| updated_at.clone());
    Some(PullRequestSnapshot {
        state,
        title: text(pr, "title")?,
        head_branch: text(&pr["source"]["branch"], "name")?,
        base_branch: text(&pr["destination"]["branch"], "name")?,
        is_draft: pr["draft"].as_bool().unwrap_or(false),
        updated_at,
        synced_at,
        closed_at: ended_at.clone(),
        merged_at: ended_at.filter(|_| state == PullRequestState::Merged),
        author: login(&pr["author"]).map(|login| PullRequestAuthor {
            login,
            avatar_url: text(&pr["author"]["links"]["avatar"], "href"),
        }),
        // Only the diffstat counts lines, one more read per file page.
        additions: None,
        deletions: None,
        changed_files: None,
        review_decision: None,
        checks_state: checks_state(checks),
        // Conflicts are a read of their own, which the summary leaves to the detail.
        mergeability: Mergeability::Unknown,
    })
}

/// The second a timestamp names, which Bitbucket writes to the microsecond.
fn second(at: &str) -> &str {
    at.get(..19).unwrap_or(at)
}

/// When the text changed after it was written. Resolving a thread stamps `updated_on` too, at
/// the resolution's own time.
fn edited_at(comment: &Value) -> Option<String> {
    let created = comment["created_on"].as_str()?;
    let updated = comment["updated_on"].as_str()?;
    let resolved = comment["resolution"]["created_on"].as_str();
    (second(updated) != second(created) && resolved.map(second) != Some(second(updated)))
        .then(|| updated.to_owned())
}

/// A comment that shows: not deleted, and not one of the viewer's pending review comments.
fn shown(comment: &Value) -> bool {
    comment["deleted"].as_bool() != Some(true) && comment["pending"].as_bool() != Some(true)
}

fn comment(raw: &Value, viewer: Option<&str>) -> Option<PullRequestComment> {
    Some(PullRequestComment {
        id: raw["id"].as_u64()?.to_string(),
        author: actor(&raw["user"]),
        body: text(&raw["content"], "raw").unwrap_or_default(),
        created_at: text(raw, "created_on")?,
        edited_at: edited_at(raw),
        url: text(&raw["links"]["html"], "href"),
        review_state: None,
        reactions: Vec::new(),
        viewer_can_update: viewer.is_some() && raw["user"]["uuid"].as_str() == viewer,
        viewer_can_react: false,
    })
}

/// The revisions a line comment's code link compares, `…/diff/ws/repo:{new}..{old}?path=…`:
/// the ones Bitbucket shows the comment against.
fn compared(raw: &Value) -> Option<(String, String)> {
    let link = raw["links"]["code"]["href"].as_str()?;
    let spec = link.split('?').next()?.rsplit(':').next()?;
    let (new, old) = spec.split_once("..")?;
    let hex =
        |revision: &str| !revision.is_empty() && revision.bytes().all(|b| b.is_ascii_hexdigit());
    (hex(new) && hex(old)).then(|| (new.to_owned(), old.to_owned()))
}

/// Where a line comment sits: `to` is a line on the new side, `from` one on the old side, each
/// with a `start_` line when it spans several.
fn anchor(raw: &Value) -> Option<PullRequestReviewAnchor> {
    let inline = &raw["inline"];
    let line = |field: &str| {
        inline[field]
            .as_u64()
            .filter(|line| *line > 0)
            .and_then(|line| u32::try_from(line).ok())
    };
    let (new, old) = compared(raw)?;
    let (side, end, start, revision) = match line("to") {
        Some(end) => (ReviewSide::New, end, line("start_to"), new),
        None => (ReviewSide::Old, line("from")?, line("start_from"), old),
    };
    Some(PullRequestReviewAnchor {
        revision,
        path: text(inline, "path")?,
        side,
        start_line: start.filter(|start| *start <= end).unwrap_or(end),
        end_line: end,
    })
}

/// The comment each one answers at the top of its chain, so a reply to a reply joins the
/// thread its first comment opened. A chain that runs past a comment the read lacks ends there.
fn roots(comments: &[Value]) -> HashMap<u64, u64> {
    let parents: HashMap<u64, u64> = comments
        .iter()
        .filter_map(|comment| Some((comment["id"].as_u64()?, comment["parent"]["id"].as_u64()?)))
        .collect();
    let known: std::collections::HashSet<u64> = comments
        .iter()
        .filter_map(|comment| comment["id"].as_u64())
        .collect();
    known
        .iter()
        .map(|id| {
            let mut root = *id;
            for _ in 0..comments.len() {
                match parents.get(&root) {
                    Some(parent) if known.contains(parent) => root = *parent,
                    _ => break,
                }
            }
            (*id, root)
        })
        .collect()
}

pub(super) struct Conversation {
    pub(super) description: PullRequestComment,
    pub(super) comments: Vec<PullRequestComment>,
    pub(super) threads: Vec<PullRequestReviewThread>,
    pub(super) permissions: PullRequestPermissions,
    pub(super) reviewers: Vec<PullRequestReviewerState>,
}

/// The conversation: the description, the comments outside line threads, oldest first, and
/// each line comment with its replies as a thread named by the first comment's id, which
/// replies and resolution address. `viewer` is the reading account's uuid; `signed_in` is
/// whether any credential reads, which Bitbucket gives no permissions for beyond the request.
pub(super) fn conversation(
    pr: &Value,
    raw: &[Value],
    viewer: Option<&str>,
    signed_in: bool,
) -> Conversation {
    let roots = roots(raw);
    let by_id: HashMap<u64, &Value> = raw
        .iter()
        .filter_map(|comment| Some((comment["id"].as_u64()?, comment)))
        .collect();
    let mut comments = Vec::new();
    let mut threads: Vec<PullRequestReviewThread> = Vec::new();
    for item in raw {
        let Some(id) = item["id"].as_u64() else {
            continue;
        };
        let root_id = roots.get(&id).copied().unwrap_or(id);
        let root = by_id.get(&root_id).copied().unwrap_or(item);
        if !root["inline"].is_object() {
            if shown(item) {
                comments.extend(comment(item, viewer));
            }
            continue;
        }
        let thread_id = root_id.to_string();
        let index = match threads.iter().position(|thread| thread.id == thread_id) {
            Some(index) => index,
            None => {
                threads.push(PullRequestReviewThread {
                    id: thread_id,
                    path: text(&root["inline"], "path").unwrap_or_default(),
                    resolved: root["resolution"].is_object(),
                    outdated: false,
                    anchor: anchor(root),
                    diff_hunk: None,
                    comments: Vec::new(),
                    total_comments: 0,
                    replies_after: None,
                    viewer_can_reply: signed_in,
                    viewer_can_resolve: signed_in,
                });
                threads.len() - 1
            }
        };
        if shown(item)
            && let Some(comment) = comment(item, viewer)
        {
            threads[index].comments.push(comment);
            threads[index].total_comments += 1;
        }
    }
    threads.retain(|thread| !thread.comments.is_empty());
    for thread in &mut threads {
        thread
            .comments
            .sort_by(|left, right| left.created_at.cmp(&right.created_at));
    }
    comments.sort_by(|left, right| left.created_at.cmp(&right.created_at));
    let author = pr["author"]["uuid"].as_str();
    let description = PullRequestComment {
        id: PULL_REQUEST.into(),
        author: actor(&pr["author"]),
        body: text(pr, "description").unwrap_or_default(),
        created_at: text(pr, "created_on").unwrap_or_default(),
        // `updated_on` moves with every push, comment and vote, so it names no edit.
        edited_at: None,
        url: text(&pr["links"]["html"], "href"),
        review_state: None,
        reactions: Vec::new(),
        viewer_can_update: signed_in,
        viewer_can_react: false,
    };
    // Bitbucket refuses a vote on one's own pull request.
    let verdicts = match (signed_in, viewer.is_some() && viewer == author) {
        (false, _) => Vec::new(),
        (true, true) => vec![PullRequestReviewVerdict::Comment],
        (true, false) => vec![
            PullRequestReviewVerdict::Comment,
            PullRequestReviewVerdict::Approve,
            PullRequestReviewVerdict::RequestChanges,
        ],
    };
    Conversation {
        description,
        comments,
        threads,
        permissions: PullRequestPermissions {
            update: signed_in,
            verdicts,
            label: false,
            request_reviewers: signed_in,
        },
        reviewers: reviewers(pr),
    }
}

/// A participant's vote, the nearest Bitbucket has to a review.
fn vote(participant: &Value) -> Option<PullRequestReviewState> {
    match participant["state"].as_str() {
        Some("approved") => Some(PullRequestReviewState::Approved),
        Some("changes_requested") => Some(PullRequestReviewState::ChangesRequested),
        _ => (participant["approved"].as_bool() == Some(true))
            .then_some(PullRequestReviewState::Approved),
    }
}

pub(super) fn user_reviewer(user: &Value) -> Option<PullRequestReviewer> {
    Some(PullRequestReviewer {
        id: text(user, "uuid")?,
        login: login(user)?,
        kind: PullRequestReviewerKind::User,
    })
}

/// The reviewers asked, each with their vote, then whoever voted without being asked.
pub(super) fn reviewers(pr: &Value) -> Vec<PullRequestReviewerState> {
    let participants: Vec<&Value> = pr["participants"]
        .as_array()
        .into_iter()
        .flatten()
        .collect();
    let vote_of = |uuid: &str| {
        participants
            .iter()
            .find(|participant| participant["user"]["uuid"].as_str() == Some(uuid))
            .and_then(|participant| vote(participant))
    };
    let mut states: Vec<PullRequestReviewerState> = pr["reviewers"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|user| {
            let reviewer = user_reviewer(user)?;
            Some(PullRequestReviewerState {
                verdict: vote_of(&reviewer.id),
                avatar_url: text(&user["links"]["avatar"], "href"),
                reviewer,
            })
        })
        .collect();
    for participant in &participants {
        let (Some(verdict), Some(reviewer)) =
            (vote(participant), user_reviewer(&participant["user"]))
        else {
            continue;
        };
        if states.iter().any(|known| known.reviewer.id == reviewer.id) {
            continue;
        }
        states.push(PullRequestReviewerState {
            reviewer,
            avatar_url: text(&participant["user"]["links"]["avatar"], "href"),
            verdict: Some(verdict),
        });
    }
    states
}

fn remark(raw: &Value) -> Option<PullRequestRemark> {
    shown(raw).then_some(())?;
    Some(PullRequestRemark {
        id: raw["id"].as_u64()?.to_string(),
        author: login(&raw["user"]),
        body: text(&raw["content"], "raw").unwrap_or_default(),
        created_at: text(raw, "created_on")?,
        edited_at: edited_at(raw),
        url: text(&raw["links"]["html"], "href"),
        path: text(&raw["inline"], "path"),
        review_state: None,
    })
}

/// A vote as a remark, named by who cast it and when, since Bitbucket gives it no id.
fn vote_remark(raw: &Value, state: &str) -> Option<PullRequestRemark> {
    let at = text(raw, "date")?;
    Some(PullRequestRemark {
        id: format!("{state}:{}:{at}", raw["user"]["uuid"].as_str()?),
        author: login(&raw["user"]),
        body: String::new(),
        created_at: at,
        edited_at: None,
        url: None,
        path: None,
        review_state: Some(state.to_owned()),
    })
}

/// The comments and votes among a pull request's activity, as news for a watch.
pub(super) fn remarks(activity: &[Value]) -> Vec<PullRequestRemark> {
    activity
        .iter()
        .filter_map(|entry| {
            if entry["comment"].is_object() {
                remark(&entry["comment"])
            } else if entry["approval"].is_object() {
                vote_remark(&entry["approval"], "APPROVED")
            } else if entry["changes_requested"].is_object() {
                vote_remark(&entry["changes_requested"], "CHANGES_REQUESTED")
            } else {
                None
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A head's statuses are its checks, one per key with the later status of a rerun holding,
    /// and the state the snapshot carries follows them: a stopped build failed, a running one
    /// is pending, and none at all is no state.
    #[test]
    fn build_statuses_read_as_checks() {
        let page = json!({"values": [
            {"key": "a", "name": "Pipeline #1", "state": "FAILED", "url": "https://bitbucket.org/x"},
            {"key": "b", "name": "Pipeline #1", "state": "INPROGRESS"},
            {"key": "a", "name": "Pipeline #2", "state": "SUCCESSFUL"},
        ]});
        let read = checks(&page);
        assert_eq!(
            read.iter()
                .map(|check| (check.name.as_str(), check.status))
                .collect::<Vec<_>>(),
            [
                ("Pipeline #2", CheckStatus::Success),
                ("Pipeline #1", CheckStatus::Pending)
            ]
        );
        assert_eq!(checks_state(&read), Some(ChecksState::Pending));
        let stopped =
            checks(&json!({"values": [{"key": "a", "name": "Build", "state": "STOPPED"}]}));
        assert_eq!(checks_state(&stopped), Some(ChecksState::Failing));
        assert_eq!(checks_state(&checks(&json!({"values": []}))), None);
    }

    /// A listed conflicting path is a conflict, an empty list is clean, and a page without a
    /// list says nothing.
    #[test]
    fn a_conflicts_page_reads_as_mergeability() {
        assert_eq!(
            conflicts(&json!({"values": [{"path": "src/lib.rs"}]})),
            Mergeability::Conflicting
        );
        assert_eq!(conflicts(&json!({"values": []})), Mergeability::Clean);
        assert_eq!(conflicts(&json!({})), Mergeability::Unknown);
    }

    /// Replies join the thread their first line comment opened, however deep, and a reply to
    /// a remark outside the diff stays a remark; the thread is anchored on the side its line
    /// is on, at the revision Bitbucket compares it at, and resolving is no edit.
    #[test]
    fn line_comments_and_their_replies_are_one_thread() {
        let code = json!({"code": {"href": "https://api.bitbucket.org/2.0/repositories/a/b/diff/a/b:d0a06076aec9..b58dfbb281c8?path=src%2Flib.rs"}});
        let at = |second: u32| format!("2026-10-01T00:00:{second:02}.000001+00:00");
        let user = json!({"uuid": "{u1}", "nickname": "ana"});
        let raw = vec![
            json!({"id": 1, "inline": {"path": "src/lib.rs", "to": 12, "from": null, "start_to": 10},
                   "content": {"raw": "why?"}, "user": user, "created_on": at(1), "updated_on": at(9),
                   "resolution": {"created_on": at(9)}, "links": code}),
            json!({"id": 2, "parent": {"id": 1}, "inline": {"path": "src/lib.rs", "to": 12},
                   "content": {"raw": "because"}, "user": {"uuid": "{u2}", "nickname": "bo"},
                   "created_on": at(2), "updated_on": at(3), "links": code}),
            json!({"id": 3, "parent": {"id": 2}, "inline": {"path": "src/lib.rs", "to": 12},
                   "content": {"raw": ""}, "deleted": true, "user": user,
                   "created_on": at(4), "updated_on": at(4), "links": code}),
            json!({"id": 4, "content": {"raw": "ship it"}, "user": user,
                   "created_on": at(5), "updated_on": at(5)}),
            json!({"id": 5, "parent": {"id": 4}, "content": {"raw": "thanks"}, "user": user,
                   "created_on": at(6), "updated_on": at(6)}),
        ];
        let pr = json!({"author": {"uuid": "{u1}"}});
        let read = conversation(&pr, &raw, Some("{u1}"), true);
        assert_eq!(read.threads.len(), 1);
        let thread = &read.threads[0];
        assert_eq!(thread.id, "1");
        assert!(thread.resolved);
        assert_eq!(
            thread.anchor,
            Some(PullRequestReviewAnchor {
                revision: "d0a06076aec9".into(),
                path: "src/lib.rs".into(),
                side: ReviewSide::New,
                start_line: 10,
                end_line: 12,
            })
        );
        assert_eq!(
            thread
                .comments
                .iter()
                .map(|comment| (comment.id.as_str(), comment.edited_at.is_some()))
                .collect::<Vec<_>>(),
            [("1", false), ("2", true)]
        );
        assert!(thread.comments[0].viewer_can_update && !thread.comments[1].viewer_can_update);
        assert_eq!(
            read.comments
                .iter()
                .map(|comment| comment.body.as_str())
                .collect::<Vec<_>>(),
            ["ship it", "thanks"]
        );
        // The author may only comment on their own pull request.
        assert_eq!(
            read.permissions.verdicts,
            [PullRequestReviewVerdict::Comment]
        );
    }
}
