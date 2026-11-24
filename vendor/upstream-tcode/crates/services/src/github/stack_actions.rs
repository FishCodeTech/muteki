//! Writes to a GitHub native stack: the asynchronous merge of a layer with the layers below it,
//! and the rebase of every unmerged layer onto the one below it. Each re-reads the stack first
//! and goes ahead only at exactly the layers and heads the user confirmed.

use super::{
    GitHubError, RequestOptions, RestRequest,
    api::Authentication,
    graphql::Document,
    pull_request_actions::rejection,
    pull_request_reads::{PullRequestReads, Reader, is_revision, percent_encode},
    pull_requests::decode_stack,
    stack_rebase::{self, Identity, RebaseLayer},
};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use tcode_core::pull_request::{
    PullRequestKey, PullRequestMergeMethod, PullRequestState, StackRebaseStep,
};
use tcode_protocol::{
    PullRequestActionResult as Outcome, PullRequestRejection as Rejection,
    PullRequestStackActionState, PullRequestStackHead, PullRequestStackLayerState,
    PullRequestStackPushAccess,
};

/// What GitHub said of an asynchronous merge, on submission or when asked again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MergeStatus {
    Pending { id: String },
    Merged,
    Enqueued,
    Failed { message: Option<String> },
}

pub use crate::forge::MergeSubmission;

/// A rebase the host may start: the stack's base, its unmerged layers bottom first with the
/// heads that were reviewed, and where to fetch and push them.
#[derive(Clone)]
pub struct RebasePlan {
    pub base: String,
    pub layers: Vec<RebaseLayer>,
    pub remote: String,
    token: String,
    pub identity: Identity,
}

struct FreshStack {
    number: u64,
    base: String,
    layers: Vec<PullRequestStackLayerState>,
}

fn method_name(method: PullRequestMergeMethod) -> &'static str {
    match method {
        PullRequestMergeMethod::Merge => "merge",
        PullRequestMergeMethod::Squash => "squash",
        PullRequestMergeMethod::Rebase => "rebase",
    }
}

fn merge_status(body: &Value) -> Option<MergeStatus> {
    let details = &body["details"];
    Some(match body["status"].as_str()? {
        "pending" => MergeStatus::Pending {
            id: details["uuid"]
                .as_str()
                .filter(|id| !id.is_empty())?
                .to_owned(),
        },
        "merged" => MergeStatus::Merged,
        "enqueued" => MergeStatus::Enqueued,
        "failed" => MergeStatus::Failed {
            message: details["message"]
                .as_str()
                .filter(|message| !message.trim().is_empty())
                .map(str::to_owned),
        },
        _ => return None,
    })
}

/// What a finished asynchronous merge comes to.
pub fn merge_outcome(status: &MergeStatus) -> Option<Outcome> {
    Some(match status {
        MergeStatus::Pending { .. } => return None,
        MergeStatus::Merged => Outcome::Applied,
        MergeStatus::Enqueued => Outcome::Queued { position: None },
        MergeStatus::Failed { message } => Outcome::Rejected(Rejection::Refused {
            messages: message.iter().cloned().collect(),
        }),
    })
}

/// The layers a write goes ahead with must be exactly `expected`, each at its confirmed head:
/// a layer added, removed or reordered, or a head moved, rejects it before anything is sent.
fn check_heads(
    layers: &[&PullRequestStackLayerState],
    expected: &[PullRequestStackHead],
) -> Result<(), Rejection> {
    if layers.len() != expected.len()
        || layers
            .iter()
            .zip(expected)
            .any(|(layer, expected)| layer.number != expected.number)
    {
        return Err(Rejection::StackChanged);
    }
    for (layer, expected) in layers.iter().zip(expected) {
        match &layer.head {
            Some(head) if *head == expected.head => {}
            Some(head) => {
                return Err(Rejection::LayerChanged {
                    number: layer.number,
                    expected: expected.head.clone(),
                    actual: head.clone(),
                });
            }
            None => return Err(Rejection::StackChanged),
        }
    }
    Ok(())
}

