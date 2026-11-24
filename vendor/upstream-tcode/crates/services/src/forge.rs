//! The pull request host boundary: everything the runtime asks of a host that keeps pull
//! requests. A host's ids, cursors, query language, credentials and error shapes stay behind
//! it; what crosses is Tcode's own model. Each host of a kind is served by that kind's
//! implementation. Call blocking entries via HostCx::unblock.

pub(crate) mod anchors;
pub(crate) mod checkout;
pub use checkout::Checkout;
mod http;
mod rate_limit;
#[cfg(test)]
pub(crate) mod verification_tests;
mod viewed;

use crate::settings::SettingsStore;
pub(crate) use http::{Step, answered, in_order, media, run_cli, same_origin};
pub(crate) use rate_limit::RateLimits;
use std::{
    collections::{BTreeMap, HashMap},
    sync::{Arc, RwLock},
    time::{SystemTime, UNIX_EPOCH},
};
use tcode_core::{
    pull_request::{
        HostKind, HostTerms, PullRequestKey, PullRequestMergeMethod, PullRequestReviewDraftComment,
        PullRequestSnapshot, PullRequestStackState, StackRebaseStep,
    },
    pull_request_watch::{PullRequestRemark, PullRequestWatchRead},
    session::ReviewSide,
    settings::{HostSettings, HostStatus},
};
use tcode_protocol::{
    PullRequestAction, PullRequestActionResult as Outcome, PullRequestCapabilities,
    PullRequestRead, PullRequestReadResponse, PullRequestRejection as Rejection,
    PullRequestReviewVerdict, PullRequestStackHead,
};
pub(crate) use viewed::{ViewedMarks, revisions};

/// The hosts Tcode reads pull requests from, with credentials from `store` and the launch
/// environment.
pub fn connect(
    store: SettingsStore,
    environment: impl IntoIterator<Item = (String, String)>,
) -> Arc<dyn Forge> {
    let environment: Vec<_> = environment.into_iter().collect();
    let github = crate::github::GitHub::new(crate::github::GitHubApi::host(
        crate::github::Credentials::new(store.clone(), environment.clone()),
    ));
    // One file, so a host's marks never overwrite another's.
    let viewed = Arc::new(ViewedMarks::new(store.data_file("viewed-marks.json")));
    let forgejo = crate::forgejo::Forgejo::new(store.clone(), environment.clone(), viewed.clone());
    let gitlab = crate::gitlab::GitLab::new(store.clone(), environment.clone(), viewed.clone());
    let bitbucket =
        crate::bitbucket::Bitbucket::new(store.clone(), environment.clone(), viewed.clone());
    let azure = crate::azure::Azure::new(store, environment, viewed);
    Hosts::new(github, forgejo, gitlab, bitbucket, azure)
}

/// A pull request host. Each entry answers for the host the key names.
pub trait Forge: Send + Sync {
    /// What Tcode's text says about the host of the pull request.
    fn terms(&self, key: &PullRequestKey) -> &'static HostTerms;
    /// What the host offers on the pull request at all, whoever reads it.
    fn capabilities(&self, key: &PullRequestKey) -> PullRequestCapabilities;

    /// Every host Tcode reads, with each kind's own read of the ones it serves.
    fn configure(&self, hosts: BTreeMap<String, HostSettings>);
    /// Where each host's credential comes from, read now: configured hosts, those a CLI login
    /// or the environment names, and the public host.
    fn credential_status(&self) -> BTreeMap<String, HostStatus>;
    /// Drops what is held of the host's credential, so the next request resolves it again.
    fn forget_credential(&self, host: &str);

