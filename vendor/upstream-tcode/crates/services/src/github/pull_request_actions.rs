//! Writes to a linked pull request. Each request is sent once and answered in the domain's terms;
//! whatever a write may have changed is dropped from the reads, so the next read sees it.

use super::{
    Fresh, GitHubError, RequestOptions, RestRequest,
    graphql::Document,
    merge_message::remove_agent_credits,
    pull_request_reads::{
        MAX_PAGES, PullRequestReads, READ_TTL, Reader, is_revision, percent_encode, reaction_name,
    },
    pull_request_watch,
};
use crate::forge::{Anchoring, Moved};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use tcode_core::{
    pull_request::{PullRequestKey, PullRequestMergeMethod, PullRequestReviewDraftComment},
    pull_request_watch::CheckStatus,
    session::ReviewSide,
};
use tcode_protocol::{
    PullRequestAction, PullRequestActionResult as Outcome, PullRequestActionState,
    PullRequestCapabilities, PullRequestFile, PullRequestFileText, PullRequestMergeState,
    PullRequestPatch, PullRequestRejection as Rejection, PullRequestReviewVerdict,
    PullRequestReviewer, PullRequestReviewerKind,
};

/// What `gh pr merge` and `gh pr update-branch` read before they act, with the account's rights,
/// the repository's merge settings and the head's checks.
const ACTION_STATE: &str = "query PullRequestActionState($owner: String!, $name: String!, $number: Int!, $headRef: String!) { repository(owner: $owner, name: $name) { viewerPermission mergeCommitAllowed squashMergeAllowed rebaseMergeAllowed autoMergeAllowed pullRequest(number: $number) { id headRefOid isMergeQueueEnabled mergeStateStatus viewerCanUpdate viewerCanUpdateBranch autoMergeRequest { mergeMethod } mergeQueueEntry { position } baseRef { compare(headRef: $headRef) { behindBy } } commits(last: 1) { nodes { commit { statusCheckRollup { contexts(first: 100) { nodes { __typename ... on StatusContext { context state createdAt } ... on CheckRun { name status conclusion startedAt completedAt checkSuite { workflowRun { workflow { name } } } } } } } } } } } } }";

const MERGE_MESSAGE: &str = "query PullRequestMergeMessage($owner: String!, $name: String!, $number: Int!, $method: PullRequestMergeMethod!) { repository(owner: $owner, name: $name) { pullRequest(number: $number) { isMergeQueueEnabled headRefOid viewerMergeBodyText(mergeType: $method) } } }";

fn method_name(method: PullRequestMergeMethod) -> &'static str {
    match method {
        PullRequestMergeMethod::Merge => "MERGE",
        PullRequestMergeMethod::Squash => "SQUASH",
        PullRequestMergeMethod::Rebase => "REBASE",
    }
}

fn method_of(name: &str) -> Option<PullRequestMergeMethod> {
    match name {
        "MERGE" => Some(PullRequestMergeMethod::Merge),
        "SQUASH" => Some(PullRequestMergeMethod::Squash),
        "REBASE" => Some(PullRequestMergeMethod::Rebase),
        _ => None,
    }
}