impl Reader<'_> {
    /// The stack the pull request is in, read now: `None` when it is in none. The listing names
    /// the stack; its detail carries each layer's title as well as its head and state.
    fn fresh_stack(&self) -> Result<Option<FreshStack>, GitHubError> {
        let read = |path: String| {
            self.api.rest(
                &self.key.host,
                RestRequest::get(&self.rest_path(&path)),
                &RequestOptions {
                    operation: "PullRequestStackState",
                    ..self.options.clone()
                },
            )
        };
        let raw: Value = match read(format!("stacks?pull_request={}", self.key.number)) {
            Ok(response) => response.json()?,
            Err(GitHubError::NotFound) => return Ok(None),
            Err(error) => return Err(error),
        };
        let Some(listed) = raw.as_array().ok_or(GitHubError::InvalidResponse)?.first() else {
            return Ok(None);
        };
        let number = listed["number"]
            .as_u64()
            .ok_or(GitHubError::InvalidResponse)?;
        let stack: Value = read(format!("stacks/{number}"))?.json()?;
        let topology =
            decode_stack(&stack, &self.repository).ok_or(GitHubError::InvalidResponse)?;
        let rows = stack["pull_requests"]
            .as_array()
            .ok_or(GitHubError::InvalidResponse)?;
        let layers = topology
            .layers
            .iter()
            .zip(rows)
            .map(|(layer, row)| PullRequestStackLayerState {
                number: layer.number,
                title: row["title"].as_str().unwrap_or_default().to_owned(),
                head_branch: layer.head_branch.clone(),
                head: row["head"]["sha"]
                    .as_str()
                    .filter(|sha| is_revision(sha))
                    .map(str::to_owned),
                state: layer.state,
                draft: row["draft"].as_bool() == Some(true),
                push: None,
            })
            .collect();
        Ok(Some(FreshStack {
            number: topology.number,
            base: topology.base,
            layers,
        }))
    }

    /// Whether the account may push to each layer's branch: a write role on its repository, or
    /// a fork's branch that allows maintainers to push.
    fn push_access(
        &self,
        numbers: &[u64],
    ) -> Result<BTreeMap<u64, PullRequestStackPushAccess>, GitHubError> {
        if numbers.is_empty() {
            return Ok(BTreeMap::new());
        }
        let fields: String = numbers
            .iter()
            .map(|number| {
                format!(
                    " pr{number}: pullRequest(number: {number}) {{ headRepository {{ viewerPermission }} maintainerCanModify }}"
                )
            })
            .collect();
        let document = Document {
            query: format!(
                "query PullRequestStackPushAccess($owner: String!, $name: String!) {{ repository(owner: $owner, name: $name) {{{fields} }} }}"
            ),
            variables: BTreeMap::from([
                ("owner".to_owned(), json!(self.repository.owner)),
                ("name".to_owned(), json!(self.repository.name)),
            ]),
        };
        let response: Value = self
            .api
            .graphql(
                &self.key.host,
                &document,
                &RequestOptions {
                    operation: "PullRequestStackPushAccess",
                    ..self.options.clone()
                },
            )?
            .json()?;
        Ok(numbers
            .iter()
            .map(|number| {
                let pr = &response["data"]["repository"][format!("pr{number}")];
                let write = matches!(
                    pr["headRepository"]["viewerPermission"].as_str(),
                    Some("ADMIN" | "MAINTAIN" | "WRITE")
                );
                let access = if pr["headRepository"].is_null() {
                    PullRequestStackPushAccess::Denied
                } else if write {
                    PullRequestStackPushAccess::Write
                } else if pr["maintainerCanModify"].as_bool() == Some(true) {
                    PullRequestStackPushAccess::MaintainerCanModify
                } else {
                    PullRequestStackPushAccess::Denied
                };
                (*number, access)
            })
            .collect())
    }

    fn token(&self) -> Result<String, GitHubError> {
        match &self.options.authentication {
            Authentication::Pinned(credential) => Ok(credential.token.clone()),
            _ => Err(GitHubError::Request),
        }
    }
}

impl PullRequestReads {
    /// The stack as GitHub has it now, with what a merge of `key` or a rebase would meet.
    pub fn stack_state(
        &self,
        key: &PullRequestKey,
        rebase: bool,
    ) -> Result<PullRequestStackActionState, GitHubError> {
        let reader = self.reader(key)?;
        let stack = reader.fresh_stack()?.ok_or(GitHubError::NotFound)?;
        let (_, action) = reader.action_state()?;
        let mut layers = stack.layers;
        let mut git_identity = None;
        if rebase {
            let unmerged: Vec<_> = layers
                .iter()
                .filter(|layer| layer.state != PullRequestState::Merged)
                .map(|layer| layer.number)
                .collect();
            let access = reader.push_access(&unmerged)?;
            for layer in &mut layers {
                layer.push = access.get(&layer.number).copied();
            }
            git_identity = Some(stack_rebase::identity().is_some());
        }
        Ok(PullRequestStackActionState {
            stack: stack.number,
            base: stack.base,
            layers,
            merge_methods: action.merge_methods,
            merge_queue: action.merge_queue,
            can_merge: action.can_merge,
            git_identity,
        })
    }