    /// The key and canonical URL of a pull request's web URL, or `None` for any other URL.
    fn pull_request_url(&self, url: &str) -> Option<(PullRequestKey, String)>;
    /// The repository a checkout's remote names on the host.
    fn checkout_repository(&self, checkout: &Checkout<'_>) -> Option<Repository>;
    /// A repository named as the host writes it, on `host`.
    fn repository(&self, name: &str, host: &str) -> Option<Repository>;
    /// The web URL of a pull request.
    fn url(&self, key: &PullRequestKey) -> Option<String>;
    fn summary(&self, key: &PullRequestKey) -> Result<Summary, ForgeError>;
    fn stack(&self, key: &PullRequestKey) -> Result<PullRequestStackState, ForgeError>;

    /// One read of the pull request and when it should be read again.
    fn read(
        &self,
        key: &PullRequestKey,
        read: PullRequestRead,
    ) -> Result<(PullRequestReadResponse, SystemTime), ForgeError>;
    /// Drops every read held of the pull request.
    fn invalidate(&self, key: &PullRequestKey);
    /// Opaque, and different for each account the host reads as.
    fn account(&self, key: &PullRequestKey) -> Result<String, ForgeError>;
    fn set_viewed(
        &self,
        key: &PullRequestKey,
        paths: &[String],
        viewed: bool,
    ) -> Result<(), ForgeError>;

    fn act(&self, key: &PullRequestKey, action: &PullRequestAction) -> Outcome;
    /// Sends a review in one submission, refused when the head is no longer `head`.
    fn submit_review(
        &self,
        key: &PullRequestKey,
        verdict: PullRequestReviewVerdict,
        head: &str,
        body: &str,
        comments: &[PullRequestReviewDraftComment],
    ) -> Outcome;
    /// Whether the host would take a review comment on those lines at `head`.
    fn commentable(
        &self,
        key: &PullRequestKey,
        head: &str,
        path: &str,
        side: ReviewSide,
        lines: (u32, u32),
    ) -> Result<Anchoring, ForgeError>;
    /// The current head, and the revision each pending comment's lines read at there.
    fn reanchor(
        &self,
        key: &PullRequestKey,
        comments: &[PullRequestReviewDraftComment],
    ) -> Result<(String, Vec<Moved>), ForgeError>;

    /// Submits the host's asynchronous merge of native stack `stack` up to this layer. A host
    /// without native stacks has none to merge.
    fn merge_stack(
        &self,
        _key: &PullRequestKey,
        _stack: u64,
        _heads: &[PullRequestStackHead],
        _method: PullRequestMergeMethod,
    ) -> MergeSubmission {
        MergeSubmission::Done(Outcome::Rejected(Rejection::Invalid))
    }
    /// What became of stack merge `id`: `None` while it is still pending.
    fn merge_status(
        &self,
        _key: &PullRequestKey,
        _id: &str,
    ) -> Result<Option<Outcome>, ForgeError> {
        Err(ForgeError {
            kind: ForgeErrorKind::NotFound,
            description: "no native stacks on this host".into(),
        })
    }
    fn plan_stack_rebase(
        &self,
        _key: &PullRequestKey,
        _stack: u64,
        _heads: &[PullRequestStackHead],
    ) -> Result<StackRebase, Rejection> {
        Err(Rejection::Invalid)
    }

    /// One fingerprint per key, in order. `Ok(None)` is a pull request the host gave none for,
    /// as every one is on a host without fingerprints; it takes the reads gated by its sync
    /// snapshot.
    fn fingerprints(
        &self,
        keys: &[PullRequestKey],
    ) -> Vec<Result<Option<Fingerprint>, ForgeError>> {
        keys.iter().map(|_| Ok(None)).collect()
    }
    /// What a watch evaluates of the pull request now.
    fn watch_detail(&self, key: &PullRequestKey) -> Result<PullRequestWatchRead, ForgeError>;
    /// Every remark on the pull request, or `Ok(None)` when one could be missing. `tails` holds
    /// review-thread replies past a thread's first page between reads.
    fn activity(
        &self,
        key: &PullRequestKey,
        tails: &mut Tails,
    ) -> Result<Option<Vec<PullRequestRemark>>, ForgeError>;
}

/// Why a host could not answer, in Tcode's terms.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForgeError {
    pub kind: ForgeErrorKind,
    /// The host's own description, which carries no request values.
    pub description: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ForgeErrorKind {
    /// Turned off for this host in settings.
    HostDisabled,
    /// No usable credential for the host: none saved or in the environment, and the host's CLI
    /// that would supply one is missing or signed out.
    NoCredential,
    /// The host refused the credential.
    Unauthorized,
    /// Refused before any request while an earlier rate limit lasts.
    Paused {
        retry_at: SystemTime,
    },
    RateLimited {
        retry_at: SystemTime,
    },
    NotFound,
    /// The host refused, in its own words.
    Refused {
        messages: Vec<String>,
    },
    /// No answer arrived, or none that could be read: a write may have been applied.
    Uncertain,
    Deadline,
    TooLarge,
    /// A read Tcode refuses before any request: an invalid revision, path or media URL.
    InvalidInput,
    /// Media whose type is not an image, video or audio, or an image that does not decode.
    UnsupportedMedia,
}

impl std::fmt::Display for ForgeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.description)
    }
}
impl std::error::Error for ForgeError {}

