//! Writes to a Forgejo or Gitea pull request. Each is sent once; an answer that cannot say
//! whether the server applied it is uncertain, never retried.

use super::{
    api::{Request, Response},
    reads::{PULL_REQUEST, Pull, reaction_name, text},
};
use crate::forge::{ForgeError, ForgeErrorKind, Step, answered, in_order};
use serde_json::{Value, json};
use tcode_core::{
    pull_request::{PullRequestMergeMethod, PullRequestReviewDraftComment},
    session::ReviewSide,
};
use tcode_protocol::{
    PullRequestAction, PullRequestActionResult as Outcome, PullRequestRejection as Rejection,
    PullRequestReviewVerdict,
};

impl Pull<'_> {
    fn write(
        &self,
        method: &str,
        rest: &str,
        body: Option<Value>,
        operation: &'static str,
    ) -> Result<Response, ForgeError> {
        self.api.send(
            self.authority(),
            Request::write(method, self.path(rest), body, operation),
        )
    }

    /// The pull request at `head`, or the head it moved to.
    fn at_head(&self, head: &str) -> Result<Value, Rejection> {
        let pr = self.pull().map_err(ForgeError::rejection)?;
        let current = text(&pr["head"], "sha").ok_or(Rejection::Failed)?;
        if current != head {
            return Err(Rejection::StaleHead { head: current });
        }
        Ok(pr)
    }

    /// An issue comment id that belongs to this pull request: an id names any comment in the
    /// repository, whichever pull request the request names.
    fn own_comment(&self, id: &str) -> Result<u64, Rejection> {
        let id: u64 = id.parse().map_err(|_| Rejection::Invalid)?;
        let comment = self
            .get(&format!("issues/comments/{id}"), "Comment")
            .map_err(|error| match error.kind {
                ForgeErrorKind::NotFound => Rejection::ForeignSubject,
                _ => error.rejection(),
            })?;
        let number = format!("/{}", self.key.number);
        let on = |field: &str| {
            comment[field]
                .as_str()
                .is_some_and(|url| url.ends_with(&number))
        };
        if on("issue_url") || on("pull_request_url") {
            Ok(id)
        } else {
            Err(Rejection::ForeignSubject)
        }
    }

    /// A review comment id among this pull request's line comments.
    fn own_line_comment(&self, id: &str) -> Result<u64, Rejection> {
        let id: u64 = id.parse().map_err(|_| Rejection::Invalid)?;
        let reviews = self
            .get(&format!("pulls/{}/reviews", self.key.number), "Reviews")
            .map_err(ForgeError::rejection)?;
        for review in reviews.as_array().into_iter().flatten() {
            let Some(review) = review["id"].as_u64() else {
                continue;
            };
            let comments = self
                .get(
                    &format!("pulls/{}/reviews/{review}/comments", self.key.number),
                    "ReviewComments",
                )
                .map_err(ForgeError::rejection)?;
            if comments
                .as_array()
                .into_iter()
                .flatten()
                .any(|comment| comment["id"].as_u64() == Some(id))
            {
                return Ok(id);
            }
        }
        Err(Rejection::ForeignSubject)
    }

    fn merge(&self, head: &str, method: PullRequestMergeMethod) -> Result<Outcome, Rejection> {
        self.at_head(head)?;
        let style = match method {
            PullRequestMergeMethod::Merge => "merge",
            PullRequestMergeMethod::Squash => "squash",
            PullRequestMergeMethod::Rebase => "rebase",
        };
        // Gitea 1.26 renamed the body's fields to snake case; older Gitea and Forgejo take `Do`.
        let field = if self.api.server(self.authority()).snake_case_merge() {
            "do"
        } else {
            "Do"
        };
        let body = json!({ field: style, "head_commit_id": head });
        Ok(answered(self.write(
            "POST",
            &format!("pulls/{}/merge", self.key.number),
            Some(body),
            "Merge",
        )))
    }

    fn update_branch(&self, head: &str, rebase: bool) -> Result<Outcome, Rejection> {
        let pr = self.at_head(head)?;
        if text(&pr, "merge_base").is_some() && pr["merge_base"] == pr["base"]["sha"] {
            return Ok(Outcome::UpToDate);
        }
        let style = if rebase { "rebase" } else { "merge" };
        Ok(answered(self.write(
            "POST",
            &format!("pulls/{}/update?style={style}", self.key.number),
            None,
            "UpdateBranch",
        )))
    }
}