/// The pull request's node id and its action state.
fn action_state(response: &Value) -> Result<(String, PullRequestActionState), GitHubError> {
    let repository = &response["data"]["repository"];
    let pr = &repository["pullRequest"];
    if pr.is_null() {
        return Err(GitHubError::NotFound);
    }
    let id = pr["id"].as_str().ok_or(GitHubError::InvalidResponse)?;
    let head = pr["headRefOid"]
        .as_str()
        .filter(|head| is_revision(head))
        .ok_or(GitHubError::InvalidResponse)?;
    let contexts: Vec<Value> = pr["commits"]["nodes"][0]["commit"]["statusCheckRollup"]["contexts"]
        ["nodes"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let checks = pull_request_watch::checks(&contexts);
    // Write or above merges and reverts (study: provider permission map).
    let writes = matches!(
        repository["viewerPermission"].as_str(),
        Some("ADMIN" | "MAINTAIN" | "WRITE")
    );
    let state = PullRequestActionState {
        head: head.to_owned(),
        merge_state: match pr["mergeStateStatus"].as_str() {
            Some("CLEAN") => PullRequestMergeState::Clean,
            Some("UNSTABLE") => PullRequestMergeState::Unstable,
            Some("HAS_HOOKS") => PullRequestMergeState::HasHooks,
            Some("BLOCKED") => PullRequestMergeState::Blocked,
            Some("BEHIND") => PullRequestMergeState::Behind,
            Some("DIRTY") => PullRequestMergeState::Dirty,
            Some("DRAFT") => PullRequestMergeState::Draft,
            _ => PullRequestMergeState::Unknown,
        },
        behind_by: pr["baseRef"]["compare"]["behindBy"].as_u64(),
        merge_queue: pr["isMergeQueueEnabled"].as_bool() == Some(true),
        merge_methods: [
            ("mergeCommitAllowed", PullRequestMergeMethod::Merge),
            ("squashMergeAllowed", PullRequestMergeMethod::Squash),
            ("rebaseMergeAllowed", PullRequestMergeMethod::Rebase),
        ]
        .into_iter()
        .filter(|(field, _)| repository[*field].as_bool() == Some(true))
        .map(|(_, method)| method)
        .collect(),
        auto_merge_allowed: repository["autoMergeAllowed"].as_bool() == Some(true),
        auto_merge: pr["autoMergeRequest"]["mergeMethod"]
            .as_str()
            .and_then(method_of),
        queued: pr["mergeQueueEntry"].is_object(),
        queue_position: pr["mergeQueueEntry"]["position"]
            .as_u64()
            .map(|position| position as u32),
        failing_checks: checks
            .iter()
            .filter(|check| check.status.failed())
            .map(|check| check.name.clone())
            .collect(),
        pending_checks: checks
            .iter()
            .filter(|check| check.status == CheckStatus::Pending)
            .count() as u32,
        can_update: pr["viewerCanUpdate"].as_bool() == Some(true),
        can_update_branch: pr["viewerCanUpdateBranch"].as_bool() == Some(true),
        can_merge: writes,
        capabilities: PullRequestCapabilities::ALL,
    };
    Ok((id.to_owned(), state))
}

/// What GitHub says became of a merge: merged, queued, or armed to merge later.
fn merge_outcome(pr: &Value) -> Outcome {
    if pr["merged"].as_bool() == Some(true) {
        Outcome::Applied
    } else if pr["mergeQueueEntry"].is_object() {
        Outcome::Queued {
            position: pr["mergeQueueEntry"]["position"]
                .as_u64()
                .map(|position| position as u32),
        }
    } else if let Some(method) = pr["autoMergeRequest"]["mergeMethod"]
        .as_str()
        .and_then(method_of)
    {
        Outcome::AutoMergeEnabled { method }
    } else {
        Outcome::Uncertain
    }
}

/// Whether the lines fall inside one hunk of the file on that side; GitHub refuses a review
/// comment anywhere else.
pub(crate) fn in_hunks(
    files: &[PullRequestFile],
    path: &str,
    side: ReviewSide,
    (start, end): (u32, u32),
) -> bool {
    let Some(PullRequestPatch::Hunks(hunks)) = files
        .iter()
        .find(|file| file.path == path)
        .map(|file| &file.patch)
    else {
        return false;
    };
    hunks
        .lines()
        .filter_map(|line| line.strip_prefix("@@ "))
        .any(|header| {
            let mut ranges = header.split_whitespace();
            let range = match side {
                ReviewSide::Old => ranges.next().and_then(|range| range.strip_prefix('-')),
                ReviewSide::New => ranges.nth(1).and_then(|range| range.strip_prefix('+')),
            };
            let Some((first, count)) =
                range.map(|range| range.split_once(',').unwrap_or((range, "1")))
            else {
                return false;
            };
            let (Ok(first), Ok(count)) = (first.parse::<u32>(), count.parse::<u32>()) else {
                return false;
            };
            count > 0 && first <= start && end < first + count
        })
}

/// Why a write was not sent, or why GitHub refused it.
pub fn rejection(error: GitHubError) -> Rejection {
    crate::forge::ForgeError::from(error).rejection()
}

/// The answer to a write that was sent. Without an answer, or with a server failure, GitHub may
/// have applied it; a refusal or a rate limit says it did not.
fn answered<T>(result: Result<T, GitHubError>) -> Outcome {
    match result {
        Ok(_) => Outcome::Applied,
        Err(
            GitHubError::Request
            | GitHubError::Deadline
            | GitHubError::BodyTooLarge
            | GitHubError::InvalidResponse,
        ) => Outcome::Uncertain,
        Err(GitHubError::Response { status, .. }) if status >= 500 => Outcome::Uncertain,
        Err(error) => Outcome::Rejected(rejection(error)),
    }
}

fn blank(text: &str) -> bool {
    text.trim().is_empty()
}

fn side(side: ReviewSide) -> &'static str {
    match side {
        ReviewSide::Old => "LEFT",
        ReviewSide::New => "RIGHT",
    }
}

