//! One parameter struct per `tcode_control` tool, shared by the MCP server and the
//! runtime that answers it, plus the shapes the tools have in common.

use serde::{Deserialize, Serialize};
use tcode_core::control::{ControlCategory, ControlScope, ControlTier, SelfSettleWhen, WatchEvent};

/// `"<thread_id>#<position>"`: the thread and the position of one stored entry.
pub fn parse_anchor(anchor: &str) -> Option<(&str, u64)> {
    let (thread_id, position) = anchor.rsplit_once('#')?;
    if thread_id.is_empty() {
        return None;
    }
    Some((thread_id, position.parse().ok()?))
}

/// A precondition on the target; an item whose target no longer matches is skipped as
/// `changed_since_preview`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Expect {
    #[serde(default)]
    #[schemars(description = "The target's last_activity_at (unix ms) as previously read.")]
    pub last_activity_at: Option<u64>,
    #[serde(default)]
    #[schemars(description = "The highest entry position previously read in the target.")]
    pub max_position: Option<u64>,
    #[serde(default)]
    #[schemars(description = "The target's state as previously read, for example settled.")]
    pub state: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Applied,
    WouldApply,
    Skipped,
    Failed,
}

/// One item of a batch answer, in input order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct BatchResult {
    pub target: String,
    pub outcome: Outcome,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub side_effects: Option<Vec<String>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ThreadState {
    Active,
    Settled,
    Archived,
}

/// Kinds of stored items a search may be limited to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ItemKind {
    UserMessage,
    AssistantMessage,
    Reasoning,
    Command,
    FileChange,
    ToolCall,
    Subagent,
    ImageRead,
    WebSearch,
    Steer,
    Other,
}

