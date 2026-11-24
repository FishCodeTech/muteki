//! What Tcode reads of an Azure DevOps pull request, in Tcode's terms. An account shows by its
//! unique name (its sign-in), or its display name where Azure withholds that, as it does from
//! anonymous reads; a write names it by its identity id.

use super::{
    api::{Api, PREVIEW, Request, error},
    repository,
};
use crate::forge::{ForgeError, ForgeErrorKind};
use serde_json::Value;
use std::collections::HashMap;
use tcode_core::{
    pull_request::{
        ChecksState, Mergeability, PullRequestAuthor, PullRequestKey, PullRequestMergeMethod,
        PullRequestSnapshot, PullRequestState, ReviewDecision,
    },
    pull_request_watch::{CheckStatus, PullRequestCheck, PullRequestRemark},
    session::ReviewSide,
};
use tcode_protocol::{
    PullRequestActor, PullRequestCapabilities, PullRequestComment, PullRequestLabel,
    PullRequestMergeState, PullRequestPermissions, PullRequestReviewAnchor, PullRequestReviewState,
    PullRequestReviewThread, PullRequestReviewVerdict, PullRequestReviewer,
    PullRequestReviewerKind, PullRequestReviewerState,
};

/// The pull request itself, as edits name it.
pub(super) const PULL_REQUEST: &str = "pr";

pub(super) fn capabilities() -> PullRequestCapabilities {
    PullRequestCapabilities {
        reply: true,
        // A thread's status: active, or fixed once resolved.
        resolve: true,
        // Azure has likes on comments, not reactions.
        reactions: false,
        labels: true,
        request_changes: true,
        draft: true,
        // An abandoned pull request is reactivated.
        reopen: true,
        // Auto-complete, which Azure reads back and cancels.
        auto_merge: true,
        update_branch: false,
        update_merge: false,
        revert: false,
        host_viewed_marks: false,
        merge_message: true,
    }
}

/// A pull request's place on Azure DevOps and the requests that read it.
pub(super) struct Pr<'a> {
    pub(super) api: &'a Api,
    pub(super) key: &'a PullRequestKey,
}

impl Pr<'_> {
    pub(super) fn authority(&self) -> &str {
        &self.key.host
    }
    /// Below the project.
    pub(super) fn project_path(&self, rest: &str) -> String {
        let (project, _) = repository::names(self.key);
        format!(
            "/{}{rest}",
            crate::github::pull_request_reads::percent_encode(project)
        )
    }
    /// Below the repository.
    pub(super) fn repository_path(&self, rest: &str) -> String {
        let (_, name) = repository::names(self.key);
        self.project_path(&format!(
            "/_apis/git/repositories/{}{rest}",
            crate::github::pull_request_reads::percent_encode(name)
        ))
    }
    /// Below the pull request.
    pub(super) fn path(&self, rest: &str) -> String {
        self.repository_path(&format!("/pullRequests/{}{rest}", self.key.number))
    }
    pub(super) fn get(&self, path: String, operation: &'static str) -> Result<Value, ForgeError> {
        self.api
            .send(self.authority(), Request::get(path, operation))?
            .json()
    }
    pub(super) fn pr(&self) -> Result<Value, ForgeError> {
        let pr = self.get(self.path(""), "PullRequest")?;
        if pr["pullRequestId"].as_u64() != Some(self.key.number) {
            return Err(error(
                ForgeErrorKind::Uncertain,
                "Azure DevOps answered another pull request",
            ));
        }
        Ok(pr)
    }
    /// The account the credential reads as; `None` anonymously.
    pub(super) fn viewer(&self) -> Result<Option<Value>, ForgeError> {
        self.api.viewer(self.authority())
    }
    /// Every thread, which Azure answers unpaged.
    pub(super) fn threads(&self) -> Result<Vec<Value>, ForgeError> {
        Ok(self.get(self.path("/threads"), "Threads")?["value"]
            .as_array()
            .cloned()
            .unwrap_or_default())
    }
    /// Every push, oldest first.
    pub(super) fn iterations(&self) -> Result<Vec<Value>, ForgeError> {
        Ok(self.get(self.path("/iterations"), "Iterations")?["value"]
            .as_array()
            .cloned()
            .unwrap_or_default())
    }
    pub(super) fn labels(&self) -> Result<Vec<PullRequestLabel>, ForgeError> {
        let read: Value = self
            .api
            .send(
                self.authority(),
                Request {
                    version: PREVIEW,
                    ..Request::get(self.path("/labels"), "Labels")
                },
            )?
            .json()?;
        Ok(read["value"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|label| label["active"].as_bool() != Some(false))
            .filter_map(|label| {
                let name = text(label, "name")?;
                Some(PullRequestLabel {
                    id: name.clone(),
                    name,
                    color: None,
                    description: None,
                })
            })
            .collect())
    }
    /// The statuses posted to the pull request and its branch policies' evaluations, as checks.
    /// Azure keeps both from anonymous reads, which then have none.
    pub(super) fn checks(&self, pr: &Value) -> Result<Vec<PullRequestCheck>, ForgeError> {
        let statuses = self.api.send(
            self.authority(),
            Request {
                version: PREVIEW,
                ..Request::get(self.path("/statuses"), "Statuses")
            },
        );
        let statuses: Value = match statuses {
            Ok(response) => response.json()?,
            Err(ForgeError {
                kind: ForgeErrorKind::NoCredential,
                ..
            }) => return Ok(Vec::new()),
            Err(failure) => return Err(failure),
        };
        let mut checks = status_checks(&statuses);
        let Some(project) = pr["repository"]["project"]["id"].as_str() else {
            return Ok(checks);
        };
        let artifact = crate::github::pull_request_reads::percent_encode(&format!(
            "vstfs:///CodeReview/CodeReviewId/{project}/{}",
            self.key.number
        ));
        let evaluations: Value = self
            .api
            .send(
                self.authority(),
                Request {
                    version: PREVIEW,
                    ..Request::get(
                        self.project_path(&format!(
                            "/_apis/policy/evaluations?artifactId={artifact}"
                        )),
                        "PolicyEvaluations",
                    )
                },
            )?
            .json()?;
        checks.extend(policy_checks(&evaluations));
        Ok(checks)
    }
}