impl Reader<'_> {
    fn mutation(
        &self,
        operation: &'static str,
        query: &str,
        variables: Value,
    ) -> Result<Value, GitHubError> {
        let Value::Object(variables) = variables else {
            unreachable!("mutation variables are an object")
        };
        self.api
            .graphql(
                &self.key.host,
                &Document {
                    query: query.to_owned(),
                    variables: variables.into_iter().collect::<BTreeMap<_, _>>(),
                },
                &RequestOptions {
                    operation,
                    ..self.options.clone()
                },
            )?
            .json()
    }

    fn mutate(&self, operation: &'static str, query: &str, variables: Value) -> Outcome {
        answered(self.mutation(operation, query, variables))
    }

    /// Read now, not from the cache: a merge or a branch update acts on the head as it stands.
    pub(super) fn action_state(&self) -> Result<(String, PullRequestActionState), GitHubError> {
        let response = self.query(
            "PullRequestActionState",
            ACTION_STATE.to_owned(),
            [(
                "headRef",
                json!(format!("refs/pull/{}/head", self.key.number)),
            )],
        )?;
        action_state(&response)
    }

    /// GitHub's merge or squash message without agents' credits, when that differs from it.
    fn cleaned_message(
        &self,
        head: &str,
        method: PullRequestMergeMethod,
    ) -> Result<Option<String>, Rejection> {
        let response = self
            .query(
                "PullRequestMergeMessage",
                MERGE_MESSAGE.to_owned(),
                [("method", json!(method_name(method)))],
            )
            .map_err(rejection)?;
        let pr = &response["data"]["repository"]["pullRequest"];
        let current = pr["headRefOid"].as_str().ok_or(Rejection::Failed)?;
        if current != head {
            return Err(Rejection::StaleHead {
                head: current.to_owned(),
            });
        }
        // A merge queue writes its own message and ignores one sent with the merge.
        if pr["isMergeQueueEnabled"].as_bool() == Some(true) {
            return Ok(None);
        }
        let message = pr["viewerMergeBodyText"]
            .as_str()
            .ok_or(Rejection::Failed)?;
        let cleaned = remove_agent_credits(message);
        Ok((cleaned != message).then_some(cleaned))
    }

    fn merge(
        &self,
        head: &str,
        method: PullRequestMergeMethod,
        auto: bool,
        remove_credits: bool,
    ) -> Result<Outcome, Rejection> {
        let (id, state) = self.action_state().map_err(rejection)?;
        if state.head != head {
            return Err(Rejection::StaleHead { head: state.head });
        }
        if !state.merge_methods.contains(&method) {
            return Err(Rejection::Invalid);
        }
        let mut input = json!({
            "pullRequestId": id,
            "mergeMethod": method_name(method),
            "expectedHeadOid": head,
        });
        // Rebasing keeps each commit's own message.
        if remove_credits
            && method != PullRequestMergeMethod::Rebase
            && !state.merge_queue
            && let Some(body) = self.cleaned_message(head, method)?
        {
            input["commitBody"] = json!(body);
        }
        // A merge queue takes a pull request through auto-merge, and auto-merge asked of one
        // that can merge now simply merges it, as `gh pr merge --auto` does.
        let arm = state.merge_queue
            || (auto
                && !matches!(
                    state.merge_state,
                    PullRequestMergeState::Clean
                        | PullRequestMergeState::HasHooks
                        | PullRequestMergeState::Unstable
                ));
        let (operation, field, input_type) = if arm {
            (
                "EnablePullRequestAutoMerge",
                "enablePullRequestAutoMerge",
                "EnablePullRequestAutoMergeInput",
            )
        } else {
            (
                "MergePullRequest",
                "mergePullRequest",
                "MergePullRequestInput",
            )
        };
        let query = format!(
            "mutation {operation}($input: {input_type}!) {{ {field}(input: $input) {{ pullRequest {{ merged mergeQueueEntry {{ position }} autoMergeRequest {{ mergeMethod }} }} }} }}"
        );
        Ok(
            match self.mutation(operation, &query, json!({ "input": input })) {
                Ok(answer) => merge_outcome(&answer["data"][field]["pullRequest"]),
                Err(error) => answered::<()>(Err(error)),
            },
        )
    }

    fn send(
        &self,
        operation: &'static str,
        method: &str,
        rest: &str,
        body: Option<&Value>,
    ) -> Outcome {
        answered(self.api.rest(
            &self.key.host,
            RestRequest {
                method,
                path: &self.rest_path(rest),
                body,
                if_none_match: None,
                accept: None,
                answers: &[],
            },
            &RequestOptions {
                operation,
                ..self.options.clone()
            },
        ))
    }

    /// The kind of node a client-given id names, once it is known to hang off this pull request:
    /// a mutation writes wherever the id belongs, whichever pull request the request names.
    fn subject(&self, id: &str) -> Result<String, Rejection> {
        let response = self
            .query(
                "PullRequestSubject",
                "query PullRequestSubject($owner: String!, $name: String!, $number: Int!, $subject: ID!) { repository(owner: $owner, name: $name) { pullRequest(number: $number) { id } } node(id: $subject) { __typename id ... on IssueComment { pullRequest { id } } ... on PullRequestReviewComment { pullRequest { id } } ... on PullRequestReview { pullRequest { id } } ... on PullRequestReviewThread { pullRequest { id } } } }".to_owned(),
                [("subject", json!(id))],
            )
            .map_err(rejection)?;
        let data = &response["data"];
        let expected = data["repository"]["pullRequest"]["id"].as_str();
        let node = &data["node"];
        let actual = node["pullRequest"]["id"].as_str().or(node["id"].as_str());
        if expected.is_none() || actual != expected {
            return Err(Rejection::ForeignSubject);
        }
        Ok(node["__typename"].as_str().unwrap_or_default().to_owned())
    }
}

