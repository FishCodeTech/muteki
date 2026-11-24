//! Writes to a Bitbucket Cloud pull request. Each is sent once; an answer that cannot say
//! whether Bitbucket applied it is uncertain, never retried.

use super::{
    api::{Request, Response},
    reads::{PULL_REQUEST, Pr, head, text},
};
use crate::forge::{ForgeError, ForgeErrorKind, Step, answered, in_order};
use serde_json::{Value, json};
use tcode_core::{
    pull_request::{PullRequestMergeMethod, PullRequestReviewDraftComment},
    session::ReviewSide,
};
use tcode_protocol::{
    PullRequestAction, PullRequestActionResult as Outcome, PullRequestFile,
    PullRequestRejection as Rejection, PullRequestReviewVerdict, PullRequestReviewer,
};

impl Pr<'_> {
    fn write(
        &self,
        method: &str,
        path: String,
        body: Option<Value>,
        operation: &'static str,
    ) -> Result<Response, ForgeError> {
        self.api.send(Request::write(method, path, body, operation))
    }

    /// The pull request at `head`, or the head it moved to.
    fn at_head(&self, at: &str) -> Result<Value, Rejection> {
        let pr = self.pr().map_err(ForgeError::rejection)?;
        let current = head(&pr).ok_or(Rejection::Failed)?;
        if current != at {
            return Err(Rejection::StaleHead { head: current });
        }
        Ok(pr)
    }

    /// A comment id among this pull request's: the path names the pull request, so Bitbucket
    /// answers not found for another's.
    fn own_comment(&self, id: &str) -> Result<u64, Rejection> {
        let id: u64 = id.parse().map_err(|_| Rejection::Invalid)?;
        self.get(self.path(&format!("/comments/{id}?fields=id")), "Comment")
            .map_err(|error| match error.kind {
                ForgeErrorKind::NotFound => Rejection::ForeignSubject,
                _ => error.rejection(),
            })?;
        Ok(id)
    }

    /// Bitbucket writes the reviewers whole, so the change applies to the set it holds now; the
    /// set it answers with says which part of the change took.
    fn set_reviewers(
        &self,
        add: &[PullRequestReviewer],
        remove: &[PullRequestReviewer],
    ) -> Result<Outcome, Rejection> {
        let pr = self.pr().map_err(ForgeError::rejection)?;
        let mut wanted: Vec<String> = pr["reviewers"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|user| text(user, "uuid"))
            .filter(|uuid| !remove.iter().any(|reviewer| reviewer.id == *uuid))
            .collect();
        for reviewer in add {
            if !wanted.contains(&reviewer.id) {
                wanted.push(reviewer.id.clone());
            }
        }
        let body = json!({ "reviewers": wanted.iter().map(|uuid| json!({ "uuid": uuid })).collect::<Vec<_>>() });
        Ok(
            match self.write("PUT", self.path(""), Some(body), "SetReviewers") {
                Ok(response) => match response.json::<Value>() {
                    Ok(answer) => reviewer_change(add, remove, &answer),
                    Err(_) => Outcome::Uncertain,
                },
                Err(error) => answered::<()>(Err(error)),
            },
        )
    }

    fn merge(&self, at: &str, method: PullRequestMergeMethod) -> Result<Outcome, Rejection> {
        let pr = self.at_head(at)?;
        let strategy = match method {
            PullRequestMergeMethod::Merge => "merge_commit",
            PullRequestMergeMethod::Squash => "squash",
            // The linear history GitHub calls rebase and merge.
            PullRequestMergeMethod::Rebase => "rebase_fast_forward",
        };
        let body = json!({
            "type": "pullrequest",
            "merge_strategy": strategy,
            "close_source_branch": pr["close_source_branch"].as_bool().unwrap_or(false),
        });
        Ok(
            match self.write("POST", self.path("/merge"), Some(body), "Merge") {
                // A merge Bitbucket finishes later answers 202, with nothing yet to confirm.
                Ok(response) if response.status == 202 => Outcome::Uncertain,
                Ok(response) => match response.json::<Value>() {
                    Ok(merged) if merged["state"].as_str() == Some("MERGED") => Outcome::Applied,
                    _ => Outcome::Uncertain,
                },
                Err(error) => answered::<()>(Err(error)),
            },
        )
    }
}