pub(super) fn text(raw: &Value, field: &str) -> Option<String> {
    raw[field].as_str().map(str::to_owned)
}

/// The commit the pull request's source branch was last merged from.
pub(super) fn head(pr: &Value) -> Option<String> {
    text(&pr["lastMergeSourceCommit"], "commitId")
}

pub(super) fn state(pr: &Value) -> Option<PullRequestState> {
    match pr["status"].as_str()? {
        "active" => Some(PullRequestState::Open),
        "completed" => Some(PullRequestState::Merged),
        "abandoned" => Some(PullRequestState::Closed),
        _ => None,
    }
}

/// A branch name without `refs/heads/`.
pub(super) fn branch(reference: &str) -> String {
    reference
        .strip_prefix("refs/heads/")
        .unwrap_or(reference)
        .to_owned()
}

/// Azure's trial merge of the source into the target. A policy that rejects the pull request
/// and a merge that failed for another reason are no conflict; one queued or not yet set is
/// still to run.
pub(super) fn mergeability(pr: &Value) -> Mergeability {
    match pr["mergeStatus"].as_str() {
        Some("succeeded") => Mergeability::Clean,
        Some("conflicts") => Mergeability::Conflicting,
        Some("queued" | "notSet") => Mergeability::Pending,
        _ => Mergeability::Unknown,
    }
}

/// The strategy auto-complete or a completion was set with.
pub(super) fn merge_method(strategy: &str) -> Option<PullRequestMergeMethod> {
    match strategy {
        "noFastForward" => Some(PullRequestMergeMethod::Merge),
        "squash" => Some(PullRequestMergeMethod::Squash),
        "rebase" => Some(PullRequestMergeMethod::Rebase),
        _ => None,
    }
}

pub(super) fn merge_strategy(method: PullRequestMergeMethod) -> &'static str {
    match method {
        PullRequestMergeMethod::Merge => "noFastForward",
        PullRequestMergeMethod::Squash => "squash",
        // Rebased onto the target and fast-forwarded, the linear history GitHub's rebase
        // merge writes.
        PullRequestMergeMethod::Rebase => "rebase",
    }
}