impl ForgeError {
    /// When a rate limit or the pause after one ends.
    pub fn retry_at(&self) -> Option<SystemTime> {
        match self.kind {
            ForgeErrorKind::Paused { retry_at } | ForgeErrorKind::RateLimited { retry_at } => {
                Some(retry_at)
            }
            _ => None,
        }
    }

    /// Why a write was not sent, or why the host refused it.
    pub fn rejection(self) -> Rejection {
        match self.kind {
            ForgeErrorKind::HostDisabled => Rejection::HostDisabled,
            ForgeErrorKind::NoCredential | ForgeErrorKind::Unauthorized => Rejection::NoCredential,
            ForgeErrorKind::Paused { retry_at } | ForgeErrorKind::RateLimited { retry_at } => {
                Rejection::RateLimited {
                    retry_at: retry_at
                        .duration_since(UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs(),
                }
            }
            ForgeErrorKind::NotFound => Rejection::NotFound,
            ForgeErrorKind::Refused { messages } => Rejection::Refused { messages },
            ForgeErrorKind::InvalidInput => Rejection::Invalid,
            _ => Rejection::Failed,
        }
    }
}

/// A repository on a host, as pull request keys locate it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Repository {
    pub host: String,
    pub locator: String,
}
impl Repository {
    pub fn key(&self, number: u64) -> PullRequestKey {
        PullRequestKey::new(&self.host, &self.locator, number)
    }
}

#[derive(Debug, Clone)]
pub struct Summary {
    pub snapshot: PullRequestSnapshot,
    /// Absent on hosts where native membership cannot be read.
    pub stack_number: Option<Option<u64>>,
}

/// A pending comment's id and the revision its lines now read at, if they still do.
pub type Moved = (u64, Option<String>);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Anchoring {
    InDiff,
    OutsideDiff,
    /// The pull request is at another head now.
    Moved,
}

/// What became of a stack merge's submission.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MergeSubmission {
    /// Answered at once, or refused before anything was sent.
    Done(Outcome),
    /// The host works on operation `id`; `adopted` when it was already running.
    Following {
        id: String,
        adopted: bool,
        layers: Vec<u64>,
    },
}

/// A rebase of a native stack the host may start: its unmerged layers, bottom first, by number
/// and branch, and the run that rebases and pushes them.
pub struct StackRebase {
    pub layers: Vec<(u64, String)>,
    run: Run,
}
/// Rebases and pushes the layers, telling its argument each layer's step as it happens.
type Run = Box<dyn FnOnce(&mut dyn FnMut(usize, StackRebaseStep)) -> Outcome + Send>;
impl StackRebase {
    pub fn new(
        layers: Vec<(u64, String)>,
        run: impl FnOnce(&mut dyn FnMut(usize, StackRebaseStep)) -> Outcome + Send + 'static,
    ) -> Self {
        Self {
            layers,
            run: Box::new(run),
        }
    }

    /// Runs the rebase, telling `progress` each layer's step, and answers how it ended.
    pub fn run(self, mut progress: impl FnMut(usize, StackRebaseStep)) -> Outcome {
        (self.run)(&mut progress)
    }
}

/// Two parts, so a watch reads only what moved: `status` needs the detail read, `remarks` the
/// far costlier activity read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fingerprint {
    pub status: String,
    pub remarks: String,
}

/// Replies past one review thread's first page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tail {
    pub(crate) count: u64,
    pub(crate) comments: Vec<PullRequestRemark>,
}
pub type Tails = HashMap<String, Tail>;

