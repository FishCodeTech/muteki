//! GitHub behind the pull request host boundary.

use super::{
    CredentialError, Fresh, GitHubApi, GitHubError, pull_request_reads::PullRequestReads,
    pull_request_watch as watch, pull_requests::PullRequests, repository,
    stack_actions::merge_outcome,
};
use crate::forge::{
    Anchoring, Fingerprint, Forge, ForgeError, ForgeErrorKind, MergeSubmission, Moved, Repository,
    StackRebase, Summary, Tails,
};
use std::{
    collections::BTreeMap,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tcode_core::{
    pull_request::{
        GITHUB, HostTerms, PullRequestKey, PullRequestMergeMethod, PullRequestReviewDraftComment,
        PullRequestStackState,
    },
    pull_request_watch::{PullRequestRemark, PullRequestWatchRead},
    session::ReviewSide,
    settings::{HostSettings, HostStatus},
};
use tcode_protocol::{
    PullRequestAction, PullRequestActionResult as Outcome, PullRequestCapabilities,
    PullRequestFiles, PullRequestMedia, PullRequestRead, PullRequestReadResponse,
    PullRequestRejection as Rejection, PullRequestReviewVerdict, PullRequestStackHead,
};

pub struct GitHub {
    api: Arc<GitHubApi>,
    pull_requests: Arc<PullRequests>,
    reads: Arc<PullRequestReads>,
}

impl GitHub {
    pub fn new(api: Arc<GitHubApi>) -> Arc<Self> {
        Arc::new(Self {
            pull_requests: PullRequests::new(api.clone()),
            reads: PullRequestReads::new(api.clone()),
            api,
        })
    }
}

impl From<GitHubError> for ForgeError {
    fn from(error: GitHubError) -> Self {
        let description = error.to_string();
        let kind = match error {
            GitHubError::Credential(CredentialError::Disabled) => ForgeErrorKind::HostDisabled,
            GitHubError::Credential(_) => ForgeErrorKind::NoCredential,
            GitHubError::Unauthorized => ForgeErrorKind::Unauthorized,
            GitHubError::Paused { retry_at } => ForgeErrorKind::Paused { retry_at },
            GitHubError::RateLimited { retry_at, .. } => ForgeErrorKind::RateLimited { retry_at },
            GitHubError::NotFound => ForgeErrorKind::NotFound,
            GitHubError::Response { messages, .. } => ForgeErrorKind::Refused { messages },
            GitHubError::Request | GitHubError::InvalidResponse => ForgeErrorKind::Uncertain,
            GitHubError::Deadline => ForgeErrorKind::Deadline,
            GitHubError::BodyTooLarge => ForgeErrorKind::TooLarge,
            GitHubError::InvalidInput => ForgeErrorKind::InvalidInput,
            GitHubError::UnsupportedMedia => ForgeErrorKind::UnsupportedMedia,
        };
        Self { kind, description }
    }
}

fn neutral(repository: repository::Repository) -> Repository {
    Repository {
        locator: format!("{}/{}", repository.owner, repository.name),
        host: repository.host,
    }
}

impl Forge for GitHub {
    fn terms(&self, _: &PullRequestKey) -> &'static HostTerms {
        &GITHUB
    }

    fn capabilities(&self, _: &PullRequestKey) -> PullRequestCapabilities {
        PullRequestCapabilities::ALL
    }

    fn configure(&self, hosts: BTreeMap<String, HostSettings>) {
        self.api.credentials().configure(hosts);
    }

    fn credential_status(&self) -> BTreeMap<String, HostStatus> {
        self.api.credentials().discover()
    }

    fn forget_credential(&self, host: &str) {
        self.api.credentials().invalidate(host);
    }

    fn pull_request_url(&self, url: &str) -> Option<(PullRequestKey, String)> {
        repository::pull_request_url(url)
    }

    fn checkout_repository(&self, checkout: &crate::forge::Checkout<'_>) -> Option<Repository> {
        repository::resolve(checkout).map(neutral)
    }

    fn repository(&self, name: &str, host: &str) -> Option<Repository> {
        repository::selector(name, host).map(neutral)
    }

    fn url(&self, key: &PullRequestKey) -> Option<String> {
        repository::Repository::from_key(key).map(|repository| repository.url(key.number))
    }

    fn summary(&self, key: &PullRequestKey) -> Result<Summary, ForgeError> {
        Ok(self.pull_requests.summary(key)?)
    }

    fn stack(&self, key: &PullRequestKey) -> Result<PullRequestStackState, ForgeError> {
        Ok(self.pull_requests.stack(key)?)
    }

    fn read(
        &self,
        key: &PullRequestKey,
        read: PullRequestRead,
    ) -> Result<(PullRequestReadResponse, SystemTime), ForgeError> {
        fn reply<V: Clone>(
            fresh: Fresh<V>,
            wrap: impl FnOnce(V) -> PullRequestReadResponse,
        ) -> (PullRequestReadResponse, SystemTime) {
            (wrap((*fresh.value).clone()), fresh.expires_at)
        }
        let reads = &self.reads;
        Ok(match read {
            PullRequestRead::Files { cursor } => {
                let page = cursor
                    .map(|cursor| cursor.parse().map_err(|_| GitHubError::InvalidInput))
                    .transpose()?;
                reply(reads.files(key, page)?, |files| {
                    PullRequestReadResponse::Files(PullRequestFiles {
                        base: files.base,
                        head: files.head,
                        files: files.files,
                        next_cursor: files.next_page.map(|page| page.to_string()),
                        complete: files.complete,
                        changed_files: files.changed_files,
                    })
                })
            }
            PullRequestRead::FileText { revision, path } => reply(
                reads.file_text(key, &revision, &path)?,
                PullRequestReadResponse::FileText,
            ),
            PullRequestRead::Conversation => reply(reads.conversation(key)?, |conversation| {
                PullRequestReadResponse::Conversation(Box::new(conversation))
            }),
            PullRequestRead::ThreadReplies { thread_id, after } => reply(
                reads.thread_replies(key, &thread_id, &after)?,
                PullRequestReadResponse::ThreadReplies,
            ),
            PullRequestRead::ViewedFiles => reply(
                reads.viewed_files(key)?,
                PullRequestReadResponse::ViewedFiles,
            ),
            PullRequestRead::LabelCandidates => reply(
                reads.label_candidates(key)?,
                PullRequestReadResponse::LabelCandidates,
            ),
            PullRequestRead::ReviewerCandidates => reply(
                reads.reviewer_candidates(key)?,
                PullRequestReadResponse::ReviewerCandidates,
            ),
            PullRequestRead::ActionState => reply(
                reads.action_state(key)?,
                PullRequestReadResponse::ActionState,
            ),
            // Read for one confirmation and never kept: the write reads it again.
            PullRequestRead::StackState { rebase } => (
                PullRequestReadResponse::StackState(reads.stack_state(key, rebase)?),
                SystemTime::now(),
            ),
            PullRequestRead::Media { url, validator } => {
                let media = reads.media(key, &url, validator.as_deref())?;
                let expires_at = match &media {
                    PullRequestMedia::Image { expires_at, .. }
                    | PullRequestMedia::NotModified { expires_at } => {
                        UNIX_EPOCH + Duration::from_secs(*expires_at)
                    }
                    PullRequestMedia::External { .. } | PullRequestMedia::Unsupported => {
                        SystemTime::now()
                    }
                };
                (PullRequestReadResponse::Media(media), expires_at)
            }
        })
    }

    fn invalidate(&self, key: &PullRequestKey) {
        self.reads.invalidate(key);
    }

    fn account(&self, key: &PullRequestKey) -> Result<String, ForgeError> {
        Ok(self.reads.account(key)?)
    }

    fn set_viewed(
        &self,
        key: &PullRequestKey,
        paths: &[String],
        viewed: bool,
    ) -> Result<(), ForgeError> {
        Ok(self.reads.set_viewed(key, paths, viewed)?)
    }

    fn act(&self, key: &PullRequestKey, action: &PullRequestAction) -> Outcome {
        self.reads.act(key, action)
    }

    fn submit_review(
        &self,
        key: &PullRequestKey,
        verdict: PullRequestReviewVerdict,
        head: &str,
        body: &str,
        comments: &[PullRequestReviewDraftComment],
    ) -> Outcome {
        self.reads.submit_review(key, verdict, head, body, comments)
    }

    fn commentable(
        &self,
        key: &PullRequestKey,
        head: &str,
        path: &str,
        side: ReviewSide,
        lines: (u32, u32),
    ) -> Result<Anchoring, ForgeError> {
        Ok(self.reads.commentable(key, head, path, side, lines)?)
    }

    fn reanchor(
        &self,
        key: &PullRequestKey,
        comments: &[PullRequestReviewDraftComment],
    ) -> Result<(String, Vec<Moved>), ForgeError> {
        Ok(self.reads.reanchor(key, comments)?)
    }

    fn merge_stack(
        &self,
        key: &PullRequestKey,
        stack: u64,
        heads: &[PullRequestStackHead],
        method: PullRequestMergeMethod,
    ) -> MergeSubmission {
        self.reads.merge_stack(key, stack, heads, method)
    }

    fn merge_status(&self, key: &PullRequestKey, id: &str) -> Result<Option<Outcome>, ForgeError> {
        Ok(merge_outcome(&self.reads.merge_status(key, id)?))
    }

    fn plan_stack_rebase(
        &self,
        key: &PullRequestKey,
        stack: u64,
        heads: &[PullRequestStackHead],
    ) -> Result<StackRebase, Rejection> {
        let plan = self.reads.plan_stack_rebase(key, stack, heads)?;
        let layers = plan
            .layers
            .iter()
            .map(|layer| (layer.number, layer.branch.clone()))
            .collect();
        Ok(StackRebase::new(layers, move |progress| plan.run(progress)))
    }

    fn fingerprints(
        &self,
        keys: &[PullRequestKey],
    ) -> Vec<Result<Option<Fingerprint>, ForgeError>> {
        watch::fingerprints(&self.api, keys)
            .into_iter()
            .map(|fingerprint| Ok(fingerprint?))
            .collect()
    }

    fn watch_detail(&self, key: &PullRequestKey) -> Result<PullRequestWatchRead, ForgeError> {
        Ok(watch::detail(&self.api, key)?)
    }

    fn activity(
        &self,
        key: &PullRequestKey,
        tails: &mut Tails,
    ) -> Result<Option<Vec<PullRequestRemark>>, ForgeError> {
        Ok(watch::activity(&self.api, key, tails)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What the runtime acts on survives the crossing: when to read again, and a host turned
    /// off rather than a missing credential.
    #[test]
    fn github_errors_keep_their_meaning_across_the_boundary() {
        let retry_at = UNIX_EPOCH + Duration::from_secs(1_800_000_000);
        let limited = ForgeError::from(GitHubError::RateLimited {
            status: 403,
            retry_at,
        });
        assert_eq!(limited.retry_at(), Some(retry_at));
        assert_eq!(
            limited.rejection(),
            Rejection::RateLimited {
                retry_at: 1_800_000_000
            }
        );
        assert_eq!(
            ForgeError::from(GitHubError::Credential(CredentialError::Disabled)).rejection(),
            Rejection::HostDisabled
        );
    }
}