/// What auto-complete is armed with, when someone set it.
pub(super) fn auto_complete(pr: &Value) -> Option<PullRequestMergeMethod> {
    pr["autoCompleteSetBy"]["id"].as_str()?;
    let options = &pr["completionOptions"];
    Some(
        options["mergeStrategy"]
            .as_str()
            .and_then(merge_method)
            .unwrap_or(if options["squashMerge"].as_bool() == Some(true) {
                PullRequestMergeMethod::Squash
            } else {
                PullRequestMergeMethod::Merge
            }),
    )
}

/// Each status context's latest state: a later post to the same context replaces the earlier.
pub(super) fn status_checks(statuses: &Value) -> Vec<PullRequestCheck> {
    let mut checks: Vec<(String, PullRequestCheck)> = Vec::new();
    for status in statuses["value"].as_array().into_iter().flatten() {
        let context = &status["context"];
        let Some(name) = text(context, "name") else {
            continue;
        };
        let name = match text(context, "genre").filter(|genre| !genre.is_empty()) {
            Some(genre) => format!("{genre}/{name}"),
            None => name,
        };
        let check = PullRequestCheck {
            status: match status["state"].as_str().unwrap_or_default() {
                "succeeded" => CheckStatus::Success,
                "failed" | "error" => CheckStatus::Failure,
                "pending" => CheckStatus::Pending,
                "notApplicable" => CheckStatus::Skipped,
                _ => CheckStatus::Neutral,
            },
            url: text(status, "targetUrl"),
            required: None,
            name: name.clone(),
        };
        match checks.iter_mut().find(|(held, _)| *held == name) {
            Some((_, held)) => *held = check,
            None => checks.push((name, check)),
        }
    }
    checks.into_iter().map(|(_, check)| check).collect()
}

/// Each branch policy's evaluation, required when the policy blocks completion.
pub(super) fn policy_checks(evaluations: &Value) -> Vec<PullRequestCheck> {
    evaluations["value"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|evaluation| evaluation["configuration"]["isEnabled"].as_bool() != Some(false))
        .filter_map(|evaluation| {
            let configuration = &evaluation["configuration"];
            let name = text(&configuration["settings"], "displayName")
                .filter(|name| !name.is_empty())
                .or_else(|| text(&configuration["type"], "displayName"))?;
            Some(PullRequestCheck {
                name,
                status: match evaluation["status"].as_str().unwrap_or_default() {
                    "approved" => CheckStatus::Success,
                    "rejected" | "broken" => CheckStatus::Failure,
                    "queued" | "running" => CheckStatus::Pending,
                    "notApplicable" => CheckStatus::Skipped,
                    _ => CheckStatus::Neutral,
                },
                url: None,
                required: configuration["isBlocking"].as_bool(),
            })
        })
        .collect()
}

pub(super) fn checks_state(checks: &[PullRequestCheck]) -> Option<ChecksState> {
    let counted: Vec<_> = checks
        .iter()
        .filter(|check| check.status != CheckStatus::Skipped)
        .collect();
    if counted.is_empty() {
        None
    } else if counted.iter().any(|check| check.status.failed()) {
        Some(ChecksState::Failing)
    } else if counted
        .iter()
        .any(|check| check.status == CheckStatus::Pending)
    {
        Some(ChecksState::Pending)
    } else {
        Some(ChecksState::Passing)
    }
}

/// What completing now would meet: a blocking policy not yet approved holds it back.
pub(super) fn merge_state(
    pr: &Value,
    mergeability: Mergeability,
    checks: &[PullRequestCheck],
) -> PullRequestMergeState {
    if pr["isDraft"].as_bool() == Some(true) {
        return PullRequestMergeState::Draft;
    }
    if state(pr) != Some(PullRequestState::Open) {
        return PullRequestMergeState::Unknown;
    }
    match mergeability {
        Mergeability::Conflicting => PullRequestMergeState::Dirty,
        Mergeability::Pending | Mergeability::Unknown => PullRequestMergeState::Unknown,
        Mergeability::Clean
            if checks.iter().any(|check| {
                check.required == Some(true)
                    && !matches!(check.status, CheckStatus::Success | CheckStatus::Skipped)
            }) =>
        {
            PullRequestMergeState::Blocked
        }
        Mergeability::Clean
            if checks.iter().any(|check| {
                !matches!(check.status, CheckStatus::Success | CheckStatus::Skipped)
            }) =>
        {
            PullRequestMergeState::Unstable
        }
        Mergeability::Clean => PullRequestMergeState::Clean,
    }
}