fn blank(text: &str) -> bool {
    text.trim().is_empty()
}

pub(super) fn act(pull: &Pull<'_>, action: &PullRequestAction) -> Outcome {
    let number = pull.key.number;
    let result = || -> Result<Outcome, Rejection> {
        Ok(match action {
            PullRequestAction::Comment { body } => {
                if blank(body) {
                    return Err(Rejection::Invalid);
                }
                answered(pull.write(
                    "POST",
                    &format!("issues/{number}/comments"),
                    Some(json!({ "body": body })),
                    "Comment",
                ))
            }
            PullRequestAction::ReplyToThread { thread_id, body } => {
                if blank(body) {
                    return Err(Rejection::Invalid);
                }
                let id: u64 = thread_id.parse().map_err(|_| Rejection::Invalid)?;
                answered(pull.write(
                    "POST",
                    &format!("pulls/{number}/comments/{id}/replies"),
                    Some(json!({ "body": body })),
                    "Reply",
                ))
            }
            PullRequestAction::ResolveThread {
                thread_id,
                resolved,
            } => {
                let id = pull.own_line_comment(thread_id)?;
                let verb = if *resolved { "resolve" } else { "unresolve" };
                answered(pull.write(
                    "POST",
                    &format!("pulls/comments/{id}/{verb}"),
                    None,
                    "Resolve",
                ))
            }
            PullRequestAction::React {
                subject_id,
                content,
                reacted,
            } => {
                let path = if subject_id == PULL_REQUEST {
                    format!("issues/{number}/reactions")
                } else {
                    format!(
                        "issues/comments/{}/reactions",
                        pull.own_comment(subject_id)?
                    )
                };
                answered(pull.write(
                    if *reacted { "POST" } else { "DELETE" },
                    &path,
                    Some(json!({ "content": reaction_name(*content) })),
                    "React",
                ))
            }
            PullRequestAction::EditComment { comment_id, body } => {
                if blank(body) {
                    return Err(Rejection::Invalid);
                }
                if comment_id == PULL_REQUEST {
                    return Err(Rejection::Invalid);
                }
                let id = pull.own_comment(comment_id)?;
                answered(pull.write(
                    "PATCH",
                    &format!("issues/comments/{id}"),
                    Some(json!({ "body": body })),
                    "EditComment",
                ))
            }
            PullRequestAction::Edit { title, body } => {
                let mut fields = serde_json::Map::new();
                if let Some(title) = title {
                    if blank(title) {
                        return Err(Rejection::Invalid);
                    }
                    fields.insert("title".into(), json!(title));
                }
                if let Some(body) = body {
                    fields.insert("body".into(), json!(body));
                }
                if fields.is_empty() {
                    return Err(Rejection::Invalid);
                }
                answered(pull.write(
                    "PATCH",
                    &format!("pulls/{number}"),
                    Some(Value::Object(fields)),
                    "Edit",
                ))
            }
            PullRequestAction::SetLabels { add, remove } => {
                let ids = |labels: &[String]| -> Result<Vec<u64>, Rejection> {
                    labels
                        .iter()
                        .map(|id| id.parse().map_err(|_| Rejection::Invalid))
                        .collect()
                };
                let (added, removed) = (ids(add)?, ids(remove)?);
                if added.is_empty() && removed.is_empty() {
                    return Err(Rejection::Invalid);
                }
                let mut steps: Vec<Step> = Vec::new();
                if !added.is_empty() {
                    steps.push((
                        add.clone(),
                        Box::new(move || {
                            answered(pull.write(
                                "POST",
                                &format!("issues/{number}/labels"),
                                Some(json!({ "labels": added })),
                                "AddLabels",
                            ))
                        }),
                    ));
                }
                for (id, name) in removed.into_iter().zip(remove) {
                    steps.push((
                        vec![name.clone()],
                        Box::new(move || {
                            answered(pull.write(
                                "DELETE",
                                &format!("issues/{number}/labels/{id}"),
                                None,
                                "RemoveLabel",
                            ))
                        }),
                    ));
                }
                in_order(steps)
            }
            PullRequestAction::SetReviewers { add, remove } => {
                if add.is_empty() && remove.is_empty() {
                    return Err(Rejection::Invalid);
                }
                let logins = |reviewers: &[tcode_protocol::PullRequestReviewer]| -> Vec<String> {
                    reviewers
                        .iter()
                        .map(|reviewer| reviewer.id.clone())
                        .collect()
                };
                let mut steps: Vec<Step> = Vec::new();
                for (method, reviewers, operation) in [
                    ("POST", logins(add), "RequestReviewers"),
                    ("DELETE", logins(remove), "RemoveReviewers"),
                ] {
                    if reviewers.is_empty() {
                        continue;
                    }
                    steps.push((
                        reviewers.clone(),
                        Box::new(move || {
                            answered(pull.write(
                                method,
                                &format!("pulls/{number}/requested_reviewers"),
                                Some(json!({ "reviewers": reviewers })),
                                operation,
                            ))
                        }),
                    ));
                }
                in_order(steps)
            }
            PullRequestAction::Close | PullRequestAction::Reopen => {
                let state = if matches!(action, PullRequestAction::Close) {
                    "closed"
                } else {
                    "open"
                };
                answered(pull.write(
                    "PATCH",
                    &format!("pulls/{number}"),
                    Some(json!({ "state": state })),
                    "SetState",
                ))
            }
            PullRequestAction::UpdateBranch { head, rebase } => {
                pull.update_branch(head, *rebase)?
            }
            // Auto-merge and a cleaned message are not capabilities here, so the dispatcher sends
            // neither.
            PullRequestAction::Merge { head, method, .. } => pull.merge(head, *method)?,
            PullRequestAction::SubmitReview { .. }
            | PullRequestAction::DisableAutoMerge
            | PullRequestAction::ReadyForReview
            | PullRequestAction::ConvertToDraft
            | PullRequestAction::Revert
            | PullRequestAction::MergeStack { .. }
            | PullRequestAction::RebaseStack { .. } => return Err(Rejection::Invalid),
        })
    };
    result().unwrap_or_else(Outcome::Rejected)
}

