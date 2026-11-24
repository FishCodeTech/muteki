//! Writes to a GitLab merge request. Each is sent once; an answer that cannot say whether the
//! server applied it is uncertain, never retried.

use super::{
    api::{Request, Response},
    reads::{Mr, PULL_REQUEST, award_name, merge_methods, nodes, text},
};
use crate::forge::{ForgeError, ForgeErrorKind, Step, answered, in_order};
use serde_json::{Value, json};
use tcode_core::{
    pull_request::{PullRequestMergeMethod, PullRequestReviewDraftComment},
    session::ReviewSide,
};
use tcode_protocol::{
    PullRequestAction, PullRequestActionResult as Outcome, PullRequestFile, PullRequestPatch,
    PullRequestRejection as Rejection, PullRequestReviewVerdict,
};

impl Mr<'_> {
    fn write(
        &self,
        method: &str,
        path: String,
        body: Option<Value>,
        operation: &'static str,
    ) -> Result<Response, ForgeError> {
        self.api.send(
            self.authority(),
            Request::write(method, path, body, operation),
        )
    }

    /// The merge request at `head`, or the head it moved to.
    pub(super) fn at_head(&self, head: &str) -> Result<Value, Rejection> {
        let mr = self.mr().map_err(ForgeError::rejection)?;
        let current = text(&mr, "sha").ok_or(Rejection::Failed)?;
        if current != head {
            return Err(Rejection::StaleHead { head: current });
        }
        Ok(mr)
    }

    /// A note id among this merge request's: the path names the merge request, so GitLab
    /// answers not found for another's.
    fn own_note(&self, id: &str) -> Result<u64, Rejection> {
        let id: u64 = id.parse().map_err(|_| Rejection::Invalid)?;
        self.get(self.path(&format!("/notes/{id}")), "Note")
            .map_err(|error| match error.kind {
                ForgeErrorKind::NotFound => Rejection::ForeignSubject,
                _ => error.rejection(),
            })?;
        Ok(id)
    }

    fn react(&self, subject: &str, name: &str, reacted: bool) -> Result<Outcome, Rejection> {
        let awards = if subject == PULL_REQUEST {
            self.path("/award_emoji")
        } else {
            self.path(&format!("/notes/{}/award_emoji", self.own_note(subject)?))
        };
        if reacted {
            return Ok(answered(self.write(
                "POST",
                awards,
                Some(json!({ "name": name })),
                "React",
            )));
        }
        // GitLab takes an award back by its id, never by its emoji.
        let viewer = self
            .viewer()
            .map_err(ForgeError::rejection)?
            .ok_or(Rejection::NoCredential)?;
        let (rows, _) = self
            .api
            .list(self.authority(), &awards, "Awards", 1)
            .map_err(ForgeError::rejection)?;
        let own = rows.iter().find_map(|award| {
            (award["name"].as_str() == Some(name)
                && award["user"]["username"]
                    .as_str()
                    .is_some_and(|user| user.eq_ignore_ascii_case(&viewer)))
            .then(|| award["id"].as_u64())
            .flatten()
        });
        Ok(match own {
            Some(id) => answered(self.write("DELETE", format!("{awards}/{id}"), None, "Unreact")),
            // Already taken back.
            None => Outcome::Applied,
        })
    }

    /// The title with or without GitLab's draft prefix.
    fn set_draft(&self, draft: bool) -> Result<Outcome, Rejection> {
        let mr = self.mr().map_err(ForgeError::rejection)?;
        let title = text(&mr, "title").ok_or(Rejection::Failed)?;
        let plain = undrafted(&title);
        let wanted = if draft {
            format!("Draft: {plain}")
        } else {
            plain.to_owned()
        };
        if mr["draft"].as_bool() == Some(draft) || wanted == title {
            return Ok(Outcome::Applied);
        }
        Ok(answered(self.write(
            "PUT",
            self.path(""),
            Some(json!({ "title": wanted })),
            "SetDraft",
        )))
    }

    fn rebase(&self, head: &str) -> Result<Outcome, Rejection> {
        let mr = self.at_head(head)?;
        if mr["diverged_commits_count"].as_u64() == Some(0) {
            return Ok(Outcome::UpToDate);
        }
        // GitLab rebases in the background and answers once it has the request.
        Ok(
            match answered(self.write("PUT", self.path("/rebase"), None, "Rebase")) {
                Outcome::Applied => Outcome::RebaseStarted,
                outcome => outcome,
            },
        )
    }

    fn merge(
        &self,
        head: &str,
        method: PullRequestMergeMethod,
        auto: bool,
        remove_credits: bool,
    ) -> Result<Outcome, Rejection> {
        self.at_head(head)?;
        let project = self
            .get(self.project_path(""), "Project")
            .map_err(ForgeError::rejection)?;
        if !merge_methods(&project).contains(&method) {
            return Err(Rejection::Invalid);
        }
        let squash = method == PullRequestMergeMethod::Squash;
        // Asked for in both of GitLab's names; a merge that is not to wait must say so, or
        // GitLab may keep it for the pipeline.
        let mut body = json!({
            "sha": head,
            "squash": squash,
            "merge_when_pipeline_succeeds": auto,
            "auto_merge": auto,
        });
        // Rebasing keeps each commit's own message.
        if remove_credits && method != PullRequestMergeMethod::Rebase {
            let (at, message) = self.merge_message(squash).map_err(ForgeError::rejection)?;
            if at.as_deref() != Some(head) {
                return Err(Rejection::StaleHead {
                    head: at.unwrap_or_default(),
                });
            }
            let message = message.ok_or(Rejection::Failed)?;
            let cleaned = crate::github::remove_agent_credits(&message);
            if cleaned != message {
                let field = if squash {
                    "squash_commit_message"
                } else {
                    "merge_commit_message"
                };
                body[field] = json!(cleaned);
            }
        }
        Ok(
            match self.write("PUT", self.path("/merge"), Some(body), "Merge") {
                Ok(response) => match response.json::<Value>() {
                    Ok(mr) if mr["state"].as_str() == Some("merged") => Outcome::Applied,
                    Ok(mr) if mr["merge_when_pipeline_succeeds"].as_bool() == Some(true) => {
                        Outcome::AutoMergeEnabled { method }
                    }
                    Ok(_) => Outcome::Applied,
                    Err(_) => Outcome::Uncertain,
                },
                Err(error) => answered::<()>(Err(error)),
            },
        )
    }
}