/// An account as Tcode shows it.
pub(super) fn login(identity: &Value) -> Option<String> {
    text(identity, "uniqueName")
        .or_else(|| text(identity, "displayName"))
        .filter(|login| !login.is_empty())
}

fn avatar(identity: &Value) -> Option<String> {
    text(&identity["_links"]["avatar"], "href").or_else(|| text(identity, "imageUrl"))
}

pub(super) fn actor(identity: &Value) -> Option<PullRequestActor> {
    Some(PullRequestActor {
        login: login(identity)?,
        avatar_url: avatar(identity),
    })
}

/// A reviewer's vote: 10 approved, 5 approved with suggestions, -5 waiting for the author, -10
/// rejected, 0 none yet.
fn vote(reviewer: &Value) -> Option<PullRequestReviewState> {
    match reviewer["vote"].as_i64()? {
        vote if vote > 0 => Some(PullRequestReviewState::Approved),
        vote if vote < 0 => Some(PullRequestReviewState::ChangesRequested),
        _ => None,
    }
}

/// The decision the votes come to: a vote against holds it back, then a required reviewer who
/// has not approved, then any approval.
fn review_decision(pr: &Value) -> Option<ReviewDecision> {
    let reviewers: Vec<&Value> = pr["reviewers"].as_array().into_iter().flatten().collect();
    if reviewers
        .iter()
        .any(|reviewer| reviewer["vote"].as_i64().is_some_and(|vote| vote < 0))
    {
        Some(ReviewDecision::ChangesRequested)
    } else if reviewers.iter().any(|reviewer| {
        reviewer["isRequired"].as_bool() == Some(true)
            && reviewer["vote"].as_i64().unwrap_or(0) <= 0
    }) {
        Some(ReviewDecision::Required)
    } else if reviewers
        .iter()
        .any(|reviewer| reviewer["vote"].as_i64().unwrap_or(0) > 0)
    {
        Some(ReviewDecision::Approved)
    } else {
        None
    }
}

/// The pull request from one read; `checks` are its statuses and policies. Azure keeps no
/// last-update time, so the closing time stands for it, else the creation.
pub(super) fn snapshot(
    pr: &Value,
    checks: &[PullRequestCheck],
    mergeability: Mergeability,
    synced_at: u64,
) -> Option<PullRequestSnapshot> {
    let state = state(pr)?;
    let created = text(pr, "creationDate")?;
    let closed = text(pr, "closedDate").filter(|_| state != PullRequestState::Open);
    Some(PullRequestSnapshot {
        state,
        title: text(pr, "title")?,
        head_branch: branch(pr["sourceRefName"].as_str()?),
        base_branch: branch(pr["targetRefName"].as_str()?),
        is_draft: pr["isDraft"].as_bool().unwrap_or(false),
        updated_at: closed.clone().unwrap_or(created),
        synced_at,
        merged_at: closed.clone().filter(|_| state == PullRequestState::Merged),
        closed_at: closed,
        author: login(&pr["createdBy"]).map(|login| PullRequestAuthor {
            login,
            avatar_url: avatar(&pr["createdBy"]),
        }),
        // Azure counts no lines; only the files read with both sides of each would.
        additions: None,
        deletions: None,
        changed_files: None,
        review_decision: review_decision(pr),
        checks_state: checks_state(checks),
        mergeability,
    })
}

/// Where a thread's comments are on the pull request's page.
fn thread_url(pr_url: &str, thread: &Value) -> Option<String> {
    Some(format!("{pr_url}?discussionId={}", thread["id"].as_u64()?))
}

/// When a comment's text changed after it was posted.
fn edited_at(comment: &Value) -> Option<String> {
    let published = comment["publishedDate"].as_str()?;
    let changed = comment["lastContentUpdatedDate"].as_str()?;
    (changed != published).then(|| changed.to_owned())
}

/// A comment someone wrote, not one Azure wrote of an event, and not deleted.
fn written(comment: &Value) -> bool {
    comment["commentType"].as_str() == Some("text") && comment["isDeleted"].as_bool() != Some(true)
}