const SCOPE: &str = "Limit the call to the caller's own project (project), named projects ({\"projects\": [ids]}) or the whole host (host). Omit for the grant's scope. A scope wider than the grant is refused.";

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ListProjectsParams {}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ListThreadsParams {
    #[serde(default)]
    #[schemars(description = SCOPE)]
    pub scope: Option<ControlScope>,
    #[serde(default)]
    #[schemars(description = "Only threads in these states; omit for active and settled.")]
    pub states: Option<Vec<ThreadState>>,
    #[serde(default)]
    #[schemars(description = "Only threads on (true) or off (false) the Follow-up list.")]
    pub follow_up: Option<bool>,
    #[serde(default)]
    #[schemars(description = "Only threads active at or after this unix time in ms.")]
    pub active_since: Option<u64>,
    #[serde(default)]
    #[schemars(description = "Only threads active before this unix time in ms.")]
    pub active_before: Option<u64>,
    #[serde(default)]
    #[schemars(description = "Case-insensitive substring the title must contain.")]
    pub title_query: Option<String>,
    #[serde(default)]
    #[schemars(description = "Include orchestrated child threads and subagent mirrors.")]
    pub include_children: Option<bool>,
    #[serde(default)]
    #[schemars(
        description = "Include each thread's cwd, worktree kind and live dirty state (slower)."
    )]
    pub include_worktree: Option<bool>,
    #[serde(default)]
    #[schemars(description = "Page size; the host caps it.")]
    pub limit: Option<u32>,
    #[serde(default)]
    #[schemars(description = "The cursor a previous page returned.")]
    pub cursor: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct GetThreadParams {
    #[schemars(description = "The thread id.")]
    pub thread_id: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SearchParams {
    #[schemars(
        description = "Substring to find; a paraphrase will not match, so rephrase and filter."
    )]
    pub query: String,
    #[serde(default)]
    #[schemars(description = SCOPE)]
    pub scope: Option<ControlScope>,
    #[serde(default)]
    #[schemars(description = "Only these threads.")]
    pub thread_ids: Option<Vec<String>>,
    #[serde(default)]
    #[schemars(
        description = "Only items of these kinds. subagent, image_read, web_search and other are indexed as one kind, so naming any of them matches all four; reasoning is not indexed."
    )]
    pub kinds: Option<Vec<ItemKind>>,
    #[serde(default)]
    #[schemars(description = "Page size; the host caps it.")]
    pub limit: Option<u32>,
    #[serde(default)]
    #[schemars(description = "The cursor a previous page returned.")]
    pub cursor: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct PositionRange {
    #[schemars(description = "First position, inclusive.")]
    pub from: u64,
    #[schemars(description = "Last position, exclusive.")]
    pub to: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum OutlineLevel {
    /// Per turn: time, the start of the user message, the first lines and headings of the
    /// assistant's reply, commits and pull requests, markers.
    Coarse,
    /// Coarse plus the tools, files, commands with exit codes and errors of each turn.
    Turns,
    /// Turns plus display segments inside long autonomous turns.
    Segments,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct OutlineParams {
    #[schemars(description = "The thread id.")]
    pub thread_id: String,
    #[serde(default)]
    #[schemars(
        description = "Detail; default turns. A very large thread answers coarser and newest first whatever is asked."
    )]
    pub level: Option<OutlineLevel>,
    #[serde(default)]
    #[schemars(description = "Only entries within this position range.")]
    pub range: Option<PositionRange>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ReadParams {
    #[serde(default)]
    #[schemars(description = "Anchors (\"<thread_id>#<position>\") of the entries to read.")]
    pub anchors: Option<Vec<String>>,
    #[serde(default)]
    #[schemars(description = "Instead of anchors: the first anchor of a contiguous range.")]
    pub from: Option<String>,
    #[serde(default)]
    #[schemars(description = "The last anchor of the range, inclusive; same thread as from.")]
    pub to: Option<String>,
    #[serde(default)]
    #[schemars(
        description = "Instead of anchors or a range: the cursor a truncated entry returned, to continue its text."
    )]
    pub cursor: Option<String>,
    #[serde(default)]
    #[schemars(description = "Response budget in bytes; default 32 KiB, capped at 64 KiB.")]
    pub max_bytes: Option<u64>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SearchInItemParams {
    #[schemars(
        description = "The anchor of the tool call or command whose full output to search."
    )]
    pub anchor: String,
    #[schemars(description = "Substring to find in the output.")]
    pub query: String,
    #[serde(default)]
    #[schemars(description = "Lines of context around each match; default 2.")]
    pub context_lines: Option<u32>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct FileHistoryParams {
    #[schemars(description = "The thread id.")]
    pub thread_id: String,
    #[schemars(
        description = "The file path, as the thread's edits name it or relative to the project root."
    )]
    pub path: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum GitWhat {
    Status,
    Diff,
    Branches,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct GetGitParams {
    #[serde(default)]
    #[schemars(description = "The thread whose checkout to read (its worktree when it has one).")]
    pub thread_id: Option<String>,
    #[serde(default)]
    #[schemars(description = "Instead of a thread: the project whose root checkout to read.")]
    pub project_id: Option<String>,
    #[schemars(description = "What to read.")]
    pub what: GitWhat,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct GetPullRequestParams {
    #[schemars(description = "The thread the pull request is linked to.")]
    pub thread_id: String,
    #[schemars(description = "The pull request's key as get_thread lists it, or its URL.")]
    pub key: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ListProvidersParams {}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct GetSettingsParams {
    #[serde(default)]
    #[schemars(description = "Top-level settings keys to return; omit for all.")]
    pub keys: Option<Vec<String>>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ReadTerminalParams {
    #[schemars(description = "The terminal id, as get_thread lists it.")]
    pub terminal_id: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ReviewCandidatesParams {
    #[serde(default)]
    #[schemars(description = SCOPE)]
    pub scope: Option<ControlScope>,
    #[serde(default)]
    #[schemars(description = "Only threads active at or after this unix time in ms.")]
    pub since: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ThreadOp {
    Settle,
    Unsettle,
    Archive,
    Unarchive,
    Pin,
    Unpin,
    Rename { title: String },
    MarkRead,
    MarkUnread,
    FollowUp { on: bool, note: Option<String> },
    Park { on: bool },
    Keep { on: bool },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ThreadChange {
    #[schemars(description = "The thread id.")]
    pub thread_id: String,
    #[schemars(
        description = "One of settle, unsettle, archive, unarchive, pin, unpin, {\"rename\": {\"title\"}}, mark_read, mark_unread, {\"follow_up\": {\"on\", \"note\"?}}, {\"park\": {\"on\"}}, {\"keep\": {\"on\"}}."
    )]
    pub op: ThreadOp,
    #[serde(default)]
    #[schemars(
        description = "Skip this item as changed_since_preview when the thread no longer matches."
    )]
    pub expect: Option<Expect>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct UpdateThreadsParams {
    #[schemars(description = "The changes, applied in order; one result per change.")]
    pub changes: Vec<ThreadChange>,
    #[serde(default)]
    #[schemars(description = "Report would_apply and the side effects without changing anything.")]
    pub dry_run: Option<bool>,
    #[serde(default)]
    #[schemars(description = "Idempotency key: a retry with the same key does not apply twice.")]
    pub request_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ReviewItemInput {
    #[schemars(description = "The thread the item is about.")]
    pub thread_id: String,
    #[schemars(
        description = "continue, needs_decision, waiting_on_others, can_settle, parked_or_superseded or unsure."
    )]
    pub group: String,
    #[schemars(
        description = "In words: clear evidence, inference, or unsure. Never a percentage."
    )]
    pub confidence: String,
    #[schemars(description = "Anchors and quoted facts the grouping rests on.")]
    pub evidence: Vec<String>,
    #[serde(default)]
    #[schemars(description = "The suggested action, if any.")]
    pub suggestion: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct RecordReviewParams {
    #[schemars(description = "The reviewed items with their evidence.")]
    pub items: Vec<ReviewItemInput>,
    #[schemars(
        description = "What the review covered, for example \"3 projects, 41 threads, 4 without enough data\"."
    )]
    pub coverage: String,
    #[serde(default)]
    #[schemars(description = "Idempotency key: a retry with the same key does not record twice.")]
    pub request_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SettleSelfParams {
    #[schemars(
        description = "end_of_turn, {\"pr_merged\": {\"url\"}} or {\"pr_closed\": {\"url\"}}: when this thread settles."
    )]
    pub when: SelfSettleWhen,
    #[schemars(
        description = "The user's words, quoted verbatim, that consent to this thread settling itself."
    )]
    pub consent_quote: String,
    #[serde(default)]
    #[schemars(description = "Idempotency key.")]
    pub request_id: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Workspace {
    Local,
    Worktree,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct StartThreadParams {
    #[schemars(description = "The project the thread belongs to.")]
    pub project_id: String,
    #[serde(default)]
    #[schemars(
        description = "Working directory relative to the project root, inside it; default the project root."
    )]
    pub cwd: Option<String>,
    #[serde(default)]
    #[schemars(
        description = "local (the checkout, the default). worktree is refused: Tcode makes a thread's dedicated worktree only from a draft's first send."
    )]
    pub workspace: Option<Workspace>,
    #[schemars(
        description = "Provider name as list_providers lists it, for example claude or codex."
    )]
    pub provider: String,
    #[serde(default)]
    #[schemars(description = "Model id from list_providers; omit for the provider's default.")]
    pub model: Option<String>,
    #[serde(default)]
    #[schemars(
        description = "Provider profile id from list_providers; omit for the built-in profile."
    )]
    pub profile: Option<String>,
    #[serde(default)]
    #[schemars(
        description = "Provider option id → value, as list_providers describes them. The permission mode is not settable: the project's default applies."
    )]
    pub options: Option<serde_json::Map<String, serde_json::Value>>,
    #[serde(default)]
    #[schemars(description = "Thread title; omit to let Tcode title it.")]
    pub title: Option<String>,
    #[schemars(
        description = "The first message of the thread, sent as from this thread (origin agent)."
    )]
    pub message: String,
    #[serde(default)]
    #[schemars(
        description = "Image paths already in the attachments folder of a thread in your scope."
    )]
    pub attachments: Option<Vec<String>>,
    #[schemars(
        description = "Idempotency key, stored on the thread: a retry after a failure or restart returns the same thread."
    )]
    pub request_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SendMode {
    Queue,
    Steer,
    Schedule { at: u64 },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SendMessageParams {
    #[schemars(description = "The threads to send to; one result per thread.")]
    pub thread_ids: Vec<String>,
    #[schemars(description = "The message text.")]
    pub text: String,
    #[schemars(
        description = "queue (after the running turn), steer (into the running turn, else queued) or {\"schedule\": {\"at\": unix_secs}}."
    )]
    pub mode: SendMode,
    #[serde(default)]
    #[schemars(
        description = "Image paths already in the attachments folder of a thread in your scope."
    )]
    pub attachments: Option<Vec<String>>,
    #[serde(default)]
    #[schemars(description = "Idempotency key: a retry with the same key does not send twice.")]
    pub request_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RunOp {
    Interrupt,
    Retry {
        turn: String,
    },
    Compact,
    SetModel {
        model: String,
    },
    SetOption {
        id: String,
        value: serde_json::Value,
    },
    Fork,
    SteerQueued {
        id: u64,
    },
    DropQueued {
        id: u64,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct RunChange {
    #[schemars(description = "The thread id.")]
    pub thread_id: String,
    #[schemars(
        description = "One of interrupt, {\"retry\": {\"turn\"}}, compact, {\"set_model\": {\"model\"}} (a model of the thread's own provider and profile), {\"set_option\": {\"id\", \"value\"}}, fork, {\"steer_queued\": {\"id\"}}, {\"drop_queued\": {\"id\"}}. Dropping a queued human message needs history_edit."
    )]
    pub op: RunOp,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ControlThreadParams {
    #[schemars(description = "The changes, applied in order; one result per change.")]
    pub changes: Vec<RunChange>,
    #[serde(default)]
    #[schemars(description = "Idempotency key.")]
    pub request_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PullRequestOp {
    Link,
    Unlink,
    Watch,
    Unwatch,
    Refresh,
    Comment {
        body: String,
    },
    Reply {
        review_thread_id: String,
        body: String,
    },
    Resolve {
        review_thread_id: String,
    },
}

impl PullRequestOp {
    /// Comment, reply and resolve write to the host and need `pr_write`.
    pub fn writes(&self) -> bool {
        matches!(
            self,
            Self::Comment { .. } | Self::Reply { .. } | Self::Resolve { .. }
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct PullRequestParams {
    #[schemars(description = "The thread the pull request is or becomes linked to.")]
    pub thread_id: String,
    #[serde(default)]
    #[schemars(description = "The pull request's key as get_thread lists it.")]
    pub key: Option<String>,
    #[serde(default)]
    #[schemars(
        description = "Instead of key: a pull request URL or owner/repo#number, required for link."
    )]
    pub reference: Option<String>,
    #[schemars(
        description = "link, unlink, watch, unwatch, refresh, or a write: {\"comment\": {\"body\"}}, {\"reply\": {\"review_thread_id\", \"body\"}} (a reply in a review comment thread), {\"resolve\": {\"review_thread_id\"}}. Writes need pr_write."
    )]
    pub op: PullRequestOp,
    #[serde(default)]
    #[schemars(description = "Idempotency key.")]
    pub request_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum GitOp {
    CheckoutBranch {
        branch: String,
    },
    Commit {
        message: String,
        included: Option<Vec<String>>,
    },
    Push,
    Pull,
    CommitPush {
        message: String,
        included: Option<Vec<String>>,
    },
    PublishBranch,
    MergeWorktree,
}

impl GitOp {
    /// Push, pull, commit_push, publish_branch and merge_worktree need `git_remote`.
    pub fn remote(&self) -> bool {
        !matches!(self, Self::CheckoutBranch { .. } | Self::Commit { .. })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct GitParams {
    #[schemars(description = "The thread whose checkout to act in.")]
    pub thread_id: String,
    #[schemars(
        description = "{\"checkout_branch\": {\"branch\"}}, {\"commit\": {\"message\", \"included\"?}}, or with git_remote: push, pull, {\"commit_push\": {\"message\", \"included\"?}}, publish_branch, merge_worktree."
    )]
    pub op: GitOp,
    #[serde(default)]
    #[schemars(description = "Idempotency key.")]
    pub request_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct CreateProjectParams {
    #[schemars(description = "Absolute path of an existing directory on the host.")]
    pub root: String,
    #[serde(default)]
    #[schemars(description = "Idempotency key.")]
    pub request_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct MoveThreadParams {
    #[schemars(description = "The thread to move; your own thread id moves this thread.")]
    pub thread_id: String,
    #[schemars(description = "The project to move it into, from list_projects or create_project.")]
    pub project_id: String,
    #[serde(default)]
    #[schemars(
        description = "Working directory relative to the project root, which it must stay inside; the root when absent."
    )]
    pub cwd: Option<String>,
    #[serde(default)]
    #[schemars(description = "Idempotency key.")]
    pub request_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct NotifyWhenParams {
    #[schemars(description = "The threads to watch.")]
    pub thread_ids: Vec<String>,
    #[schemars(
        description = "Events that wake this thread: idle, turn_completed, needs_input, needs_approval, settled, pr_changed."
    )]
    pub events: Vec<WatchEvent>,
    #[serde(default)]
    #[schemars(description = "Remove the watch after it fires once.")]
    pub once: Option<bool>,
    #[serde(default)]
    #[schemars(description = "Idempotency key.")]
    pub request_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct CancelNotifyParams {
    #[schemars(description = "Watch ids notify_when returned.")]
    pub ids: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ReferenceParams {
    #[schemars(
        description = "The thread to reference; its archived log becomes readable to this thread."
    )]
    pub thread_id: String,
    #[serde(default)]
    #[schemars(description = "Why, shown to the user beside the reference.")]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct RespondInputParams {
    #[schemars(description = "The thread with the pending user-input request.")]
    pub thread_id: String,
    #[serde(default)]
    #[schemars(
        description = "The request id as get_thread lists it; omit for the only pending one."
    )]
    pub question_id: Option<String>,
    #[schemars(description = "Answers keyed by question id, as the request defines them.")]
    pub answers: serde_json::Map<String, serde_json::Value>,
    #[serde(default)]
    #[schemars(description = "Idempotency key.")]
    pub request_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TerminalOp {
    Open,
    Input { terminal_id: u64, text: String },
    Close { terminal_id: u64 },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TerminalParams {
    #[schemars(description = "The thread whose terminal panel to act in.")]
    pub thread_id: String,
    #[schemars(
        description = "open, {\"input\": {\"terminal_id\", \"text\"}} or {\"close\": {\"terminal_id\"}}."
    )]
    pub op: TerminalOp,
    #[serde(default)]
    #[schemars(description = "Idempotency key.")]
    pub request_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct DeleteThreadsParams {
    #[schemars(description = "The threads to delete; descendants go with them.")]
    pub thread_ids: Vec<String>,
    #[serde(default)]
    #[schemars(description = "Also remove each thread's dedicated worktree.")]
    pub remove_worktree: Option<bool>,
    #[serde(default)]
    #[schemars(description = "Report what would be deleted and delete nothing.")]
    pub dry_run: Option<bool>,
    #[serde(default)]
    #[schemars(description = "Idempotency key.")]
    pub request_id: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RewindScope {
    Files,
    Conversation,
    FilesAndConversation,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct RewindParams {
    #[schemars(description = "The thread to rewind.")]
    pub thread_id: String,
    #[schemars(description = "The turn number to rewind to, as outline lists it.")]
    pub turn: usize,
    #[schemars(
        description = "files, conversation or files_and_conversation: what the provider restores."
    )]
    pub mode: RewindScope,
    #[serde(default)]
    #[schemars(description = "Idempotency key.")]
    pub request_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct PatchSettingsParams {
    #[schemars(
        description = "A settings patch as {\"type\": <patch>, \"content\": <value>}, as get_settings shows the fields. Credentials, hosting, tcode_control grants and the settlement automation tier are refused whatever the grant."
    )]
    pub patch: serde_json::Value,
    #[serde(default)]
    #[schemars(description = "Idempotency key.")]
    pub request_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct RequestGrantParams {
    #[schemars(description = "project, {\"projects\": [ids]} or host.")]
    pub scope: ControlScope,
    #[serde(default)]
    #[schemars(description = "Tiers asked for: observe, organize, operate.")]
    pub tiers: Vec<ControlTier>,
    #[serde(default)]
    #[schemars(
        description = "Privileged categories asked for: answer_input, terminal, delete, git_remote, pr_write, history_edit, settings_write."
    )]
    pub categories: Vec<ControlCategory>,
    #[schemars(description = "Why, shown to the user with the request.")]
    pub reason: String,
}