/// Adds or removes reviewers by username against the set GitLab holds when it applies the
/// change, so a reviewer someone else added meanwhile stays.
const SET_REVIEWERS: &str = "mutation($input: MergeRequestSetReviewersInput!) {
  mergeRequestSetReviewers(input: $input) { errors mergeRequest { reviewers { nodes { username } } } }
}";

/// Which of `names` a reviewer change in `mode` took, by the reviewers GitLab answered with, and
/// what became of the rest. A server that takes one reviewer at a time keeps its set without
/// an error, so only the answered set says whether the change happened.
pub(super) fn reviewer_change(
    mode: &str,
    names: &[String],
    payload: &Value,
) -> (Vec<String>, Option<Outcome>) {
    let refused = || {
        Outcome::Rejected(Rejection::Refused {
            messages: payload["errors"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|error| error.as_str().map(str::to_owned))
                .collect(),
        })
    };
    if payload["mergeRequest"].is_null() {
        return (Vec::new(), Some(refused()));
    }
    let now: Vec<String> = nodes(&payload["mergeRequest"]["reviewers"])
        .filter_map(|user| text(user, "username"))
        .collect();
    let (took, missed): (Vec<_>, Vec<_>) = names.iter().cloned().partition(|name| {
        now.iter().any(|held| held.eq_ignore_ascii_case(name)) == (mode == "APPEND")
    });
    (took, (!missed.is_empty()).then(refused))
}

/// A title without the prefixes GitLab reads as a draft: `Draft:`, `[Draft]`, `(Draft)` and
/// `Draft -`, in any case, however many lead it.
pub(super) fn undrafted(title: &str) -> &str {
    let mut rest = title.trim_start();
    loop {
        let lower = rest.to_ascii_lowercase();
        let Some(prefix) = ["draft:", "[draft]", "(draft)", "draft - "]
            .into_iter()
            .find(|prefix| lower.starts_with(prefix))
        else {
            return rest;
        };
        rest = rest[prefix.len()..].trim_start();
    }
}

fn blank(text: &str) -> bool {
    text.trim().is_empty()
}

/// A discussion id as GitLab writes it: hex.
fn discussion(id: &str) -> Result<&str, Rejection> {
    (!id.is_empty() && id.bytes().all(|b| b.is_ascii_hexdigit()))
        .then_some(id)
        .ok_or(Rejection::Invalid)
}