fn comment(
    thread: &Value,
    raw: &Value,
    viewer: Option<&str>,
    pr_url: &str,
) -> Option<PullRequestComment> {
    written(raw).then_some(())?;
    Some(PullRequestComment {
        id: format!("{}:{}", thread["id"].as_u64()?, raw["id"].as_u64()?),
        author: actor(&raw["author"]),
        body: text(raw, "content").unwrap_or_default(),
        created_at: text(raw, "publishedDate")?,
        edited_at: edited_at(raw),
        url: thread_url(pr_url, thread),
        review_state: None,
        reactions: Vec::new(),
        viewer_can_update: viewer.is_some() && raw["author"]["id"].as_str() == viewer,
        viewer_can_react: false,
    })
}

/// Whether a thread's status says it was dealt with.
fn resolved(thread: &Value) -> bool {
    matches!(
        thread["status"].as_str(),
        Some("fixed" | "wontFix" | "closed" | "byDesign")
    )
}

/// A push's head and the commit it was compared with the target at.
pub(super) struct Iteration {
    pub(super) id: u64,
    pub(super) head: String,
    pub(super) base: String,
}

pub(super) fn iteration(raw: &Value) -> Option<Iteration> {
    Some(Iteration {
        id: raw["id"].as_u64()?,
        head: text(&raw["sourceRefCommit"], "commitId")?,
        base: text(&raw["commonRefCommit"], "commitId")?,
    })
}

/// Where a thread's lines are, at the commit Azure showed them at: the right side is the head
/// of the second iteration it compares; the left is the first iteration's head, or the common
/// commit when it compares one iteration with its own base.
fn anchor(thread: &Value, iterations: &[Iteration]) -> Option<PullRequestReviewAnchor> {
    let context = &thread["threadContext"];
    let latest = iterations.last()?.id;
    let compared = &thread["pullRequestThreadContext"]["iterationContext"];
    let second = compared["secondComparingIteration"]
        .as_u64()
        .unwrap_or(latest);
    let first = compared["firstComparingIteration"]
        .as_u64()
        .unwrap_or(second);
    let at = |id: u64| iterations.iter().find(|iteration| iteration.id == id);
    let line = |position: &Value| {
        position["line"]
            .as_u64()
            .filter(|line| *line > 0)
            .and_then(|line| u32::try_from(line).ok())
    };
    let (side, start, end, revision) = match line(&context["rightFileStart"]) {
        Some(start) => (
            ReviewSide::New,
            start,
            line(&context["rightFileEnd"]),
            at(second)?.head.clone(),
        ),
        None => (
            ReviewSide::Old,
            line(&context["leftFileStart"])?,
            line(&context["leftFileEnd"]),
            if first == second {
                at(second)?.base.clone()
            } else {
                at(first)?.head.clone()
            },
        ),
    };
    let end = end.filter(|end| *end >= start).unwrap_or(start);
    Some(PullRequestReviewAnchor {
        revision,
        path: text(context, "filePath")?
            .trim_start_matches('/')
            .to_owned(),
        side,
        start_line: start,
        end_line: end,
    })
}

pub(super) struct Conversation {
    pub(super) description: PullRequestComment,
    pub(super) comments: Vec<PullRequestComment>,
    pub(super) threads: Vec<PullRequestReviewThread>,
    pub(super) permissions: PullRequestPermissions,
    pub(super) reviewers: Vec<PullRequestReviewerState>,
}