/// One request of a write made of several, and the names it covers.
type Step<'a> = (Vec<String>, Box<dyn FnOnce() -> Outcome + 'a>);

/// Sends the steps in order until one is not applied; when some went through before it, the
/// answer is [`Outcome::Partial`] by the names each covers.
fn in_order<'a>(steps: impl IntoIterator<Item = Step<'a>>) -> Outcome {
    let mut applied = Vec::new();
    let mut steps = steps.into_iter();
    while let Some((names, send)) = steps.next() {
        let outcome = send();
        if outcome == Outcome::Applied {
            applied.extend(names);
            continue;
        }
        if applied.is_empty() {
            return outcome;
        }
        return Outcome::Partial {
            applied,
            unapplied: names
                .into_iter()
                .chain(steps.flat_map(|(names, _)| names))
                .collect(),
            failure: Box::new(outcome),
        };
    }
    Outcome::Applied
}

impl PullRequestReads {
    pub fn action_state(
        &self,
        key: &PullRequestKey,
    ) -> Result<Fresh<PullRequestActionState>, GitHubError> {
        let reader = self.reader(key)?;
        self.action_states.read(
            reader.read_key("action state"),
            || Ok((reader.action_state()?.1, READ_TTL)),
            |state| 256 + state.failing_checks.iter().map(String::len).sum::<usize>(),
        )
    }