pub(super) fn act(mr: &Mr<'_>, action: &PullRequestAction) -> Outcome {
    let result = || -> Result<Outcome, Rejection> {
        Ok(match action {
            PullRequestAction::Comment { body } => {
                if blank(body) {
                    return Err(Rejection::Invalid);
                }
                answered(mr.write(
                    "POST",
                    mr.path("/notes"),
                    Some(json!({ "body": body })),
                    "Comment",
                ))
            }
            PullRequestAction::ReplyToThread { thread_id, body } => {
                if blank(body) {
                    return Err(Rejection::Invalid);
                }
                answered(mr.write(
                    "POST",
                    mr.path(&format!("/discussions/{}/notes", discussion(thread_id)?)),
                    Some(json!({ "body": body })),
                    "Reply",
                ))
            }
            PullRequestAction::ResolveThread {
                thread_id,
                resolved,
            } => answered(mr.write(
                "PUT",
                mr.path(&format!("/discussions/{}", discussion(thread_id)?)),
                Some(json!({ "resolved": resolved })),
                "Resolve",
            )),
            PullRequestAction::React {
                subject_id,
                content,
                reacted,
            } => mr.react(subject_id, award_name(*content), *reacted)?,
            PullRequestAction::EditComment { comment_id, body } => {
                if blank(body) || comment_id == PULL_REQUEST {
                    return Err(Rejection::Invalid);
                }
                let id = mr.own_note(comment_id)?;
                answered(mr.write(
                    "PUT",
                    mr.path(&format!("/notes/{id}")),
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
                    fields.insert("description".into(), json!(body));
                }
                if fields.is_empty() {
                    return Err(Rejection::Invalid);
                }
                answered(mr.write("PUT", mr.path(""), Some(Value::Object(fields)), "Edit"))
            }
            // Labels by name, which is GitLab's id for them in a write; a name never holds a
            // comma, which the write separates them by.
            PullRequestAction::SetLabels { add, remove } => {
                if (add.is_empty() && remove.is_empty())
                    || add
                        .iter()
                        .chain(remove)
                        .any(|label| blank(label) || label.contains(','))
                {
                    return Err(Rejection::Invalid);
                }
                let mut fields = serde_json::Map::new();
                if !add.is_empty() {
                    fields.insert("add_labels".into(), json!(add.join(",")));
                }
                if !remove.is_empty() {
                    fields.insert("remove_labels".into(), json!(remove.join(",")));
                }
                answered(mr.write("PUT", mr.path(""), Some(Value::Object(fields)), "SetLabels"))
            }
            // The additions in one change, then the removals, each by username.
            PullRequestAction::SetReviewers { add, remove } => {
                if add.is_empty() && remove.is_empty() {
                    return Err(Rejection::Invalid);
                }
                let logins = |reviewers: &[tcode_protocol::PullRequestReviewer]| -> Vec<String> {
                    reviewers
                        .iter()
                        .map(|reviewer| reviewer.login.clone())
                        .collect()
                };
                let changes = [("APPEND", logins(add)), ("REMOVE", logins(remove))];
                let mut applied = Vec::new();
                for (index, (mode, names)) in changes.iter().enumerate() {
                    if names.is_empty() {
                        continue;
                    }
                    let input = json!({ "input": {
                        "projectPath": mr.key.repository,
                        "iid": mr.key.number.to_string(),
                        "reviewerUsernames": names,
                        "operationMode": mode,
                    }});
                    let (took, failure) = match mr.api.graphql_write(
                        mr.authority(),
                        SET_REVIEWERS,
                        input,
                        "SetReviewers",
                    ) {
                        Ok(data) => reviewer_change(mode, names, &data["mergeRequestSetReviewers"]),
                        Err(error) => (Vec::new(), Some(answered::<()>(Err(error)))),
                    };
                    let Some(failure) = failure else {
                        applied.extend(took);
                        continue;
                    };
                    let unapplied: Vec<String> = names
                        .iter()
                        .filter(|name| !took.contains(name))
                        .cloned()
                        .chain(
                            changes[index + 1..]
                                .iter()
                                .flat_map(|(_, rest)| rest.clone()),
                        )
                        .collect();
                    applied.extend(took);
                    return Ok(if applied.is_empty() {
                        failure
                    } else {
                        Outcome::Partial {
                            applied,
                            unapplied,
                            failure: Box::new(failure),
                        }
                    });
                }
                Outcome::Applied
            }
            PullRequestAction::ReadyForReview => mr.set_draft(false)?,
            PullRequestAction::ConvertToDraft => mr.set_draft(true)?,
            PullRequestAction::Close | PullRequestAction::Reopen => {
                let event = if matches!(action, PullRequestAction::Close) {
                    "close"
                } else {
                    "reopen"
                };
                answered(mr.write(
                    "PUT",
                    mr.path(""),
                    Some(json!({ "state_event": event })),
                    "SetState",
                ))
            }
            PullRequestAction::UpdateBranch { head, rebase: true } => mr.rebase(head)?,
            PullRequestAction::Merge {
                head,
                method,
                auto,
                remove_credits,
            } => mr.merge(head, *method, *auto, *remove_credits)?,
            PullRequestAction::DisableAutoMerge => answered(mr.write(
                "POST",
                mr.path("/cancel_merge_when_pipeline_succeeds"),
                None,
                "CancelAutoMerge",
            )),
            PullRequestAction::UpdateBranch { rebase: false, .. } => {
                return Err(Rejection::Unsupported);
            }
            PullRequestAction::SubmitReview { .. }
            | PullRequestAction::Revert
            | PullRequestAction::MergeStack { .. }
            | PullRequestAction::RebaseStack { .. } => return Err(Rejection::Invalid),
        })
    };
    result().unwrap_or_else(Outcome::Rejected)
}

/// The old and new line numbers of a line on one side of a file's hunks: an unchanged line has
/// both, an added one only the new, a removed one only the old. `None` outside the hunks.
pub(super) fn lines_at(
    hunks: &str,
    side: ReviewSide,
    line: u32,
) -> Option<(Option<u32>, Option<u32>)> {
    let (mut old, mut new) = (0u32, 0u32);
    for raw in hunks.lines() {
        if let Some(header) = raw.strip_prefix("@@ ") {
            let mut ranges = header.split_whitespace();
            let start = |range: Option<&str>, sign: char| {
                range?
                    .strip_prefix(sign)?
                    .split(',')
                    .next()?
                    .parse::<u32>()
                    .ok()
            };
            // Each line counts itself in, so the counts stand one before the hunk's first.
            old = start(ranges.next(), '-')?.saturating_sub(1);
            new = start(ranges.next(), '+')?.saturating_sub(1);
            continue;
        }
        let (at, here) = match raw.chars().next() {
            Some('+') => {
                new += 1;
                ((None, Some(new)), side == ReviewSide::New && new == line)
            }
            Some('-') => {
                old += 1;
                ((Some(old), None), side == ReviewSide::Old && old == line)
            }
            Some('\\') => continue,
            _ => {
                old += 1;
                new += 1;
                let here = match side {
                    ReviewSide::Old => old == line,
                    ReviewSide::New => new == line,
                };
                ((Some(old), Some(new)), here)
            }
        };
        if here {
            return Some(at);
        }
    }
    None
}

/// GitLab has no review: its line comments go first, each a discussion on its line at the
/// diff the merge request has now, then the summary as a note, then the approval, so a review
/// cut short is never an approval. One that stops part way is [`Outcome::Partial`].
pub(super) fn submit_review(
    mr: &Mr<'_>,
    files: &[PullRequestFile],
    verdict: PullRequestReviewVerdict,
    head: &str,
    body: &str,
    comments: &[PullRequestReviewDraftComment],
) -> Outcome {
    if verdict == PullRequestReviewVerdict::RequestChanges
        || (verdict == PullRequestReviewVerdict::Comment && blank(body) && comments.is_empty())
    {
        return Outcome::Rejected(Rejection::Invalid);
    }
    let current = match mr.at_head(head) {
        Ok(current) => current,
        Err(rejection) => return Outcome::Rejected(rejection),
    };
    let refs = &current["diff_refs"];
    let (Some(base), Some(start)) = (text(refs, "base_sha"), text(refs, "start_sha")) else {
        return Outcome::Rejected(Rejection::Failed);
    };
    let mut steps: Vec<Step> = Vec::new();
    for comment in comments {
        let file = files.iter().find(|file| file.path == comment.path);
        let Some((old_line, new_line)) = file.and_then(|file| match &file.patch {
            PullRequestPatch::Hunks(hunks) => lines_at(hunks, comment.side, comment.end_line),
            _ => None,
        }) else {
            return Outcome::Rejected(Rejection::Invalid);
        };
        let old_path = file
            .and_then(|file| file.previous_path.clone())
            .unwrap_or_else(|| comment.path.clone());
        let mut position = json!({
            "position_type": "text",
            "base_sha": base,
            "start_sha": start,
            "head_sha": head,
            "new_path": comment.path,
            "old_path": old_path,
        });
        if let Some(line) = old_line {
            position["old_line"] = json!(line);
        }
        if let Some(line) = new_line {
            position["new_line"] = json!(line);
        }
        let request = json!({ "body": comment.body, "position": position });
        steps.push((
            vec![format!("{}:{}", comment.path, comment.end_line)],
            Box::new(move || {
                answered(mr.write(
                    "POST",
                    mr.path("/discussions"),
                    Some(request),
                    "ReviewComment",
                ))
            }),
        ));
    }
    if !blank(body) {
        let request = json!({ "body": body });
        steps.push((
            vec!["summary".into()],
            Box::new(move || {
                answered(mr.write("POST", mr.path("/notes"), Some(request), "ReviewSummary"))
            }),
        ));
    }
    if verdict == PullRequestReviewVerdict::Approve {
        let request = json!({ "sha": head });
        steps.push((
            vec!["approval".into()],
            Box::new(move || {
                answered(mr.write("POST", mr.path("/approve"), Some(request), "Approve"))
            }),
        ));
    }
    in_order(steps)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A reviewer change holds only as far as GitLab's answered set shows it: a server that takes
    /// one reviewer keeps the second out without an error, and an answer without the merge
    /// request is no removal.
    #[test]
    fn a_reviewer_change_is_what_gitlab_answers() {
        let names = |list: &[&str]| list.iter().map(|name| name.to_string()).collect::<Vec<_>>();
        let answer = |held: &[&str]| {
            json!({"errors": [], "mergeRequest": {"reviewers": {"nodes":
                held.iter().map(|name| json!({"username": name})).collect::<Vec<_>>()}}})
        };
        assert_eq!(
            reviewer_change("APPEND", &names(&["ana", "Bo"]), &answer(&["ana", "bo"])),
            (names(&["ana", "Bo"]), None)
        );
        let (took, failure) = reviewer_change("APPEND", &names(&["ana", "bo"]), &answer(&["ana"]));
        assert_eq!(took, names(&["ana"]));
        assert_eq!(
            failure,
            Some(Outcome::Rejected(Rejection::Refused { messages: vec![] }))
        );
        assert_eq!(
            reviewer_change("REMOVE", &names(&["ana"]), &answer(&["bo"])),
            (names(&["ana"]), None)
        );
        let (took, failure) = reviewer_change(
            "REMOVE",
            &names(&["ana"]),
            &json!({"errors": ["Not allowed"], "mergeRequest": null}),
        );
        assert!(took.is_empty());
        assert_eq!(
            failure,
            Some(Outcome::Rejected(Rejection::Refused {
                messages: vec!["Not allowed".into()]
            }))
        );
    }

    /// GitLab reads a title as a draft by any of its prefixes, so marking ready removes every
    /// one and nothing else.
    #[test]
    fn a_draft_is_its_titles_prefix() {
        assert_eq!(undrafted("Draft: [Draft] (draft) Fix it"), "Fix it");
        assert_eq!(undrafted("draft - Fix it"), "Fix it");
        assert_eq!(undrafted("Drafting the docs"), "Drafting the docs");
    }

    /// A line comment on an unchanged line names both sides' lines, as GitLab requires; on an
    /// added or removed line only its own side's.
    #[test]
    fn a_comment_names_the_lines_gitlab_places_it_by() {
        let hunks = "@@ -10,3 +10,4 @@ fn x\n a\n-b\n+c\n+d\n e\n";
        assert_eq!(
            lines_at(hunks, ReviewSide::New, 10),
            Some((Some(10), Some(10)))
        );
        assert_eq!(lines_at(hunks, ReviewSide::Old, 11), Some((Some(11), None)));
        assert_eq!(lines_at(hunks, ReviewSide::New, 12), Some((None, Some(12))));
        assert_eq!(
            lines_at(hunks, ReviewSide::New, 13),
            Some((Some(12), Some(13)))
        );
        assert_eq!(lines_at(hunks, ReviewSide::New, 20), None);
    }
}