/// The conversation: the description; every comment of a thread on no file, oldest first; and
/// each thread on a file's lines as a review thread, which replies and resolution address by the
/// thread's id. `iterations` anchor them, and without them (anonymously) threads carry no
/// anchor. `viewer` is the reading account's id, `None` anonymously.
pub(super) fn conversation(
    pr: &Value,
    raw: &[Value],
    iterations: &[Iteration],
    viewer: Option<&str>,
    pr_url: &str,
) -> Conversation {
    let signed_in = viewer.is_some();
    let latest = iterations.last().map(|iteration| iteration.id);
    let mut comments = Vec::new();
    let mut threads = Vec::new();
    for thread in raw {
        if thread["isDeleted"].as_bool() == Some(true) {
            continue;
        }
        let shown: Vec<_> = thread["comments"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|raw| comment(thread, raw, viewer, pr_url))
            .collect();
        let path = thread["threadContext"]["filePath"]
            .as_str()
            .map(|path| path.trim_start_matches('/').to_owned());
        let (Some(path), Some(id)) = (path, thread["id"].as_u64()) else {
            comments.extend(shown);
            continue;
        };
        if shown.is_empty() {
            continue;
        }
        let compared =
            thread["pullRequestThreadContext"]["iterationContext"]["secondComparingIteration"]
                .as_u64();
        threads.push(PullRequestReviewThread {
            id: id.to_string(),
            path,
            resolved: resolved(thread),
            outdated: matches!((compared, latest), (Some(compared), Some(latest)) if compared < latest),
            anchor: anchor(thread, iterations),
            diff_hunk: None,
            total_comments: shown.len() as u64,
            comments: shown,
            replies_after: None,
            viewer_can_reply: signed_in,
            viewer_can_resolve: signed_in,
        });
    }
    comments.sort_by(|left, right| left.created_at.cmp(&right.created_at));
    for thread in &mut threads {
        thread
            .comments
            .sort_by(|left, right| left.created_at.cmp(&right.created_at));
    }
    let description = PullRequestComment {
        id: PULL_REQUEST.into(),
        author: actor(&pr["createdBy"]),
        body: text(pr, "description").unwrap_or_default(),
        created_at: text(pr, "creationDate").unwrap_or_default(),
        edited_at: None,
        url: Some(pr_url.to_owned()),
        review_state: None,
        reactions: Vec::new(),
        viewer_can_update: signed_in,
        viewer_can_react: false,
    };
    let verdicts = if signed_in {
        vec![
            PullRequestReviewVerdict::Comment,
            PullRequestReviewVerdict::Approve,
            PullRequestReviewVerdict::RequestChanges,
        ]
    } else {
        Vec::new()
    };
    Conversation {
        description,
        comments,
        threads,
        // Azure states no permission a read reaches; whoever signs in may try, and Azure
        // refuses what they may not do.
        permissions: PullRequestPermissions {
            update: signed_in,
            verdicts,
            label: signed_in,
            request_reviewers: signed_in,
        },
        reviewers: reviewers(pr),
    }
}

pub(super) fn reviewer(identity: &Value) -> Option<PullRequestReviewer> {
    Some(PullRequestReviewer {
        id: text(identity, "id")?,
        login: login(identity)?,
        kind: if identity["isContainer"].as_bool() == Some(true) {
            PullRequestReviewerKind::Team
        } else {
            PullRequestReviewerKind::User
        },
    })
}

/// The reviewers, each with their vote.
pub(super) fn reviewers(pr: &Value) -> Vec<PullRequestReviewerState> {
    pr["reviewers"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|identity| {
            Some(PullRequestReviewerState {
                reviewer: reviewer(identity)?,
                avatar_url: avatar(identity),
                verdict: vote(identity),
            })
        })
        .collect()
}

/// The written comments and the votes among a pull request's threads, as news for a watch.
pub(super) fn remarks(threads: &[Value], pr_url: &str) -> Vec<PullRequestRemark> {
    let mut remarks = Vec::new();
    for thread in threads {
        if thread["isDeleted"].as_bool() == Some(true) {
            continue;
        }
        let Some(id) = thread["id"].as_u64() else {
            continue;
        };
        let properties = &thread["properties"];
        if properties["CodeReviewThreadType"]["$value"].as_str() == Some("VoteUpdate") {
            let state = match properties["CodeReviewVoteResult"]["$value"]
                .as_str()
                .and_then(|vote| vote.parse::<i64>().ok())
            {
                Some(vote) if vote > 0 => "APPROVED",
                Some(vote) if vote < 0 => "CHANGES_REQUESTED",
                _ => continue,
            };
            let voter = properties["CodeReviewVotedByIdentity"]["$value"]
                .as_str()
                .map(|index| &thread["identities"][index]);
            remarks.push(PullRequestRemark {
                id: format!("vote:{id}"),
                author: voter.and_then(login),
                body: String::new(),
                created_at: text(thread, "publishedDate").unwrap_or_default(),
                edited_at: None,
                url: thread_url(pr_url, thread),
                path: None,
                review_state: Some(state.to_owned()),
            });
            continue;
        }
        let path = thread["threadContext"]["filePath"]
            .as_str()
            .map(|path| path.trim_start_matches('/').to_owned());
        for raw in thread["comments"].as_array().into_iter().flatten() {
            if !written(raw) {
                continue;
            }
            let (Some(comment), Some(created_at)) =
                (raw["id"].as_u64(), text(raw, "publishedDate"))
            else {
                continue;
            };
            remarks.push(PullRequestRemark {
                id: format!("{id}:{comment}"),
                author: login(&raw["author"]),
                body: text(raw, "content").unwrap_or_default(),
                created_at,
                edited_at: edited_at(raw),
                url: thread_url(pr_url, thread),
                path: path.clone(),
                review_state: None,
            });
        }
    }
    remarks
}