/// Whether the host offers what `action` asks for. One the host lacks is refused before any
/// request, so a stale client never reaches the host with it.
fn offered(capabilities: &PullRequestCapabilities, action: &PullRequestAction) -> bool {
    match action {
        PullRequestAction::ReplyToThread { .. } => capabilities.reply,
        PullRequestAction::ResolveThread { .. } => capabilities.resolve,
        PullRequestAction::React { .. } => capabilities.reactions,
        PullRequestAction::SetLabels { .. } => capabilities.labels,
        PullRequestAction::ReadyForReview | PullRequestAction::ConvertToDraft => capabilities.draft,
        PullRequestAction::Reopen => capabilities.reopen,
        PullRequestAction::Revert => capabilities.revert,
        PullRequestAction::UpdateBranch { rebase, .. } => {
            capabilities.update_branch && (*rebase || capabilities.update_merge)
        }
        PullRequestAction::DisableAutoMerge | PullRequestAction::Merge { auto: true, .. } => {
            capabilities.auto_merge
        }
        _ => true,
    }
}

/// Routes each key and host to the implementation of its kind: the kind settings give it, else
/// the one its name says, else GitHub, as every host was before hosts had kinds.
struct Hosts {
    github: Arc<dyn Forge>,
    forgejo: Arc<dyn Forge>,
    gitlab: Arc<dyn Forge>,
    bitbucket: Arc<dyn Forge>,
    azure: Arc<dyn Forge>,
    kinds: RwLock<BTreeMap<String, HostKind>>,
}

impl Hosts {
    fn new(
        github: Arc<dyn Forge>,
        forgejo: Arc<dyn Forge>,
        gitlab: Arc<dyn Forge>,
        bitbucket: Arc<dyn Forge>,
        azure: Arc<dyn Forge>,
    ) -> Arc<Self> {
        Arc::new(Self {
            github,
            forgejo,
            gitlab,
            bitbucket,
            azure,
            kinds: RwLock::default(),
        })
    }

    /// Every kind's implementation, the one a host falls back to first.
    fn all(&self) -> [&dyn Forge; 5] {
        [
            self.github.as_ref(),
            self.forgejo.as_ref(),
            self.gitlab.as_ref(),
            self.bitbucket.as_ref(),
            self.azure.as_ref(),
        ]
    }

    fn kind(&self, host: &str) -> HostKind {
        HostKind::of(self.kinds.read().unwrap().get(host).copied(), host)
    }

    fn of(&self, kind: HostKind) -> &dyn Forge {
        match kind {
            HostKind::Github => self.github.as_ref(),
            HostKind::Forgejo | HostKind::Gitea => self.forgejo.as_ref(),
            HostKind::Gitlab => self.gitlab.as_ref(),
            HostKind::Bitbucket => self.bitbucket.as_ref(),
            HostKind::Azure => self.azure.as_ref(),
        }
    }

    fn host(&self, host: &str) -> &dyn Forge {
        self.of(self.kind(host))
    }

    /// Whether `host`'s kind is served by `forge`.
    fn serves(&self, forge: &dyn Forge, host: &str) -> bool {
        std::ptr::addr_eq(self.host(host), forge)
    }
}