    /// Every action but a review, whose content is the runtime's draft: see [`Self::submit_review`].
    pub fn act(&self, key: &PullRequestKey, action: &PullRequestAction) -> Outcome {
        self.drop_written(key);
        let outcome = self.write(key, action).unwrap_or_else(Outcome::Rejected);
        self.drop_written(key);
        // A lifecycle write changes the pull request itself: its head, files and state.
        if matches!(
            action,
            PullRequestAction::ReadyForReview
                | PullRequestAction::ConvertToDraft
                | PullRequestAction::Close
                | PullRequestAction::Reopen
                | PullRequestAction::Revert
                | PullRequestAction::UpdateBranch { .. }
                | PullRequestAction::Merge { .. }
                | PullRequestAction::DisableAutoMerge
        ) {
            self.invalidate(key);
        }
        outcome
    }

    fn write(
        &self,
        key: &PullRequestKey,
        action: &PullRequestAction,
    ) -> Result<Outcome, Rejection> {
        let reader = self.reader(key).map_err(rejection)?;
        Ok(match action {
            PullRequestAction::Comment { body } => {
                if blank(body) {
                    return Err(Rejection::Invalid);
                }
                let subject = self.node_id(&reader).map_err(rejection)?;
                reader.mutate(
                    "AddPullRequestComment",
                    "mutation AddPullRequestComment($subjectId: ID!, $body: String!) { addComment(input: { subjectId: $subjectId, body: $body }) { clientMutationId } }",
                    json!({"subjectId": subject, "body": body}),
                )
            }
            // A review takes the runtime's draft, and a stack write the stack's own route.
            PullRequestAction::SubmitReview { .. }
            | PullRequestAction::MergeStack { .. }
            | PullRequestAction::RebaseStack { .. } => return Err(Rejection::Invalid),
            PullRequestAction::ReplyToThread { thread_id, body } => {
                if blank(body) {
                    return Err(Rejection::Invalid);
                }
                reader.mutate(
                    "ReplyToPullRequestThread",
                    "mutation ReplyToPullRequestThread($threadId: ID!, $body: String!) { addPullRequestReviewThreadReply(input: { pullRequestReviewThreadId: $threadId, body: $body }) { comment { id } } }",
                    json!({"threadId": thread_id, "body": body}),
                )
            }
            PullRequestAction::ResolveThread {
                thread_id,
                resolved,
            } => {
                let (operation, query) = if *resolved {
                    (
                        "ResolvePullRequestThread",
                        "mutation ResolvePullRequestThread($threadId: ID!) { resolveReviewThread(input: { threadId: $threadId }) { thread { isResolved } } }",
                    )
                } else {
                    (
                        "UnresolvePullRequestThread",
                        "mutation UnresolvePullRequestThread($threadId: ID!) { unresolveReviewThread(input: { threadId: $threadId }) { thread { isResolved } } }",
                    )
                };
                reader.mutate(operation, query, json!({"threadId": thread_id}))
            }
            PullRequestAction::React {
                subject_id,
                content,
                reacted,
            } => {
                // The pull request's own id needs no proof that it is this pull request's.
                if *subject_id != self.node_id(&reader).map_err(rejection)?
                    && !matches!(
                        reader.subject(subject_id)?.as_str(),
                        "PullRequest"
                            | "IssueComment"
                            | "PullRequestReviewComment"
                            | "PullRequestReview"
                    )
                {
                    return Err(Rejection::Invalid);
                }
                let (operation, query) = if *reacted {
                    (
                        "AddPullRequestReaction",
                        "mutation AddPullRequestReaction($subjectId: ID!, $content: ReactionContent!) { addReaction(input: { subjectId: $subjectId, content: $content }) { reaction { content } } }",
                    )
                } else {
                    (
                        "RemovePullRequestReaction",
                        "mutation RemovePullRequestReaction($subjectId: ID!, $content: ReactionContent!) { removeReaction(input: { subjectId: $subjectId, content: $content }) { reaction { content } } }",
                    )
                };
                reader.mutate(
                    operation,
                    query,
                    json!({"subjectId": subject_id, "content": reaction_name(*content)}),
                )
            }
            PullRequestAction::EditComment { comment_id, body } => {
                if blank(body) {
                    return Err(Rejection::Invalid);
                }
                let (operation, query) = match reader.subject(comment_id)?.as_str() {
                    "IssueComment" => (
                        "EditPullRequestComment",
                        "mutation EditPullRequestComment($commentId: ID!, $body: String!) { updateIssueComment(input: { id: $commentId, body: $body }) { issueComment { id } } }",
                    ),
                    "PullRequestReviewComment" => (
                        "EditPullRequestReviewComment",
                        "mutation EditPullRequestReviewComment($commentId: ID!, $body: String!) { updatePullRequestReviewComment(input: { pullRequestReviewCommentId: $commentId, body: $body }) { pullRequestReviewComment { id } } }",
                    ),
                    _ => return Err(Rejection::Invalid),
                };
                reader.mutate(
                    operation,
                    query,
                    json!({"commentId": comment_id, "body": body}),
                )
            }
            PullRequestAction::Edit { title, body } => {
                if (title.is_none() && body.is_none()) || title.as_deref().is_some_and(blank) {
                    return Err(Rejection::Invalid);
                }
                let mut variables =
                    json!({"pullRequestId": self.node_id(&reader).map_err(rejection)?});
                // A variable left out puts no field in the input, which keeps GitHub's text;
                // an empty one would clear it.
                if let Some(title) = title {
                    variables["title"] = json!(title);
                }
                if let Some(body) = body {
                    variables["body"] = json!(body);
                }
                reader.mutate(
                    "EditPullRequest",
                    "mutation EditPullRequest($pullRequestId: ID!, $title: String, $body: String) { updatePullRequest(input: { pullRequestId: $pullRequestId, title: $title, body: $body }) { pullRequest { id } } }",
                    variables,
                )
            }
            PullRequestAction::SetLabels { add, remove } => {
                if add.is_empty() && remove.is_empty() {
                    return Err(Rejection::Invalid);
                }
                let issue = format!("issues/{}/labels", key.number);
                // A pull request is an issue to the labels API, which adds to what is there; it
                // takes a label off one request each, since the endpoint names one in its path.
                let adding = (!add.is_empty()).then(|| -> Step<'_> {
                    (
                        add.clone(),
                        Box::new(|| {
                            reader.send(
                                "AddPullRequestLabels",
                                "POST",
                                &issue,
                                Some(&json!({"labels": add})),
                            )
                        }),
                    )
                });
                let removing = remove.iter().map(|label| -> Step<'_> {
                    (
                        vec![label.clone()],
                        Box::new(|| {
                            reader.send(
                                "RemovePullRequestLabel",
                                "DELETE",
                                &format!("{issue}/{}", percent_encode(label)),
                                None,
                            )
                        }),
                    )
                });
                in_order(adding.into_iter().chain(removing))
            }
            PullRequestAction::SetReviewers { add, remove } => {
                if add.is_empty() && remove.is_empty() {
                    return Err(Rejection::Invalid);
                }
                let path = format!("pulls/{}/requested_reviewers", key.number);
                let names = |reviewers: &[PullRequestReviewer]| {
                    reviewers
                        .iter()
                        .map(|reviewer| reviewer.login.clone())
                        .collect::<Vec<_>>()
                };
                let request = |method: &'static str, reviewers: &[PullRequestReviewer]| {
                    let named = |kind| {
                        reviewers
                            .iter()
                            .filter(|reviewer| reviewer.kind == kind)
                            .map(|reviewer| reviewer.id.as_str())
                            .collect::<Vec<_>>()
                    };
                    // GitHub takes a request back from exactly whoever it was made of, so both
                    // methods send the same body.
                    reader.send(
                        "RequestPullRequestReviewers",
                        method,
                        &path,
                        Some(&json!({
                            "reviewers": named(PullRequestReviewerKind::User),
                            "team_reviewers": named(PullRequestReviewerKind::Team),
                        })),
                    )
                };
                let steps: [Step<'_>; 2] = [
                    (names(add), Box::new(|| request("POST", add))),
                    (names(remove), Box::new(|| request("DELETE", remove))),
                ];
                in_order(steps.into_iter().filter(|(names, _)| !names.is_empty()))
            }
            PullRequestAction::ReadyForReview
            | PullRequestAction::ConvertToDraft
            | PullRequestAction::Close
            | PullRequestAction::Reopen
            | PullRequestAction::DisableAutoMerge => {
                let (operation, field) = match action {
                    PullRequestAction::ReadyForReview => {
                        ("MarkPullRequestReady", "markPullRequestReadyForReview")
                    }
                    PullRequestAction::ConvertToDraft => {
                        ("ConvertPullRequestToDraft", "convertPullRequestToDraft")
                    }
                    PullRequestAction::Close => ("ClosePullRequest", "closePullRequest"),
                    PullRequestAction::Reopen => ("ReopenPullRequest", "reopenPullRequest"),
                    _ => ("DisablePullRequestAutoMerge", "disablePullRequestAutoMerge"),
                };
                let id = self.node_id(&reader).map_err(rejection)?;
                reader.mutate(
                    operation,
                    &format!(
                        "mutation {operation}($pullRequestId: ID!) {{ {field}(input: {{ pullRequestId: $pullRequestId }}) {{ clientMutationId }} }}"
                    ),
                    json!({"pullRequestId": id}),
                )
            }
            PullRequestAction::Revert => {
                let id = self.node_id(&reader).map_err(rejection)?;
                match reader.mutation(
                    "RevertPullRequest",
                    "mutation RevertPullRequest($pullRequestId: ID!) { revertPullRequest(input: { pullRequestId: $pullRequestId }) { revertPullRequest { number url } } }",
                    json!({"pullRequestId": id}),
                ) {
                    Ok(answer) => {
                        let opened = &answer["data"]["revertPullRequest"]["revertPullRequest"];
                        match (opened["number"].as_u64(), opened["url"].as_str()) {
                            (Some(number), Some(url)) => Outcome::Opened {
                                number,
                                url: url.to_owned(),
                            },
                            // Something answered, and may have opened one.
                            _ => Outcome::Uncertain,
                        }
                    }
                    Err(error) => answered::<()>(Err(error)),
                }
            }
            PullRequestAction::UpdateBranch { head, rebase } => {
                if !is_revision(head) {
                    return Err(Rejection::Invalid);
                }
                let (id, state) = reader.action_state().map_err(rejection)?;
                if state.head != *head {
                    return Err(Rejection::StaleHead { head: state.head });
                }
                if state.behind_by == Some(0) {
                    return Ok(Outcome::UpToDate);
                }
                reader.mutate(
                    "UpdatePullRequestBranch",
                    "mutation UpdatePullRequestBranch($pullRequestId: ID!, $expectedHeadOid: GitObjectID!, $updateMethod: PullRequestBranchUpdateMethod!) { updatePullRequestBranch(input: { pullRequestId: $pullRequestId, expectedHeadOid: $expectedHeadOid, updateMethod: $updateMethod }) { clientMutationId } }",
                    json!({
                        "pullRequestId": id,
                        "expectedHeadOid": head,
                        "updateMethod": if *rebase { "REBASE" } else { "MERGE" },
                    }),
                )
            }
            PullRequestAction::Merge {
                head,
                method,
                auto,
                remove_credits,
            } => {
                if !is_revision(head) {
                    return Err(Rejection::Invalid);
                }
                reader.merge(head, *method, *auto, *remove_credits)?
            }
        })
    }

    /// The whole review in one request, so nothing of it is visible until the verdict is sent.
    /// The head is read fresh first: a review anchored at `head` is not sent once the pull
    /// request has moved past it, and an unplaced comment is never sent.
    pub fn submit_review(
        &self,
        key: &PullRequestKey,
        verdict: PullRequestReviewVerdict,
        head: &str,
        body: &str,
        comments: &[PullRequestReviewDraftComment],
    ) -> Outcome {
        self.drop_written(key);
        let outcome = self
            .review(key, verdict, head, body, comments)
            .unwrap_or_else(Outcome::Rejected);
        self.drop_written(key);
        outcome
    }

    fn review(
        &self,
        key: &PullRequestKey,
        verdict: PullRequestReviewVerdict,
        head: &str,
        body: &str,
        comments: &[PullRequestReviewDraftComment],
    ) -> Result<Outcome, Rejection> {
        if !is_revision(head)
            || comments.iter().any(|comment| !comment.placed)
            || (verdict != PullRequestReviewVerdict::Approve && blank(body) && comments.is_empty())
        {
            return Err(Rejection::Invalid);
        }
        let reader = self.reader(key).map_err(rejection)?;
        self.revisions.invalidate(key);
        let current = self.revisions(&reader).map_err(rejection)?.head.clone();
        if current != head {
            return Err(Rejection::StaleHead { head: current });
        }
        let event = match verdict {
            PullRequestReviewVerdict::Comment => "COMMENT",
            PullRequestReviewVerdict::Approve => "APPROVE",
            PullRequestReviewVerdict::RequestChanges => "REQUEST_CHANGES",
        };
        let comments: Vec<_> = comments
            .iter()
            .map(|comment| {
                let mut line = json!({
                    "path": comment.path,
                    "line": comment.end_line,
                    "side": side(comment.side),
                    "body": comment.body,
                });
                if comment.start_line < comment.end_line {
                    line["start_line"] = json!(comment.start_line);
                    line["start_side"] = json!(side(comment.side));
                }
                line
            })
            .collect();
        Ok(reader.send(
            "SubmitPullRequestReview",
            "POST",
            &format!("pulls/{}/reviews", key.number),
            Some(&json!({
                "commit_id": current,
                "event": event,
                "body": body,
                "comments": comments,
            })),
        ))
    }

    /// The changed files at the pull request's current revisions, every page of them.
    fn diff(
        &self,
        key: &PullRequestKey,
    ) -> Result<(String, String, Vec<PullRequestFile>), GitHubError> {
        let first = self.files(key, None)?.value;
        let (base, head) = (first.base.clone(), first.head.clone());
        let mut files = first.files.clone();
        let mut next = first.next_page;
        while let Some(page) = next.filter(|page| (*page as usize) <= MAX_PAGES) {
            let more = self.files(key, Some(page))?.value;
            files.extend(more.files.iter().cloned());
            next = more.next_page;
        }
        Ok((base, head, files))
    }

    /// Whether GitHub takes a review comment on these lines: the pull request is still at
    /// `head`, and they lie inside one of the file's hunks on that side.
    pub fn commentable(
        &self,
        key: &PullRequestKey,
        head: &str,
        path: &str,
        side: ReviewSide,
        lines: (u32, u32),
    ) -> Result<Anchoring, GitHubError> {
        let (_, current, files) = self.diff(key)?;
        if current != head {
            return Ok(Anchoring::Moved);
        }
        Ok(if in_hunks(&files, path, side, lines) {
            Anchoring::InDiff
        } else {
            Anchoring::OutsideDiff
        })
    }

    /// The pull request's current head, and where each comment's lines are at it: the revision
    /// its side now shows when the lines read the same and are still in the diff, else `None`.
    pub fn reanchor(
        &self,
        key: &PullRequestKey,
        comments: &[PullRequestReviewDraftComment],
    ) -> Result<(String, Vec<Moved>), GitHubError> {
        let (base, head, files) = self.diff(key)?;
        let lines = |revision: &str, comment: &PullRequestReviewDraftComment| {
            let text = self.file_text(key, revision, &comment.path)?.value;
            Ok::<_, GitHubError>(match &*text {
                PullRequestFileText::Text(text) => {
                    let lines: Vec<_> = text.lines().collect();
                    lines
                        .get(comment.start_line as usize - 1..comment.end_line as usize)
                        .map(|lines| lines.join("\n"))
                }
                _ => None,
            })
        };
        let mut moved = Vec::new();
        for comment in comments {
            let revision = match comment.side {
                ReviewSide::Old => &base,
                ReviewSide::New => &head,
            };
            let kept = comment.placed
                && in_hunks(
                    &files,
                    &comment.path,
                    comment.side,
                    (comment.start_line, comment.end_line),
                )
                && (comment.revision == *revision || {
                    let before = lines(&comment.revision, comment)?;
                    before.is_some() && before == lines(revision, comment)?
                });
            moved.push((comment.id, kept.then(|| revision.clone())));
        }
        Ok((head, moved))
    }

    /// Everything a write may change, before it and again after whatever became of it, so no
    /// answer read while it was sent is kept.
    fn drop_written(&self, key: &PullRequestKey) {
        self.conversations.invalidate(key);
        self.replies.invalidate(key);
        self.labels.invalidate(key);
        self.reviewers.invalidate(key);
    }
}