// ---- observe responses ----

/// The keys `get_thread` and `review_candidates` name in a settlement explanation, one per
/// fact that keeps automatic settlement from applying.
pub mod settlement_reason {
    pub const ARCHIVED: &str = "archived";
    pub const SETTLED: &str = "settled";
    /// `settled_override` is Active: left by an unsettle or by pinning a settled thread.
    pub const OVERRIDE_ACTIVE: &str = "override_active";
    /// The per-thread auto-settle toggle is off ("keep").
    pub const KEEP: &str = "keep";
    pub const PINNED: &str = "pinned";
    pub const ORCHESTRATE_CHILD: &str = "orchestrate_child";
    pub const AUTOMATION_OFF: &str = "automation_off";
    pub const AUTOMATION_SUGGEST_ONLY: &str = "automation_suggest_only";
    pub const FOLLOW_UP: &str = "follow_up";
    pub const PARKED: &str = "parked";
    /// The user dismissed a settle suggestion and nothing happened in the thread since.
    pub const SUGGESTION_DISMISSED: &str = "suggestion_dismissed";
    /// A queued agent or server message that settling would drop.
    pub const PENDING_AGENT_MESSAGE: &str = "pending_agent_message";
    pub const PENDING_APPROVAL: &str = "pending_approval";
    pub const PENDING_INPUT: &str = "pending_input";
    pub const LIVE_RUN: &str = "live_run";
    pub const BACKGROUND_WORK: &str = "background_work";
    pub const UNSETTLED_CHILDREN: &str = "unsettled_children";
    pub const PULL_REQUEST_WATCHES: &str = "pull_request_watches";
    pub const PENDING_HUMAN_MESSAGE: &str = "pending_human_message";
    pub const SCHEDULED_WAKE: &str = "scheduled_wake";
    /// No inactivity rule applies (the settings turn it off) and no linked pull request ended.
    pub const TIME_RULE_OFF: &str = "time_rule_off";
    /// As [`TIME_RULE_OFF`], turned off by the thread's project override.
    pub const PROJECT_TIME_RULE_OFF: &str = "project_time_rule_off";
    /// The host has not loaded the thread's activity yet.
    pub const ACTIVITY_UNKNOWN: &str = "activity_unknown";
}

// operate responses

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StartThreadResponse {
    pub thread_id: String,
    /// False when an earlier call with the same `request_id` already started it.
    pub created: bool,
    pub title: String,
    pub cwd: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BatchResponse {
    pub results: Vec<BatchResult>,
}

/// One `control_thread` change; a fork names the thread it created.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunResult {
    #[serde(flatten)]
    pub result: BatchResult,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub new_thread_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ControlThreadResponse {
    pub results: Vec<RunResult>,
}

/// The outcome of one `git` or `pull_request` call, with what the host reported.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActionResponse {
    pub thread_id: String,
    pub outcome: Outcome,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<serde_json::Value>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreateProjectResponse {
    pub project_id: String,
    pub name: String,
    pub root: String,
    /// False when a project already covered the root.
    pub created: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MoveThreadResponse {
    pub thread_id: String,
    pub project_id: String,
    pub project_name: String,
    pub cwd: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReferenceResponse {
    pub thread_id: String,
    pub title: String,
    /// The thread is in another project or runs another provider, so its content goes to
    /// this thread's model vendor.
    pub boundary_notice: bool,
}