    /// Submits GitHub's asynchronous merge of `key` and every unmerged layer below it, once a
    /// fresh read of the stack shows exactly the confirmed scope at its confirmed heads, every
    /// layer of it open and ready for review. A conflict naming an operation already running is
    /// followed instead of submitting another.
    pub fn merge_stack(
        &self,
        key: &PullRequestKey,
        stack: u64,
        heads: &[PullRequestStackHead],
        method: PullRequestMergeMethod,
    ) -> MergeSubmission {
        self.invalidate(key);
        let submission = self
            .submit_stack_merge(key, stack, heads, method)
            .unwrap_or_else(|rejection| MergeSubmission::Done(Outcome::Rejected(rejection)));
        self.invalidate(key);
        submission
    }

    fn submit_stack_merge(
        &self,
        key: &PullRequestKey,
        stack: u64,
        heads: &[PullRequestStackHead],
        method: PullRequestMergeMethod,
    ) -> Result<MergeSubmission, Rejection> {
        let reader = self.reader(key).map_err(rejection)?;
        let fresh = reader
            .fresh_stack()
            .map_err(rejection)?
            .ok_or(Rejection::StackChanged)?;
        let Some(target) = fresh
            .layers
            .iter()
            .position(|layer| layer.number == key.number)
        else {
            return Err(Rejection::StackChanged);
        };
        if fresh.number != stack {
            return Err(Rejection::StackChanged);
        }
        let selected = &fresh.layers[target];
        if selected.state != PullRequestState::Open {
            return Err(Rejection::LayerNotOpen {
                number: selected.number,
                state: selected.state,
            });
        }
        let scope: Vec<_> = fresh.layers[..=target]
            .iter()
            .filter(|layer| layer.state != PullRequestState::Merged)
            .collect();
        check_heads(&scope, heads)?;
        if let Some(layer) = scope
            .iter()
            .find(|layer| layer.state != PullRequestState::Open)
        {
            return Err(Rejection::LayerNotOpen {
                number: layer.number,
                state: layer.state,
            });
        }
        if let Some(layer) = scope.iter().find(|layer| layer.draft) {
            return Err(Rejection::LayerDraft {
                number: layer.number,
            });
        }
        let layers: Vec<_> = scope.iter().map(|layer| layer.number).collect();
        let head = selected.head.clone().ok_or(Rejection::StackChanged)?;
        let body = json!({
            "merge_method": method_name(method),
            "merge_action": "default",
            "sha": head,
        });
        let answer = reader.api.rest(
            &key.host,
            RestRequest {
                method: "PUT",
                path: &reader.rest_path(&format!("pulls/{}/merge-async", key.number)),
                body: Some(&body),
                if_none_match: None,
                accept: None,
                answers: &[409],
            },
            &RequestOptions {
                operation: "MergePullRequestStack",
                ..reader.options.clone()
            },
        );
        let response = match answer {
            Ok(response) => response,
            Err(
                GitHubError::Request
                | GitHubError::Deadline
                | GitHubError::BodyTooLarge
                | GitHubError::InvalidResponse,
            ) => return Ok(MergeSubmission::Done(Outcome::Uncertain)),
            Err(GitHubError::Response { status, .. }) if status >= 500 => {
                return Ok(MergeSubmission::Done(Outcome::Uncertain));
            }
            Err(error) => return Err(rejection(error)),
        };
        let parsed: Value = response.json().unwrap_or_default();
        if response.status == 409 {
            // GitHub keeps one merge request per stack; the one it names is followed, never
            // a second submitted.
            return match parsed["details"]["uuid"]
                .as_str()
                .or(parsed["uuid"].as_str())
                .filter(|id| !id.is_empty())
            {
                Some(id) => Ok(MergeSubmission::Following {
                    id: id.to_owned(),
                    adopted: true,
                    layers,
                }),
                None => Err(Rejection::MergeRunning),
            };
        }
        Ok(match merge_status(&parsed) {
            Some(MergeStatus::Pending { id }) => MergeSubmission::Following {
                id,
                adopted: false,
                layers,
            },
            Some(status) => {
                MergeSubmission::Done(merge_outcome(&status).unwrap_or(Outcome::Uncertain))
            }
            // Something answered, and the merge may be under way.
            None => MergeSubmission::Done(Outcome::Uncertain),
        })
    }