/// What a reviewer change came to, by the reviewers Bitbucket answered with: each named
/// reviewer is where the change put them, or the change only partly took.
pub(super) fn reviewer_change(
    add: &[PullRequestReviewer],
    remove: &[PullRequestReviewer],
    answer: &Value,
) -> Outcome {
    let held: Vec<&str> = answer["reviewers"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|user| user["uuid"].as_str())
        .collect();
    let (applied, unapplied): (Vec<_>, Vec<_>) = add
        .iter()
        .map(|reviewer| (reviewer, true))
        .chain(remove.iter().map(|reviewer| (reviewer, false)))
        .partition(|(reviewer, wanted)| held.contains(&reviewer.id.as_str()) == *wanted);
    let names = |list: Vec<(&PullRequestReviewer, bool)>| -> Vec<String> {
        list.into_iter()
            .map(|(reviewer, _)| reviewer.login.clone())
            .collect()
    };
    let failure = Outcome::Rejected(Rejection::Refused {
        messages: Vec::new(),
    });
    match (applied.is_empty(), unapplied.is_empty()) {
        (_, true) => Outcome::Applied,
        (true, false) => failure,
        (false, false) => Outcome::Partial {
            applied: names(applied),
            unapplied: names(unapplied),
            failure: Box::new(failure),
        },
    }
}

fn blank(text: &str) -> bool {
    text.trim().is_empty()
}

fn content(body: &str) -> Value {
    json!({ "content": { "raw": body } })
}

pub(super) fn act(pr: &Pr<'_>, action: &PullRequestAction) -> Outcome {
    let result = || -> Result<Outcome, Rejection> {
        Ok(match action {
            PullRequestAction::Comment { body } => {
                if blank(body) {
                    return Err(Rejection::Invalid);
                }
                answered(pr.write("POST", pr.path("/comments"), Some(content(body)), "Comment"))
            }
            PullRequestAction::ReplyToThread { thread_id, body } => {
                let parent: u64 = thread_id.parse().map_err(|_| Rejection::Invalid)?;
                if blank(body) {
                    return Err(Rejection::Invalid);
                }
                let mut request = content(body);
                request["parent"] = json!({ "id": parent });
                answered(pr.write("POST", pr.path("/comments"), Some(request), "Reply"))
            }
            // Resolution is a sub-resource of the thread's first comment, created and deleted.
            PullRequestAction::ResolveThread {
                thread_id,
                resolved,
            } => {
                let id: u64 = thread_id.parse().map_err(|_| Rejection::Invalid)?;
                answered(pr.write(
                    if *resolved { "POST" } else { "DELETE" },
                    pr.path(&format!("/comments/{id}/resolve")),
                    None,
                    "Resolve",
                ))
            }
            PullRequestAction::EditComment { comment_id, body } => {
                if blank(body) || comment_id == PULL_REQUEST {
                    return Err(Rejection::Invalid);
                }
                let id = pr.own_comment(comment_id)?;
                answered(pr.write(
                    "PUT",
                    pr.path(&format!("/comments/{id}")),
                    Some(content(body)),
                    "EditComment",
                ))
            }
            // Bitbucket's PUT changes only the fields it is given.
            PullRequestAction::Edit { title, body } => {
                let mut fields = serde_json::Map::new();
                if let Some(title) = title {
                    if blank(title) {
                        return Err(Rejection::Invalid);
                    }
                    fields.insert("title".into(), json!(title));
                }
                if let Some(body) = body {
                    fields.insert("description".into(), json!(body));
                }
                if fields.is_empty() {
                    return Err(Rejection::Invalid);
                }
                answered(pr.write("PUT", pr.path(""), Some(Value::Object(fields)), "Edit"))
            }
            PullRequestAction::SetReviewers { add, remove } => {
                if add.is_empty() && remove.is_empty() {
                    return Err(Rejection::Invalid);
                }
                pr.set_reviewers(add, remove)?
            }
            PullRequestAction::ReadyForReview | PullRequestAction::ConvertToDraft => {
                let draft = matches!(action, PullRequestAction::ConvertToDraft);
                answered(pr.write(
                    "PUT",
                    pr.path(""),
                    Some(json!({ "draft": draft })),
                    "SetDraft",
                ))
            }
            PullRequestAction::Close => {
                answered(pr.write("POST", pr.path("/decline"), None, "Decline"))
            }
            PullRequestAction::Merge {
                head,
                method,
                auto: false,
                ..
            } => pr.merge(head, *method)?,
            PullRequestAction::React { .. }
            | PullRequestAction::SetLabels { .. }
            | PullRequestAction::Reopen
            | PullRequestAction::UpdateBranch { .. }
            | PullRequestAction::Merge { auto: true, .. }
            | PullRequestAction::DisableAutoMerge
            | PullRequestAction::Revert => return Err(Rejection::Unsupported),
            PullRequestAction::SubmitReview { .. }
            | PullRequestAction::MergeStack { .. }
            | PullRequestAction::RebaseStack { .. } => return Err(Rejection::Invalid),
        })
    };
    result().unwrap_or_else(Outcome::Rejected)
}

