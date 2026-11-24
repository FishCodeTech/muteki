//! The read side of a linked pull request, as the pull request host answers it.

use agent::FileChangeKind;
use serde::{Deserialize, Serialize};
use tcode_core::{
    pull_request::{PullRequestMergeMethod, PullRequestState, StackRebaseFailure},
    session::ReviewSide,
};

/// The largest image a media read returns; past it the host stops reading.
pub const MAX_PULL_REQUEST_MEDIA_BYTES: usize = 8 * 1024 * 1024;

/// One read of a pull request linked to the named thread.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "content", rename_all = "snake_case")]
pub enum PullRequestRead {
    /// `None` asks for the whole diff; a cursor is sent only after a reply named it.
    Files {
        cursor: Option<String>,
    },
    /// A file's text at an immutable revision, read when its diff is expanded.
    FileText {
        revision: String,
        path: String,
    },
    Conversation,
    /// A review thread's replies after the page the conversation carried.
    ThreadReplies {
        thread_id: String,
        after: String,
    },
    ViewedFiles,
    /// An image URL from the pull request's conversation or an author's avatar; the host decides
    /// whether it reads it. `validator` is the one a previous answer carried, to revalidate a
    /// cached copy.
    Media {
        url: String,
        validator: Option<String>,
    },
    /// The repository's labels, with the ones on the pull request marked.
    LabelCandidates,
    /// Whoever the pull request may be sent to for review, with current requests marked. The
    /// organization's teams are not listed: that needs `read:org`, which a token need not carry.
    ReviewerCandidates,
    /// What merging, updating the branch and the other lifecycle actions would meet now.
    ActionState,
    /// The native stack the pull request is a layer of, as GitHub has it now, for a merge or,
    /// with `rebase`, a rebase of the stack.
    StackState {
        rebase: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PullRequestFiles {
    /// The commits the changed files are read between.
    pub base: String,
    pub head: String,
    pub files: Vec<PullRequestFile>,
    /// The host refused or cut the whole diff and pages its changed files instead: where the
    /// next page starts, opaque to the client.
    pub next_cursor: Option<String>,
    /// Every changed file has been listed once the pages before this one were read too.
    pub complete: bool,
    /// The host's count of changed files, which a listing may fall short of.
    pub changed_files: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PullRequestFile {
    /// Repository-relative.
    pub path: String,
    pub previous_path: Option<String>,
    pub kind: FileChangeKind,
    pub additions: u64,
    pub deletions: u64,
    pub patch: PullRequestPatch,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "content", rename_all = "snake_case")]
pub enum PullRequestPatch {
    /// Unified hunks from the first `@@`; empty for a rename or mode change alone.
    Hunks(String),
    Binary,
    /// GitHub withheld the hunks of a file too large to show.
    Oversized,
    /// GitHub listed the file without hunks or counts: binary, or past its own diff limits,
    /// which the files listing does not tell apart.
    Withheld,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "content", rename_all = "snake_case")]
pub enum PullRequestFileText {
    Text(String),
    Binary,
    Oversized,
    /// The file does not exist at that revision.
    Missing,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PullRequestActor {
    pub login: String,
    pub avatar_url: Option<String>,
}

/// The reactions GitHub offers, in the order its picker shows them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PullRequestReactionContent {
    ThumbsUp,
    ThumbsDown,
    Laugh,
    Hooray,
    Confused,
    Heart,
    Rocket,
    Eyes,
}
impl PullRequestReactionContent {
    pub const ALL: [Self; 8] = [
        Self::ThumbsUp,
        Self::ThumbsDown,
        Self::Laugh,
        Self::Hooray,
        Self::Confused,
        Self::Heart,
        Self::Rocket,
        Self::Eyes,
    ];
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PullRequestReaction {
    pub content: PullRequestReactionContent,
    pub count: u64,
    pub viewer_reacted: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PullRequestReviewState {
    Approved,
    ChangesRequested,
    Commented,
    Dismissed,
    Pending,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PullRequestComment {
    pub id: String,
    /// Absent for a deleted account.
    pub author: Option<PullRequestActor>,
    pub body: String,
    pub created_at: String,
    pub edited_at: Option<String>,
    pub url: Option<String>,
    /// Set for a review's own body.
    pub review_state: Option<PullRequestReviewState>,
    pub reactions: Vec<PullRequestReaction>,
    /// GitHub lets the signed-in account change its text: its own, or a maintainer's right.
    #[serde(default)]
    pub viewer_can_update: bool,
    #[serde(default)]
    pub viewer_can_react: bool,
}

/// Where a review thread was left: lines of one side of a file at one commit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PullRequestReviewAnchor {
    pub revision: String,
    pub path: String,
    pub side: ReviewSide,
    pub start_line: u32,
    pub end_line: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PullRequestReviewThread {
    pub id: String,
    pub path: String,
    pub resolved: bool,
    /// The head moved past the lines; `anchor` then names the lines as first commented on.
    pub outdated: bool,
    /// Absent for a comment on a whole file.
    pub anchor: Option<PullRequestReviewAnchor>,
    /// The last lines of the hunk the thread was left on, without its `@@` header.
    pub diff_hunk: Option<String>,
    pub comments: Vec<PullRequestComment>,
    pub total_comments: u64,
    /// Where [`PullRequestRead::ThreadReplies`] carries on, while replies remain unread.
    pub replies_after: Option<String>,
    #[serde(default)]
    pub viewer_can_reply: bool,
    /// Whether the signed-in account may resolve it, or unresolve it once resolved.
    #[serde(default)]
    pub viewer_can_resolve: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PullRequestConversation {
    pub description: PullRequestComment,
    /// Issue comments and review bodies, oldest first.
    pub comments: Vec<PullRequestComment>,
    pub threads: Vec<PullRequestReviewThread>,
    /// False when a list ran past the pages the host reads.
    pub complete: bool,
    /// Opaque, and different for each GitHub account the host reads as: a client keys the
    /// media this conversation shows by it, so no copy outlives an account change.
    pub account: String,
    #[serde(default)]
    pub permissions: PullRequestPermissions,
    #[serde(default)]
    pub labels: Vec<PullRequestLabel>,
    /// Requested reviewers first, then whoever reviewed without a request outstanding.
    #[serde(default)]
    pub reviewers: Vec<PullRequestReviewerState>,
    pub capabilities: PullRequestCapabilities,
}

/// What the host offers at all, whoever reads it: an action it lacks is not offered, rather
/// than shown as one the account may not take. What the account may do travels apart, in
/// [`PullRequestPermissions`] and on each comment and thread.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PullRequestCapabilities {
    /// Replies to review threads.
    pub reply: bool,
    /// Resolving and unresolving review threads.
    pub resolve: bool,
    pub reactions: bool,
    /// Labels on pull requests.
    pub labels: bool,
    /// The request-changes review verdict.
    pub request_changes: bool,
    /// Marking a pull request ready for review and converting it to a draft.
    pub draft: bool,
    pub reopen: bool,
    pub auto_merge: bool,
    /// Bringing the base into the head on the host.
    pub update_branch: bool,
    /// Bringing the base in by a merge commit; without it an update is a rebase.
    pub update_merge: bool,
    /// Opening a pull request that reverses a merged one.
    pub revert: bool,
    /// The host keeps the account's viewed marks; without them Tcode keeps the marks.
    pub host_viewed_marks: bool,
    /// A merge takes the commit message Tcode sends, such as one without agents' credits.
    pub merge_message: bool,
}
impl PullRequestCapabilities {
    pub const ALL: Self = Self {
        reply: true,
        resolve: true,
        reactions: true,
        labels: true,
        request_changes: true,
        draft: true,
        reopen: true,
        auto_merge: true,
        update_branch: true,
        update_merge: true,
        revert: true,
        host_viewed_marks: true,
        merge_message: true,
    };
}

/// What the signed-in account may do to the pull request, as the host grants it. Per-comment and
/// per-thread rights travel on the comment and thread.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PullRequestPermissions {
    /// Edit the title and description.
    pub update: bool,
    /// Empty when the account may not review; the author may only comment.
    pub verdicts: Vec<PullRequestReviewVerdict>,
    pub label: bool,
    pub request_reviewers: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PullRequestLabel {
    /// The host's id for the label, which a write names it by; opaque to the client.
    pub id: String,
    pub name: String,
    /// Hex without the `#`.
    pub color: Option<String>,
    pub description: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PullRequestReviewerState {
    pub reviewer: PullRequestReviewer,
    pub avatar_url: Option<String>,
    /// `None` while a review is requested and not yet given.
    pub verdict: Option<PullRequestReviewState>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PullRequestThreadReplies {
    pub comments: Vec<PullRequestComment>,
    pub after: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PullRequestViewedState {
    Viewed,
    Unviewed,
    /// Viewed before a later push changed the file.
    Dismissed,
}

/// The signed-in account's viewed marks. A path missing from an incomplete list is unknown.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PullRequestViewedFiles {
    pub files: Vec<(String, PullRequestViewedState)>,
    pub complete: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "content", rename_all = "snake_case")]
pub enum PullRequestMedia {
    Image {
        #[serde(with = "crate::wire::base64_bytes")]
        bytes: Vec<u8>,
        mime: String,
        validator: Option<String>,
        /// Unix seconds after which a cached copy is read again.
        expires_at: u64,
    },
    /// The copy behind the request's validator is still current.
    NotModified { expires_at: u64 },
    /// Video and audio open in the browser until a range-capable read exists.
    External { mime: String },
    /// Not media on GitHub's own hosts: the client draws it by its URL, as a browser would.
    Unsupported,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PullRequestLabelCandidate {
    /// The host's id for the label, which a write names it by; opaque to the client.
    pub id: String,
    pub name: String,
    /// Hex without the `#`.
    pub color: Option<String>,
    pub description: Option<String>,
    pub applied: bool,
}

/// Labels on the pull request lead, including any the repository no longer lists.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PullRequestLabelCandidates {
    pub labels: Vec<PullRequestLabelCandidate>,
    /// False when the repository has more labels than one page.
    pub complete: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PullRequestReviewerKind {
    User,
    /// Named by its slug.
    Team,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PullRequestReviewer {
    /// The host's id for the reviewer, which a write names them by; opaque to the client.
    pub id: String,
    /// As the host shows the reviewer.
    pub login: String,
    pub kind: PullRequestReviewerKind,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PullRequestReviewerCandidate {
    pub reviewer: PullRequestReviewer,
    pub name: Option<String>,
    pub avatar_url: Option<String>,
    pub requested: bool,
}

/// Current requests lead, even for someone GitHub no longer counts assignable; the author is
/// left out, since GitHub refuses to ask them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PullRequestReviewerCandidates {
    pub reviewers: Vec<PullRequestReviewerCandidate>,
    /// False when the repository has more assignable people than one page.
    pub complete: bool,
}

/// GitHub's merge state status: what merging now would meet.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PullRequestMergeState {
    Clean,
    /// Mergeable, with checks that are not passing.
    Unstable,
    /// Mergeable, with pre-receive hooks to pass.
    HasHooks,
    /// Requirements such as reviews or checks are not met.
    Blocked,
    /// The branch protection requires the head to be up to date with the base.
    Behind,
    /// Conflicts with the base.
    Dirty,
    Draft,
    Unknown,
}

/// What a lifecycle action would meet: read fresh before a merge or a branch update, and on
/// request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PullRequestActionState {
    /// The head a branch update or a merge must still find.
    pub head: String,
    pub merge_state: PullRequestMergeState,
    /// Commits on the base the head does not have; `None` when GitHub could not compare them.
    pub behind_by: Option<u64>,
    pub merge_queue: bool,
    /// The repository's enabled methods, in GitHub's order.
    pub merge_methods: Vec<PullRequestMergeMethod>,
    /// The repository allows auto-merge.
    pub auto_merge_allowed: bool,
    /// The method auto-merge is armed with.
    pub auto_merge: Option<PullRequestMergeMethod>,
    pub queued: bool,
    /// Where in the merge queue, when GitHub says.
    pub queue_position: Option<u32>,
    /// Failing checks of the head by name; a list cut short names only the first page's.
    pub failing_checks: Vec<String>,
    pub pending_checks: u32,
    /// Mark ready, convert to draft, close and reopen.
    pub can_update: bool,
    pub can_update_branch: bool,
    /// Merge, auto-merge and revert.
    pub can_merge: bool,
    /// The host's, as the conversation carries them, for the lifecycle actions read from here.
    pub capabilities: PullRequestCapabilities,
}

/// A native stack as GitHub has it now, read before a stack write is confirmed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PullRequestStackActionState {
    pub stack: u64,
    pub base: String,
    /// Bottom to top.
    pub layers: Vec<PullRequestStackLayerState>,
    /// The repository's enabled methods, in GitHub's order.
    pub merge_methods: Vec<PullRequestMergeMethod>,
    pub merge_queue: bool,
    pub can_merge: bool,
    /// Whether the host's Git has a name and an email to commit the rebased layers with; read
    /// for a rebase only.
    pub git_identity: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PullRequestStackLayerState {
    pub number: u64,
    pub title: String,
    pub head_branch: String,
    /// `None` when GitHub gave no head, which no stack write goes ahead without.
    pub head: Option<String>,
    pub state: PullRequestState,
    pub draft: bool,
    /// Read for a rebase only, for each unmerged layer.
    pub push: Option<PullRequestStackPushAccess>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PullRequestStackPushAccess {
    Write,
    /// A fork's branch that allows maintainers to push.
    MaintainerCanModify,
    Denied,
}

/// The head a stack write was confirmed at, for one layer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PullRequestStackHead {
    pub number: u64,
    pub head: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "content", rename_all = "snake_case")]
pub enum PullRequestReadResponse {
    Files(PullRequestFiles),
    FileText(PullRequestFileText),
    Conversation(Box<PullRequestConversation>),
    ThreadReplies(PullRequestThreadReplies),
    ViewedFiles(PullRequestViewedFiles),
    Media(PullRequestMedia),
    LabelCandidates(PullRequestLabelCandidates),
    ReviewerCandidates(PullRequestReviewerCandidates),
    ActionState(PullRequestActionState),
    StackState(PullRequestStackActionState),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PullRequestReviewVerdict {
    Comment,
    Approve,
    RequestChanges,
}

/// A write to a linked pull request. Each is sent once: a result that cannot say whether GitHub
/// applied it is reported as uncertain, never tried again.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "content", rename_all = "snake_case")]
pub enum PullRequestAction {
    Comment {
        body: String,
    },
    /// Sends the thread's review draft of the pull request in one submission. `head` is the
    /// commit the reviewer read; a pull request whose head has moved since keeps the draft.
    SubmitReview {
        verdict: PullRequestReviewVerdict,
        head: String,
    },
    ReplyToThread {
        thread_id: String,
        body: String,
    },
    ResolveThread {
        thread_id: String,
        resolved: bool,
    },
    /// `subject_id` names the pull request itself, a comment or a review.
    React {
        subject_id: String,
        content: PullRequestReactionContent,
        reacted: bool,
    },
    /// An issue comment or a review comment.
    EditComment {
        comment_id: String,
        body: String,
    },
    /// A field left `None` is not sent, so GitHub keeps its text.
    Edit {
        title: Option<String>,
        body: Option<String>,
    },
    /// Labels by their ids: the additions in one request, then one request per removal, in
    /// order, stopping at the first that fails.
    SetLabels {
        add: Vec<String>,
        remove: Vec<String>,
    },
    /// The additions in one request, then the removals in another.
    SetReviewers {
        add: Vec<PullRequestReviewer>,
        remove: Vec<PullRequestReviewer>,
    },
    ReadyForReview,
    ConvertToDraft,
    Close,
    Reopen,
    /// Opens a pull request that reverses this merged one, linked to the thread.
    Revert,
    /// Brings the base into the head on GitHub, by a merge commit unless `rebase`. `head` is the
    /// commit the user saw; a moved head sends nothing.
    UpdateBranch {
        head: String,
        rebase: bool,
    },
    /// Merges at `head` with `method`, or joins the merge queue when the repository has one.
    /// With `auto`, a pull request that cannot merge yet has auto-merge armed instead. With
    /// `remove_credits`, a merge or squash leaves agents' credit lines out of GitHub's message.
    Merge {
        head: String,
        method: PullRequestMergeMethod,
        auto: bool,
        remove_credits: bool,
    },
    DisableAutoMerge,
    /// GitHub's asynchronous merge of this layer of native stack `stack` and every unmerged
    /// layer below it, at the heads in `heads`, which must be exactly that scope's.
    MergeStack {
        stack: u64,
        heads: Vec<PullRequestStackHead>,
        method: PullRequestMergeMethod,
    },
    /// Rebases every unmerged layer of native stack `stack` onto the one below it on the host,
    /// pushing each with a lease on its head in `heads`, which must be exactly those layers'.
    RebaseStack {
        stack: u64,
        heads: Vec<PullRequestStackHead>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "content", rename_all = "snake_case")]
pub enum PullRequestActionResult {
    Applied,
    /// Nothing was written.
    Rejected(PullRequestRejection),
    /// A write of several requests stopped: `applied` went through, `failure` is what became of
    /// the first of `unapplied`, and the rest were not sent.
    Partial {
        applied: Vec<String>,
        unapplied: Vec<String>,
        failure: Box<PullRequestActionResult>,
    },
    /// The write was sent and no answer says whether GitHub applied it.
    Uncertain,
    /// The branch already had the base; nothing was sent.
    UpToDate,
    /// The merge queue took the pull request; it is not merged yet.
    Queued {
        position: Option<u32>,
    },
    /// GitHub merges the pull request with `method` once its requirements pass.
    AutoMergeEnabled {
        method: PullRequestMergeMethod,
    },
    /// A new pull request was opened, and linked to the thread.
    Opened {
        number: u64,
        url: String,
    },
    /// GitHub took a stack merge as operation `id` and works on it; the host follows it.
    /// `adopted` when GitHub named an operation already running instead of taking this one.
    Pending {
        id: String,
        adopted: bool,
    },
    /// Operation `id` was still pending when the host stopped following it; it was not
    /// submitted again.
    MergeUnconfirmed {
        id: String,
    },
    /// The host started rebasing and finishes it on its own: for a stack its progress is the
    /// stack's operation; one pull request's rebase is not followed.
    RebaseStarted,
    /// Every layer was rebased: `pushed` were force-pushed, `current` needed nothing.
    Rebased {
        pushed: Vec<u64>,
        current: Vec<u64>,
    },
    /// The rebase stopped at `failed`: `pushed` stay rebased on GitHub, `untouched` above it
    /// were not changed.
    RebaseStopped {
        pushed: Vec<u64>,
        failed: u64,
        reason: StackRebaseFailure,
        untouched: Vec<u64>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "content", rename_all = "snake_case")]
pub enum PullRequestRejection {
    /// The pull request's head is `head`, not the commit the review was written against.
    StaleHead {
        head: String,
    },
    /// The named comment, review or thread belongs to another pull request.
    ForeignSubject,
    /// A review with nothing in it, an empty comment, or a subject of the wrong kind.
    Invalid,
    NoCredential,
    HostDisabled,
    RateLimited {
        retry_at: u64,
    },
    NotFound,
    /// GitHub refused the write, in its own words.
    Refused {
        messages: Vec<String>,
    },
    /// A read the write needed failed, so it was not sent.
    Failed,
    /// The host offers no such action.
    Unsupported,
    /// Whether the pull request is in a native stack is not known yet.
    StackUnknown,
    /// The stack's number or its layers are not what the write was confirmed against.
    StackChanged,
    /// Layer `number`'s head is `actual`, not the `expected` one the write was confirmed at.
    LayerChanged {
        number: u64,
        expected: String,
        actual: String,
    },
    LayerNotOpen {
        number: u64,
        state: PullRequestState,
    },
    LayerDraft {
        number: u64,
    },
    /// The account may not push to these layers' branches.
    NoPushAccess {
        numbers: Vec<u64>,
    },
    /// A write to the stack is already running or unconfirmed on the host.
    OperationRunning,
    /// GitHub reports a merge of the stack already running without naming it.
    MergeRunning,
    /// The host's Git has no name or email to commit rebased layers with.
    NoGitIdentity,
    /// Stack writes go only from a pull request the thread links.
    NotLinked,
}