    /// Asks GitHub once what became of operation `id`.
    pub fn merge_status(&self, key: &PullRequestKey, id: &str) -> Result<MergeStatus, GitHubError> {
        let reader = self.reader(key)?;
        let raw: Value = reader
            .api
            .rest(
                &key.host,
                RestRequest::get(&reader.rest_path(&format!(
                    "pulls/{}/merge-async/{}",
                    key.number,
                    percent_encode(id)
                ))),
                &RequestOptions {
                    operation: "PullRequestStackMergeStatus",
                    ..reader.options.clone()
                },
            )?
            .json()?;
        merge_status(&raw).ok_or(GitHubError::InvalidResponse)
    }

    /// The rebase `key`'s stack may start: a fresh read shows exactly the confirmed unmerged
    /// layers at their confirmed heads, all open, the account may push to every one of them, and
    /// the host's Git has a name and an email to commit with. Nothing is written.
    pub fn plan_stack_rebase(
        &self,
        key: &PullRequestKey,
        stack: u64,
        heads: &[PullRequestStackHead],
    ) -> Result<RebasePlan, Rejection> {
        let reader = self.reader(key).map_err(rejection)?;
        let fresh = reader
            .fresh_stack()
            .map_err(rejection)?
            .ok_or(Rejection::StackChanged)?;
        if fresh.number != stack {
            return Err(Rejection::StackChanged);
        }
        let unmerged: Vec<_> = fresh
            .layers
            .iter()
            .filter(|layer| layer.state != PullRequestState::Merged)
            .collect();
        check_heads(&unmerged, heads)?;
        if let Some(layer) = unmerged
            .iter()
            .find(|layer| layer.state != PullRequestState::Open)
        {
            return Err(Rejection::LayerNotOpen {
                number: layer.number,
                state: layer.state,
            });
        }
        let numbers: Vec<_> = unmerged.iter().map(|layer| layer.number).collect();
        let access = reader.push_access(&numbers).map_err(rejection)?;
        let denied: Vec<_> = numbers
            .iter()
            .copied()
            .filter(|number| {
                access
                    .get(number)
                    .is_none_or(|access| *access == PullRequestStackPushAccess::Denied)
            })
            .collect();
        if !denied.is_empty() {
            return Err(Rejection::NoPushAccess { numbers: denied });
        }
        let identity = stack_rebase::identity().ok_or(Rejection::NoGitIdentity)?;
        Ok(RebasePlan {
            base: fresh.base,
            layers: unmerged
                .iter()
                .map(|layer| RebaseLayer {
                    number: layer.number,
                    branch: layer.head_branch.clone(),
                    head: layer.head.clone().unwrap_or_default(),
                })
                .collect(),
            remote: format!(
                "https://{}/{}/{}.git",
                key.host, reader.repository.owner, reader.repository.name
            ),
            token: reader.token().map_err(rejection)?,
            identity,
        })
    }
}

impl RebasePlan {
    /// Runs the planned rebase, telling `progress` each layer's step, and answers how it ended.
    pub fn run(&self, progress: impl FnMut(usize, StackRebaseStep)) -> Outcome {
        let steps = stack_rebase::cascade(
            &self.remote,
            &self.token,
            &self.identity,
            &self.base,
            &self.layers,
            progress,
        );
        rebase_outcome(&self.layers, &steps)
    }
}

/// What the layers' final steps come to.
pub fn rebase_outcome(layers: &[RebaseLayer], steps: &[StackRebaseStep]) -> Outcome {
    let numbers = |wanted: fn(&StackRebaseStep) -> bool| -> Vec<u64> {
        layers
            .iter()
            .zip(steps)
            .filter(|(_, step)| wanted(step))
            .map(|(layer, _)| layer.number)
            .collect()
    };
    let pushed = numbers(|step| matches!(step, StackRebaseStep::Pushed { .. }));
    match layers
        .iter()
        .zip(steps)
        .find_map(|(layer, step)| match step {
            StackRebaseStep::Failed { reason } => Some((layer.number, reason.clone())),
            _ => None,
        }) {
        Some((failed, reason)) => Outcome::RebaseStopped {
            pushed,
            failed,
            reason,
            untouched: numbers(|step| matches!(step, StackRebaseStep::NotStarted)),
        },
        None => Outcome::Rebased {
            pushed,
            current: numbers(|step| matches!(step, StackRebaseStep::AlreadyCurrent)),
        },
    }
}