impl Forge for Hosts {
    fn terms(&self, key: &PullRequestKey) -> &'static HostTerms {
        self.host(&key.host).terms(key)
    }

    fn capabilities(&self, key: &PullRequestKey) -> PullRequestCapabilities {
        self.host(&key.host).capabilities(key)
    }

    fn configure(&self, hosts: BTreeMap<String, HostSettings>) {
        {
            let mut kinds = self.kinds.write().unwrap();
            kinds.retain(|host, _| !hosts.contains_key(host));
            kinds.extend(
                hosts
                    .iter()
                    .map(|(host, choice)| (host.clone(), choice.kind)),
            );
        }
        self.github.configure(hosts.clone());
        self.forgejo.configure(hosts.clone());
        self.gitlab.configure(hosts.clone());
        self.bitbucket.configure(hosts.clone());
        self.azure.configure(hosts);
    }

    fn credential_status(&self) -> BTreeMap<String, HostStatus> {
        let mut status = self.forgejo.credential_status();
        // A host two kinds list, such as one a gh login and a glab login both name, stays the
        // kind settings say.
        for (host, gitlab) in self.gitlab.credential_status() {
            if !status.contains_key(&host) || self.kind(&host) == HostKind::Gitlab {
                status.insert(host, gitlab);
            }
        }
        for (host, bitbucket) in self.bitbucket.credential_status() {
            if !status.contains_key(&host) || self.kind(&host) == HostKind::Bitbucket {
                status.insert(host, bitbucket);
            }
        }
        for (host, azure) in self.azure.credential_status() {
            if !status.contains_key(&host) || self.kind(&host) == HostKind::Azure {
                status.insert(host, azure);
            }
        }
        for (host, github) in self.github.credential_status() {
            if self.kind(&host) == HostKind::Github {
                status.insert(host, github);
            }
        }
        let mut kinds = self.kinds.write().unwrap();
        for (host, found) in &status {
            kinds.entry(host.clone()).or_insert(found.kind);
        }
        status
    }

    fn forget_credential(&self, host: &str) {
        self.host(host).forget_credential(host);
    }

    fn pull_request_url(&self, url: &str) -> Option<(PullRequestKey, String)> {
        self.all().into_iter().find_map(|forge| {
            forge
                .pull_request_url(url)
                .filter(|(key, _)| self.serves(forge, &key.host))
        })
    }

    fn checkout_repository(&self, checkout: &Checkout<'_>) -> Option<Repository> {
        self.all().into_iter().find_map(|forge| {
            forge
                .checkout_repository(checkout)
                .filter(|repository| self.serves(forge, &repository.host))
        })
    }

    fn repository(&self, name: &str, host: &str) -> Option<Repository> {
        self.host(host.trim()).repository(name, host)
    }

    fn url(&self, key: &PullRequestKey) -> Option<String> {
        self.host(&key.host).url(key)
    }

    fn summary(&self, key: &PullRequestKey) -> Result<Summary, ForgeError> {
        self.host(&key.host).summary(key)
    }

    fn stack(&self, key: &PullRequestKey) -> Result<PullRequestStackState, ForgeError> {
        self.host(&key.host).stack(key)
    }

    fn read(
        &self,
        key: &PullRequestKey,
        read: PullRequestRead,
    ) -> Result<(PullRequestReadResponse, SystemTime), ForgeError> {
        self.host(&key.host).read(key, read)
    }

    fn invalidate(&self, key: &PullRequestKey) {
        self.host(&key.host).invalidate(key);
    }

    fn account(&self, key: &PullRequestKey) -> Result<String, ForgeError> {
        self.host(&key.host).account(key)
    }

    fn set_viewed(
        &self,
        key: &PullRequestKey,
        paths: &[String],
        viewed: bool,
    ) -> Result<(), ForgeError> {
        self.host(&key.host).set_viewed(key, paths, viewed)
    }

    fn act(&self, key: &PullRequestKey, action: &PullRequestAction) -> Outcome {
        let host = self.host(&key.host);
        if !offered(&host.capabilities(key), action) {
            return Outcome::Rejected(Rejection::Unsupported);
        }
        host.act(key, action)
    }

    fn submit_review(
        &self,
        key: &PullRequestKey,
        verdict: PullRequestReviewVerdict,
        head: &str,
        body: &str,
        comments: &[PullRequestReviewDraftComment],
    ) -> Outcome {
        let host = self.host(&key.host);
        if verdict == PullRequestReviewVerdict::RequestChanges
            && !host.capabilities(key).request_changes
        {
            return Outcome::Rejected(Rejection::Unsupported);
        }
        host.submit_review(key, verdict, head, body, comments)
    }

    fn commentable(
        &self,
        key: &PullRequestKey,
        head: &str,
        path: &str,
        side: ReviewSide,
        lines: (u32, u32),
    ) -> Result<Anchoring, ForgeError> {
        self.host(&key.host)
            .commentable(key, head, path, side, lines)
    }

    fn reanchor(
        &self,
        key: &PullRequestKey,
        comments: &[PullRequestReviewDraftComment],
    ) -> Result<(String, Vec<Moved>), ForgeError> {
        self.host(&key.host).reanchor(key, comments)
    }

    fn merge_stack(
        &self,
        key: &PullRequestKey,
        stack: u64,
        heads: &[PullRequestStackHead],
        method: PullRequestMergeMethod,
    ) -> MergeSubmission {
        self.host(&key.host).merge_stack(key, stack, heads, method)
    }

    fn merge_status(&self, key: &PullRequestKey, id: &str) -> Result<Option<Outcome>, ForgeError> {
        self.host(&key.host).merge_status(key, id)
    }

    fn plan_stack_rebase(
        &self,
        key: &PullRequestKey,
        stack: u64,
        heads: &[PullRequestStackHead],
    ) -> Result<StackRebase, Rejection> {
        self.host(&key.host).plan_stack_rebase(key, stack, heads)
    }

    fn fingerprints(
        &self,
        keys: &[PullRequestKey],
    ) -> Vec<Result<Option<Fingerprint>, ForgeError>> {
        let mut answers: Vec<_> = keys.iter().map(|_| Ok(None)).collect();
        for forge in self.all() {
            let (indices, own): (Vec<_>, Vec<_>) = keys
                .iter()
                .enumerate()
                .filter(|(_, key)| self.serves(forge, &key.host))
                .map(|(index, key)| (index, key.clone()))
                .unzip();
            if own.is_empty() {
                continue;
            }
            for (index, answer) in indices.into_iter().zip(forge.fingerprints(&own)) {
                answers[index] = answer;
            }
        }
        answers
    }

    fn watch_detail(&self, key: &PullRequestKey) -> Result<PullRequestWatchRead, ForgeError> {
        self.host(&key.host).watch_detail(key)
    }

    fn activity(
        &self,
        key: &PullRequestKey,
        tails: &mut Tails,
    ) -> Result<Option<Vec<PullRequestRemark>>, ForgeError> {
        self.host(&key.host).activity(key, tails)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checkout_routing_follows_git_init_and_remote_changes() {
        let root = std::env::temp_dir().join(format!("tcode-checkout-{}", uuid::Uuid::new_v4()));
        let cwd = root.join("checkout");
        std::fs::create_dir_all(&cwd).unwrap();
        let forge = connect(SettingsStore::new(root.clone()), []);
        let git = |args: &[&str]| crate::git::run_git(&cwd, args).unwrap();
        let repository = || forge.checkout_repository(&Checkout::new(&cwd));
        assert!(repository().is_none());
        git(&["init", "-b", "main"]);
        assert!(repository().is_none());
        git(&["remote", "add", "origin", "https://github.com/team/app.git"]);
        for (url, host, name) in [
            ("https://github.com/team/app.git", "github.com", "team/app"),
            (
                "https://codeberg.org/team/app.git",
                "codeberg.org",
                "team/app",
            ),
            ("https://gitlab.com/team/app.git", "gitlab.com", "team/app"),
            (
                "https://bitbucket.org/team/app.git",
                "bitbucket.org",
                "team/app",
            ),
            (
                "https://dev.azure.com/team/project/_git/app",
                "dev.azure.com/team",
                "project/app",
            ),
        ] {
            git(&["remote", "set-url", "origin", url]);
            assert_eq!(
                repository().map(|repository| repository.key(1)),
                Some(PullRequestKey::new(host, name, 1)),
                "{url}"
            );
        }
        git(&["remote", "remove", "origin"]);
        assert!(repository().is_none());
        std::fs::remove_dir_all(root).unwrap();
    }

    /// A host that only rebases refuses a merge-style branch update before any request; a
    /// rebase passes the gate and reaches the host, here one turned off in settings.
    #[test]
    fn a_merge_update_is_refused_where_the_host_only_rebases() {
        let root = std::env::temp_dir().join(format!("tcode-hosts-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let forge = connect(SettingsStore::new(root.clone()), []);
        forge.configure(BTreeMap::from([(
            "gitlab.acme.test".to_owned(),
            HostSettings {
                enabled: false,
                ..HostSettings::new(HostKind::Gitlab)
            },
        )]));
        let key = PullRequestKey::new("gitlab.acme.test", "team/app", 1);
        let update = |rebase| {
            forge.act(
                &key,
                &PullRequestAction::UpdateBranch {
                    head: "abc".into(),
                    rebase,
                },
            )
        };
        assert_eq!(update(false), Outcome::Rejected(Rejection::Unsupported));
        assert_eq!(update(true), Outcome::Rejected(Rejection::HostDisabled));
        let _ = std::fs::remove_dir_all(root);
    }

    /// Bitbucket closes by declining, which it never undoes, and has no labels: a reopen or a
    /// label edit is refused before any request, as a capability the host lacks rather than a
    /// permission.
    #[test]
    fn a_bitbucket_reopen_or_label_edit_is_refused() {
        let root = std::env::temp_dir().join(format!("tcode-hosts-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let forge = connect(SettingsStore::new(root.clone()), []);
        let key = PullRequestKey::new("bitbucket.org", "team/web", 8);
        let capabilities = forge.capabilities(&key);
        assert!(!capabilities.reopen && !capabilities.labels);
        for action in [
            PullRequestAction::Reopen,
            PullRequestAction::SetLabels {
                add: vec!["bug".into()],
                remove: Vec::new(),
            },
        ] {
            assert_eq!(
                forge.act(&key, &action),
                Outcome::Rejected(Rejection::Unsupported),
                "{action:?}"
            );
        }
        let _ = std::fs::remove_dir_all(root);
    }

    /// A host is served by the kind settings give it; otherwise its name decides, and a host
    /// whose name says nothing is GitHub's, as every host was before hosts had kinds.
    #[test]
    fn each_host_goes_to_its_kinds_implementation() {
        let root = std::env::temp_dir().join(format!("tcode-hosts-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let forge = connect(SettingsStore::new(root.clone()), []);
        forge.configure(BTreeMap::from([
            (
                "gitea.acme.test".to_owned(),
                HostSettings::new(HostKind::Github),
            ),
            (
                "gitlab.acme.test".to_owned(),
                HostSettings::new(HostKind::Forgejo),
            ),
            (
                "code.acme.test:8443".to_owned(),
                HostSettings::new(HostKind::Gitlab),
            ),
        ]));
        let name = |host: &str| forge.terms(&PullRequestKey::new(host, "a/b", 1)).name;
        assert_eq!(name("gitea.acme.test"), "GitHub");
        assert_eq!(name("codeberg.org"), "Forgejo");
        assert_eq!(name("gitea.com"), "Gitea");
        assert_eq!(name("git.example.com"), "GitHub");
        assert_eq!(name("gitlab.com"), "GitLab");
        assert_eq!(name("gitlab.example.com"), "GitLab");
        assert_eq!(name("gitlab.acme.test"), "Forgejo");
        assert_eq!(name("code.acme.test:8443"), "GitLab");
        assert_eq!(name("bitbucket.org"), "Bitbucket");
        // Bitbucket Data Center is not read as Bitbucket.
        assert_eq!(name("bitbucket.acme.test"), "GitHub");
        // Each Azure DevOps organization is a host of its own, and an older visualstudio.com
        // URL reads as the same one.
        assert_eq!(name("dev.azure.com/fabrikam"), "Azure DevOps");
        for url in [
            "https://dev.azure.com/Fabrikam/Web/_git/Web/pullrequest/7",
            "https://fabrikam.visualstudio.com/Web/_git/Web/pullrequest/7",
        ] {
            assert_eq!(
                forge.pull_request_url(url).map(|(key, _)| key),
                Some(PullRequestKey::new("dev.azure.com/fabrikam", "web/web", 7)),
                "{url}"
            );
        }
        assert_eq!(
            forge
                .pull_request_url("https://bitbucket.org/team/web/pull-requests/8/diff")
                .map(|(key, _)| key),
            Some(PullRequestKey::new("bitbucket.org", "team/web", 8))
        );
        assert_eq!(
            forge
                .pull_request_url("https://code.acme.test:8443/team/apps/web/-/merge_requests/5")
                .map(|(key, _)| key),
            Some(PullRequestKey::new(
                "code.acme.test:8443",
                "team/apps/web",
                5
            ))
        );
        assert_eq!(
            forge.pull_request_url("https://gitlab.acme.test/team/web/-/merge_requests/5"),
            None
        );
        assert_eq!(
            forge
                .pull_request_url("https://codeberg.org/a/b/pulls/2")
                .map(|(key, _)| key),
            Some(PullRequestKey::new("codeberg.org", "a/b", 2))
        );
        assert_eq!(
            forge
                .pull_request_url("https://git.example.com/a/b/pull/3")
                .map(|(key, _)| key),
            Some(PullRequestKey::new("git.example.com", "a/b", 3))
        );
        assert_eq!(
            forge.pull_request_url("https://git.example.com/a/b/pulls/3"),
            None
        );
        let _ = std::fs::remove_dir_all(root);
    }
}