/// One review request carries the verdict, the body and every line comment, so a review is
/// never left half sent.
pub(super) fn submit_review(
    pull: &Pull<'_>,
    verdict: PullRequestReviewVerdict,
    head: &str,
    body: &str,
    comments: &[PullRequestReviewDraftComment],
) -> Outcome {
    if verdict == PullRequestReviewVerdict::Comment && blank(body) && comments.is_empty() {
        return Outcome::Rejected(Rejection::Invalid);
    }
    if let Err(rejection) = pull.at_head(head) {
        return Outcome::Rejected(rejection);
    }
    let event = match verdict {
        PullRequestReviewVerdict::Comment => "COMMENT",
        PullRequestReviewVerdict::Approve => "APPROVED",
        PullRequestReviewVerdict::RequestChanges => "REQUEST_CHANGES",
    };
    let comments: Vec<_> = comments
        .iter()
        .map(|comment| {
            let line = comment.end_line;
            match comment.side {
                ReviewSide::New => {
                    json!({"path": comment.path, "body": comment.body, "new_position": line})
                }
                ReviewSide::Old => {
                    json!({"path": comment.path, "body": comment.body, "old_position": line})
                }
            }
        })
        .collect();
    answered(pull.write(
        "POST",
        &format!("pulls/{}/reviews", pull.key.number),
        Some(json!({
            "event": event,
            "body": body,
            "commit_id": head,
            "comments": comments,
        })),
        "SubmitReview",
    ))
}