/// Bitbucket has no review: its line comments go first, each on its line of the diff the pull
/// request has now, then the summary as a comment, then the vote, so a review cut short is
/// never an approval. One that stops part way is [`Outcome::Partial`].
pub(super) fn submit_review(
    pr: &Pr<'_>,
    files: &[PullRequestFile],
    verdict: PullRequestReviewVerdict,
    at: &str,
    body: &str,
    comments: &[PullRequestReviewDraftComment],
) -> Outcome {
    if verdict == PullRequestReviewVerdict::Comment && blank(body) && comments.is_empty() {
        return Outcome::Rejected(Rejection::Invalid);
    }
    if let Err(rejection) = pr.at_head(at) {
        return Outcome::Rejected(rejection);
    }
    let mut steps: Vec<Step> = Vec::new();
    for comment in comments {
        let lines = (comment.start_line, comment.end_line);
        if !crate::github::pull_request_actions::in_hunks(files, &comment.path, comment.side, lines)
        {
            return Outcome::Rejected(Rejection::Invalid);
        }
        let mut request = content(&comment.body);
        let (end, start) = match comment.side {
            ReviewSide::New => ("to", "start_to"),
            ReviewSide::Old => ("from", "start_from"),
        };
        request["inline"] = json!({ "path": comment.path, end: comment.end_line });
        if comment.start_line < comment.end_line {
            request["inline"][start] = json!(comment.start_line);
        }
        steps.push((
            vec![format!("{}:{}", comment.path, comment.end_line)],
            Box::new(move || {
                answered(pr.write("POST", pr.path("/comments"), Some(request), "ReviewComment"))
            }),
        ));
    }
    if !blank(body) {
        let request = content(body);
        steps.push((
            vec!["summary".into()],
            Box::new(move || {
                answered(pr.write("POST", pr.path("/comments"), Some(request), "ReviewSummary"))
            }),
        ));
    }
    let vote = match verdict {
        PullRequestReviewVerdict::Approve => Some(("/approve", "approval", "Approve")),
        PullRequestReviewVerdict::RequestChanges => {
            Some(("/request-changes", "request changes", "RequestChanges"))
        }
        PullRequestReviewVerdict::Comment => None,
    };
    if let Some((path, name, operation)) = vote {
        steps.push((
            vec![name.into()],
            Box::new(move || answered(pr.write("POST", pr.path(path), None, operation))),
        ));
    }
    in_order(steps)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A reviewer change holds as far as the reviewers Bitbucket answered with show it: one it
    /// kept out is not applied, and an answer that shows none of it is a refusal.
    #[test]
    fn a_reviewer_change_is_what_bitbucket_answers() {
        let user = |uuid: &str, login: &str| PullRequestReviewer {
            id: uuid.into(),
            login: login.into(),
            kind: tcode_protocol::PullRequestReviewerKind::User,
        };
        let (ana, bo, cy) = (user("{a}", "ana"), user("{b}", "bo"), user("{c}", "cy"));
        let answer = |held: &[&str]| json!({"reviewers": held.iter().map(|uuid| json!({"uuid": uuid})).collect::<Vec<_>>()});
        assert_eq!(
            reviewer_change(
                std::slice::from_ref(&ana),
                std::slice::from_ref(&cy),
                &answer(&["{a}", "{b}"])
            ),
            Outcome::Applied
        );
        let refused = Outcome::Rejected(Rejection::Refused { messages: vec![] });
        assert_eq!(
            reviewer_change(&[ana.clone(), bo.clone()], &[], &answer(&["{a}"])),
            Outcome::Partial {
                applied: vec!["ana".into()],
                unapplied: vec!["bo".into()],
                failure: Box::new(refused.clone()),
            }
        );
        assert_eq!(reviewer_change(&[], &[cy], &answer(&["{c}"])), refused);
    }
}