/// Each reviewer's vote by identity id, as the pull request holds them.
pub(super) fn votes(pr: &Value) -> HashMap<String, i64> {
    pr["reviewers"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|reviewer| Some((text(reviewer, "id")?, reviewer["vote"].as_i64()?)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Azure's trial merge reads as mergeability only where it names one: a conflict is one, a
    /// rejection by policy or a failed merge is not, and a merge still to run is pending;
    /// auto-complete is armed once someone set it, with the strategy it was set with. Votes come
    /// to a decision: one against holds it back, a required reviewer yet to approve is required,
    /// then any approval.
    #[test]
    fn merge_status_auto_complete_and_votes_read_as_tcode_has_them() {
        let status = |status: &str| mergeability(&json!({"mergeStatus": status}));
        assert_eq!(status("succeeded"), Mergeability::Clean);
        assert_eq!(status("conflicts"), Mergeability::Conflicting);
        for unread in ["rejectedByPolicy", "failure"] {
            assert_eq!(status(unread), Mergeability::Unknown, "{unread}");
        }
        for running in ["queued", "notSet"] {
            assert_eq!(status(running), Mergeability::Pending, "{running}");
        }
        assert_eq!(
            auto_complete(&json!({"completionOptions": {"mergeStrategy": "squash"}})),
            None
        );
        assert_eq!(
            auto_complete(
                &json!({"autoCompleteSetBy": {"id": "u1"}, "completionOptions": {"mergeStrategy": "rebase"}})
            ),
            Some(PullRequestMergeMethod::Rebase)
        );
        assert_eq!(
            auto_complete(
                &json!({"autoCompleteSetBy": {"id": "u1"}, "completionOptions": {"squashMerge": true}})
            ),
            Some(PullRequestMergeMethod::Squash)
        );
        let decision = |reviewers: Value| review_decision(&json!({ "reviewers": reviewers }));
        assert_eq!(
            decision(json!([{"vote": 10}, {"vote": -5}])),
            Some(ReviewDecision::ChangesRequested)
        );
        assert_eq!(
            decision(json!([{"vote": 5}, {"vote": 0, "isRequired": true}])),
            Some(ReviewDecision::Required)
        );
        assert_eq!(
            decision(json!([{"vote": 5}, {"vote": 0}])),
            Some(ReviewDecision::Approved)
        );
        assert_eq!(decision(json!([{"vote": 0}])), None);
    }

    /// A thread on a file's lines is a review thread, resolved once its status is fixed, closed
    /// or the like, and anchored at the head of the iteration it was written on, outdated once a
    /// later push came; threads on no file are comments, oldest first; Azure's own comments and
    /// deleted ones show nowhere, and an edit shows by its content's time.
    #[test]
    fn threads_read_as_review_threads_and_comments() {
        let author = json!({"id": "u1", "uniqueName": "ana@fabrikam.test"});
        let text = |id: u64, at: &str, body: &str| json!({"id": id, "parentCommentId": 0, "author": author, "content": body, "publishedDate": at, "lastContentUpdatedDate": at, "commentType": "text"});
        let threads = vec![
            json!({"id": 7, "status": "fixed",
                   "threadContext": {"filePath": "/src/lib.rs", "rightFileStart": {"line": 10, "offset": 1}, "rightFileEnd": {"line": 12, "offset": 1}},
                   "pullRequestThreadContext": {"iterationContext": {"firstComparingIteration": 1, "secondComparingIteration": 1}},
                   "comments": [text(1, "2026-10-01T00:00:01Z", "why?"),
                                {"id": 2, "parentCommentId": 1, "author": {"id": "u2", "displayName": "Bo"}, "content": "because",
                                 "publishedDate": "2026-10-01T00:00:02Z", "lastContentUpdatedDate": "2026-10-01T00:00:09Z", "commentType": "text"}]}),
            json!({"id": 8, "status": "active", "comments": [text(1, "2026-10-01T00:00:05Z", "ship it")]}),
            json!({"id": 9, "comments": [{"id": 1, "commentType": "system", "content": "Ana voted 10", "publishedDate": "2026-10-01T00:00:03Z"}]}),
            json!({"id": 10, "status": "active", "comments": [text(1, "2026-10-01T00:00:04Z", "first"),
                                                             {"id": 2, "commentType": "text", "isDeleted": true, "content": "", "publishedDate": "2026-10-01T00:00:06Z"}]}),
        ];
        let iterations = [
            Iteration {
                id: 1,
                head: "h1".into(),
                base: "b1".into(),
            },
            Iteration {
                id: 2,
                head: "h2".into(),
                base: "b1".into(),
            },
        ];
        let pr = json!({"createdBy": author, "description": "d", "creationDate": "2026-10-01T00:00:00Z"});
        let read = conversation(
            &pr,
            &threads,
            &iterations,
            Some("u1"),
            "https://dev.azure.com/f/p/_git/r/pullrequest/3",
        );
        assert_eq!(read.threads.len(), 1);
        let thread = &read.threads[0];
        assert_eq!(
            (thread.id.as_str(), thread.path.as_str()),
            ("7", "src/lib.rs")
        );
        assert!(thread.resolved && thread.outdated);
        assert_eq!(
            thread.anchor,
            Some(PullRequestReviewAnchor {
                revision: "h1".into(),
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
                .map(|comment| (
                    comment.id.as_str(),
                    comment.edited_at.is_some(),
                    comment.viewer_can_update
                ))
                .collect::<Vec<_>>(),
            [("7:1", false, true), ("7:2", true, false)]
        );
        assert_eq!(
            read.comments
                .iter()
                .map(|comment| comment.body.as_str())
                .collect::<Vec<_>>(),
            ["first", "ship it"]
        );
        assert_eq!(
            thread.comments[0].url.as_deref(),
            Some("https://dev.azure.com/f/p/_git/r/pullrequest/3?discussionId=7")
        );
    }

    /// A status context's later post replaces its earlier one, and a branch policy reads as a
    /// check that is required when it blocks completion; a policy that does not apply counts
    /// for nothing, and a blocking policy not yet approved holds a clean merge back.
    #[test]
    fn statuses_and_policies_read_as_checks() {
        let statuses = status_checks(&json!({"value": [
            {"state": "failed", "context": {"genre": "ci", "name": "build"}},
            {"state": "succeeded", "context": {"genre": "ci", "name": "build"}, "targetUrl": "https://ci.test/1"},
        ]}));
        assert_eq!(
            statuses
                .iter()
                .map(|check| (check.name.as_str(), check.status))
                .collect::<Vec<_>>(),
            [("ci/build", CheckStatus::Success)]
        );
        let policies = policy_checks(&json!({"value": [
            {"status": "running", "configuration": {"isEnabled": true, "isBlocking": true, "type": {"displayName": "Build"}, "settings": {"displayName": "PR build"}}},
            {"status": "notApplicable", "configuration": {"isEnabled": true, "isBlocking": false, "type": {"displayName": "Comment requirements"}}},
        ]}));
        assert_eq!(
            policies
                .iter()
                .map(|check| (check.name.as_str(), check.status, check.required))
                .collect::<Vec<_>>(),
            [
                ("PR build", CheckStatus::Pending, Some(true)),
                ("Comment requirements", CheckStatus::Skipped, Some(false))
            ]
        );
        let checks: Vec<_> = statuses.into_iter().chain(policies).collect();
        assert_eq!(checks_state(&checks), Some(ChecksState::Pending));
        let open = json!({"status": "active"});
        assert_eq!(
            merge_state(&open, Mergeability::Clean, &checks),
            PullRequestMergeState::Blocked
        );
        assert_eq!(
            merge_state(&open, Mergeability::Clean, &checks[..1]),
            PullRequestMergeState::Clean
        );
    }
}
