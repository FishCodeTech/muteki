use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use agent::{
    ApprovalRequest, OptionDescriptor, OptionSelection, PlanStep, ProviderCommand, ProviderKind,
    RewindMode, TokenUsage,
};
use serde::{Deserialize, Serialize};
use tcode_core::{
    git::{GitAction, GitStatus},
    project::{Project, SessionMeta, WorktreeInfo},
    provider_status::ProviderSnapshot,
    session::{PendingUserInput, ReviewComment, RunningTurn, StoredEvent},
    settings::Settings,
    settlement::{AgentDelivery, AgentExecution, AgentReport},
    ui::{TerminalSplitDirection, WorkspaceMode},
};

/// Backward-compatible name for the core event record now used directly on
/// the protocol wire.
pub type SessionEventRecord = StoredEvent;

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "type", content = "content", rename_all = "snake_case")]
pub enum Topic {
    SessionEvents {
        session_id: String,
    },
    SessionStatus {
        session_id: String,
    },
    SessionPlan {
        session_id: String,
    },
    Index,
    SpaceIndex {
        space_id: String,
    },
    Scope,
    Settings,
    Providers,
    GitStatus {
        session_id: String,
    },
    RuntimeEvents,

    Terminal {
        terminal_id: u64,
    },
    /// One command execution's output, rendered by the host at `cols`
    /// (clamped into [`crate::STORED_OUTPUT_COLS`]). Answered on subscribe and
    /// again while the output grows.
    CommandOutput {
        session_id: String,
        item_id: String,
        cols: u16,
    },
    Preview,
    ExternalImport {
        project_id: String,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EventEnvelope {
    /// Present only on subscription replies; mux routes these to their requester.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<u64>,
    pub topic: Topic,
    pub event: ServerEvent,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "content", rename_all = "snake_case")]
pub enum ServerEvent {
    /// The host's whole terminal grid, sent on subscribe and after a restart.
    TerminalFrame {
        terminal_id: u64,
        frame: Box<crate::terminal::TerminalFrame>,
    },
    /// One coalesced grid update, continuing from the last frame or delta.
    TerminalDelta {
        terminal_id: u64,
        delta: Box<crate::terminal::TerminalDelta>,
    },
    /// The whole rendered screen of a [`Topic::CommandOutput`]: no cursor and
    /// no scrollback, trailing blank rows trimmed.
    CommandOutputReplaced(Box<crate::terminal::TerminalFrame>),
    PreviewRequest {
        request_id: u64,
        session_id: String,
        request: crate::PreviewRequest,
    },
    SessionEvent(StoredEvent),
    SessionStatusReplaced(Box<SessionStatus>),
    SessionUsageChanged {
        usage: Option<TokenUsage>,
        context_window: Option<u64>,
    },
    SessionPlanReplaced(SessionPlan),
    ProvidersReplaced(ProvidersStatus),
    GitStatusReplaced(GitStatusStatus),
    /// An unarchived thread was added or changed. Archiving a thread removes
    /// it with [`ServerEvent::IndexRemoveSession`].
    IndexUpsertSession(SessionMeta),
    IndexUpsertProject(Project),
    IndexSummaryReplaced(IndexSummary),
    IndexActivitiesChanged {
        upserts: HashMap<String, SessionActivity>,
        removed: Vec<String>,
    },
    IndexUnchanged {
        fingerprint: String,
        archived_revision: u64,
    },
    IndexRemoveSession {
        session_id: String,
    },
    IndexRemoveProject {
        project_id: String,
    },
    SettingsReplaced(Settings),
    /// Visit times that changed while the rest of the settings did not.
    LastVisitedChanged(HashMap<String, u64>),
    Runtime(RuntimeNotification),

    /// A provider-native rewind prompt is delivered once over the serialized
    /// event stream. The client store owns consumption after receipt.
    NativeRewindPrefill {
        session_id: String,
        text: String,
    },
    /// A turn was stopped because Claude Code's safety classifier fired: the request
    /// was blocked (fallback prevented) or the model silently fell back and tcode
    /// interrupted it. Drives the recovery card. Transient; never persisted.
    ModelFallbackBlocked {
        session_id: String,
        category: Option<agent::ClassifierCategory>,
        /// The selected model that refused / was expected.
        model: Option<String>,
        /// The model Claude rerouted to, when a silent fallback actually occurred
        /// (tcode interrupted it). None when the request was blocked outright.
        fallback_model: Option<String>,
        /// Claude's own explanation text when available (empty otherwise).
        detail: String,
    },
    /// A secondary model's review of a classifier-blocked turn: a user-facing
    /// assessment plus a DRAFT clarification the user reviews and sends. Advisory
    /// only; never auto-sent. Transient.
    FallbackReviewReady {
        session_id: String,
        assessment: String,
        /// Suggested first-person clarification, prefilled but editable. Empty when
        /// the reviewer judged the flag not a false positive.
        draft: String,
    },
    /// Records standing for the log cursors `from..end`. A cursor is the
    /// position of a stored row of the log, and a row without a readable
    /// record stands for none; the host may also merge or drop records whose
    /// effect a later record in the log repeats, so `records` can be shorter
    /// than the range.
    SessionSnapshot {
        from: u64,
        end: u64,
        records: Vec<StoredEvent>,
        #[serde(default)]
        total: u64,
        /// Absolute turn count keeps turn-addressed actions correct in a window.
        #[serde(default)]
        total_turns: u64,
        /// The byte budget reduced this reply below the requested record count.
        #[serde(default)]
        truncated: bool,
    },
    SessionHistoryError(crate::ProtocolError),
    IndexSnapshot(IndexSnapshot),
    ScopeSnapshot(Scope),
    SettingsSnapshot(Settings),
    /// The current (or latest) external-import run for one project. `None`
    /// means no run has ever started, or the project is gone. Only the latest
    /// run is retained, so a subscriber that attaches after a fast completion
    /// still recovers the outcome from the subscription snapshot.
    ExternalImportStatusReplaced {
        project_id: String,
        status: Option<ExternalImportStatus>,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "content", rename_all = "snake_case")]
pub enum Scope {
    Full,
    Space {
        space_id: String,
        space_name: String,
        projects: Vec<Project>,
        providers: Vec<ScopedProviderChoice>,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScopedProviderChoice {
    pub provider: ProviderKind,
    pub profile_id: Option<String>,
    pub name: String,
    pub models: Vec<agent::ModelSpec>,
}

/// Host-owned progress for one external-history import run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExternalImportStatus {
    /// Host-generated, so a late subscriber can tell a replayed older run from
    /// the run it started.
    pub run_id: u64,
    pub state: ExternalImportState,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "content", rename_all = "snake_case")]
pub enum ExternalImportState {
    Progress {
        done: usize,
        total: usize,
        tool: String,
    },
    Finished {
        imported: usize,
        skipped: usize,
    },
}

/// Full provider/settings-page read projection.
///
/// Provider state changes infrequently and its pieces are consumed together,
/// so hosts replace this single value after any covered mutation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct ProvidersStatus {
    pub model_catalogs: HashMap<ProviderKind, Vec<agent::ModelSpec>>,
    pub models_loading: HashMap<ProviderKind, bool>,
    pub provider_versions: HashMap<ProviderKind, ProviderVersionStatus>,
    pub update_run: Option<ProviderUpdateRun>,
    pub tcode_update: TcodeUpdateStatus,
    pub provider_snapshots: HashMap<String, ProviderSnapshot>,
    /// Latest account usage per profile id (Codex / Claude Code only).
    #[serde(default)]
    pub provider_usage: HashMap<String, tcode_core::usage::ProviderUsage>,
    /// Profile ids with a usage fetch in flight.
    #[serde(default)]
    pub usage_checking: HashSet<String>,
    pub acp_marketplace_items: Vec<AcpMarketplaceItem>,
    pub acp_registry_loading: bool,
    pub acp_registry_error: Option<String>,
    pub acp_installing: HashSet<String>,
    pub providers_checked_at: Option<u64>,
    pub providers_checking: bool,
    /// Environment-variable names present for each profile. Values never cross
    /// the protocol boundary.
    pub secret_names: HashMap<String, HashSet<String>>,
    /// Native plugin catalogs of the profiles a client asked about, for
    /// providers whose plugin management is switched on.
    #[serde(default)]
    pub plugins: Vec<ProviderPluginCatalog>,
}

/// One provider profile's native plugin catalog. The native CLI's own files
/// are the truth; the host only keeps the last listing in memory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderPluginCatalog {
    pub profile_id: String,
    /// The project directory the catalog was listed from; `None` lists from
    /// outside any project, so only user scope is offered.
    #[serde(default)]
    pub context_cwd: Option<PathBuf>,
    #[serde(default)]
    pub marketplaces: Vec<agent::ProviderPluginMarketplace>,
    #[serde(default)]
    pub marketplace_actions: Vec<agent::MarketplaceAction>,
    #[serde(default)]
    pub entries: Vec<agent::ProviderPluginEntry>,
    /// Native errors for parts of the catalog that could not be read; the
    /// rest of the listing still stands.
    #[serde(default)]
    pub errors: Vec<String>,
    pub state: PluginCatalogState,
    #[serde(default)]
    pub loading: bool,
    /// Plugin ids or marketplace names with an operation in flight.
    #[serde(default)]
    pub pending: Vec<String>,
    #[serde(default)]
    pub challenges: Vec<PluginChallenge>,
}

/// How far the entries can be trusted. A failed listing keeps the last
/// known entries rather than clearing them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "content", rename_all = "snake_case")]
pub enum PluginCatalogState {
    Fresh,
    Stale { reason: PluginStaleReason },
    Error { message: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PluginStaleReason {
    NotLoaded,
    /// Listed for another directory or profile configuration.
    ContextChanged,
    /// A native change went through and has not been listed yet.
    Changed,
}

/// A native confirmation an operation is waiting on. It is answered with
/// [`crate::Command::ResolvePluginChallenge`]; a command the CLI shows again
/// arrives as a new challenge with a new `op_id`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginChallenge {
    pub op_id: RuntimeOperationId,
    pub profile_id: String,
    /// The plugin id or marketplace name the operation targets.
    pub entry_id: String,
    pub kind: PluginChallengeKind,
    #[serde(default)]
    pub native_text: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "content", rename_all = "snake_case")]
pub enum PluginChallengeKind {
    /// Run this marketplace-declared command.
    AcceptCommand {
        command: String,
        sha256: String,
        #[serde(default)]
        mode: Option<String>,
    },
    /// The native operation has no confirmation of its own but removes more
    /// than its target: these installed plugins, in every scope.
    ConfirmDestructive { uninstalls: Vec<String> },
}

/// The native operation a plugin toast reports on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "content", rename_all = "snake_case")]
pub enum PluginOperationTarget {
    Plugin {
        action: agent::PluginActionKind,
        scope: agent::PluginScope,
        plugin: String,
    },
    AddMarketplace {
        source: String,
    },
    RemoveMarketplace {
        name: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderUpdateRun {
    pub total: usize,
    pub completed: usize,
    pub current: Option<ProviderKind>,
    pub failed: Vec<ProviderKind>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderUpdateAvailable {
    pub provider: ProviderKind,
    pub version: String,
    pub automatic: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ProviderVersionStatus {
    pub installed: Option<String>,
    pub latest: Option<String>,
    pub update_available: bool,
    pub checking: bool,
    pub updating: bool,
    pub update_command: Option<String>,
    /// Compact label; `update_command` retains the copyable execution context.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub update_command_summary: Option<String>,
    /// The command needs an interactive terminal (for example, elevation).
    #[serde(default)]
    pub update_requires_terminal: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct TcodeUpdateStatus {
    pub current: String,
    pub latest: Option<String>,
    pub release_url: Option<String>,
    pub update_available: bool,
    pub checking: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AcpMarketplaceItem {
    pub id: String,
    pub name: String,
    pub version: String,
    pub description: String,
    pub installed: bool,
    pub installing: bool,
    pub supported: bool,
}

/// Full active-workspace Git projection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct GitStatusStatus {
    pub status: Option<GitStatus>,
    pub busy: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ForkAvailability {
    Available,
    Unsupported,
    Empty,
    Running,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionActivity {
    pub working: bool,
    pub turn_running: bool,
    /// Background tasks run, or a child thread holds this thread: one still
    /// runs, or a dispatched one finished and awaits its lead's settle.
    pub waiting: bool,
    pub waiting_for_approval: bool,
    pub waiting_for_input: bool,
    /// The latest run ended in an error and nothing has run since.
    pub failed: bool,
    pub unread: bool,
    pub fork: ForkAvailability,
    /// A dispatched orchestrate child's execution and delivery; absent for
    /// every other thread.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<AgentStatus>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentStatus {
    pub execution: AgentExecution,
    pub delivery: AgentDelivery,
    /// Whether the child has reported to its lead (see `AgentReport`).
    #[serde(default)]
    pub report: AgentReport,
    /// When its latest run started and ended (unix ms); an end before the
    /// start belongs to an earlier run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_started_at: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_completed_at: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionPlan {
    pub session_id: String,
    pub steps: Vec<PlanStep>,
}

/// Host-owned runtime and full-history facts for one resident session.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionStatus {
    pub session_id: String,
    pub title: String,
    pub cwd: PathBuf,
    pub attachments_dir: PathBuf,
    pub provider: ProviderKind,
    pub requested_model: Option<String>,
    pub requested_profile_id: Option<String>,
    pub acp_agent_id: Option<String>,
    pub project_id: Option<String>,
    pub queued_messages: Vec<QueuedMessageStatus>,
    /// Backend-owned drafts used by provider-bound send-path assembly.
    pub review_comment_drafts: Vec<ReviewComment>,
    pub terminals: Vec<TerminalStatus>,
    pub active_terminal_id: Option<u64>,
    pub terminal_splits: Vec<TerminalSplitStatus>,
    pub terminal_contexts: Vec<TerminalContextStatus>,
    pub terminal_open: bool,
    pub terminal_height: f32,
    pub delivery_in_flight: Option<u64>,
    pub activity: SessionActivity,
    /// An interrupt was accepted by the provider and its turn has not ended yet.
    #[serde(default)]
    pub stopping: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_retry: Option<agent::TurnRetry>,
    /// The harness's standing goal (Codex `thread/goal`, Claude Code `/goal`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub goal: Option<agent::Goal>,
    pub native_rewind_blocked: bool,
    pub checkout_blocked: bool,
    pub conversation_read_only: bool,
    pub terminal_limit_reached: bool,
    pub terminal_split_available: bool,
    pub usage: Option<TokenUsage>,
    /// Effective capacity the context meter should use, resolved by the host.
    pub context_window: Option<u64>,
    /// The live turn and the requests it waits on. A client's fold of a
    /// history window can place them but cannot decide them: the window may
    /// begin after they opened, and a provider can stop without a record.
    pub running_turn: Option<RunningTurn>,
    pub pending_approvals: Vec<ApprovalRequest>,
    pub pending_user_input: Option<PendingUserInput>,
    #[serde(rename = "supports_steering")]
    pub steering_supported: bool,
    pub provider_option_descriptors: Vec<OptionDescriptor>,
    pub provider_option_selections: Vec<OptionSelection>,
    pub provider_option_requested_selections: Vec<OptionSelection>,
    pub provider_commands: Vec<ProviderCommand>,
    pub git_branch: Option<String>,
    pub branches: Vec<String>,
    pub draft: bool,
    pub draft_workspace: WorkspaceMode,
    pub worktree: Option<WorktreeInfo>,
    pub preparing_worktree: bool,
    pub relay_confirmation: Option<(String, String)>,
    pub native_rewind_pending: bool,
    pub native_rewind_prefill_available: bool,
    pub model_pending_restart: bool,
    pub options_pending_restart: bool,
    /// The MCP tools offered to this thread's provider, with every text Tcode gives its
    /// model outside the conversation; absent when the provider takes no MCP server.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub injected_tools: Option<InjectedTools>,
    /// The thread's `tcode_control` grant and what rests on it; absent when the provider
    /// takes no MCP server.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub control: Option<ControlStatus>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ControlStatus {
    pub grant: tcode_core::control::ControlGrant,
    pub source: tcode_core::control::ControlGrantSource,
    pub pending_requests: Vec<tcode_core::control::ControlGrantRequest>,
    pub references: Vec<ThreadReferenceStatus>,
    pub self_settle: Option<tcode_core::control::SelfSettleGrant>,
    /// Whether the thread's provider is given the `tcode_control` server when it starts.
    pub tools_offered: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThreadReferenceStatus {
    pub thread_id: String,
    pub title: String,
    /// The referenced thread no longer exists on the host.
    pub deleted: bool,
    pub by: tcode_core::control::ReferenceOrigin,
    pub boundary_notice: bool,
}

pub use tcode_core::control::ControlActivityEntry;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InjectedTools {
    /// Each tool's name and the description the model reads, across every server offered.
    pub tools: Vec<(String, String)>,
    /// Which server offers them: `tools` holds each server's tools together, in this order.
    #[serde(default)]
    pub servers: Vec<InjectedServer>,
    /// False once the provider took none of Tcode's MCP servers: its model has neither these
    /// tools nor the linking instructions.
    pub attached: bool,
    /// Verbatim, as delivered.
    pub instructions: Vec<InjectedInstructions>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InjectedServer {
    pub name: String,
    /// How many consecutive entries of `InjectedTools::tools` are this server's.
    pub tools: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InjectedInstructions {
    pub channel: InjectedChannel,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InjectedChannel {
    /// The pull request linking instructions, through the provider's own channel.
    Linking(agent::InstructionChannel),
    /// An MCP server's `instructions`, by server name.
    McpServer(String),
    /// Codex's thread `developerInstructions`.
    DeveloperInstructions,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalStatus {
    pub id: u64,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub exited: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalSplitStatus {
    pub first: u64,
    pub second: u64,
    pub direction: TerminalSplitDirection,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalContextStatus {
    pub id: u64,
    pub terminal_label: String,
    pub line_start: usize,
    pub line_end: usize,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueuedMessageStatus {
    pub id: u64,
    pub editable: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delivery_key: Option<String>,
    pub text: String,
    /// Unix timestamp for a scheduled row, or `None` for an ordinary queued
    /// message. Like the queue itself, this status is ephemeral.
    #[serde(default)]
    pub fire_at_unix_secs: Option<u64>,
}

/// The unarchived threads and every project. Archived threads are read on
/// demand with [`crate::Query::ArchivedSessions`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IndexSnapshot {
    #[serde(flatten)]
    pub summary: IndexSummary,
    pub sessions: Vec<SessionMeta>,
    pub projects: Vec<Project>,
}

/// Index facts that are not one thread's metadata.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexSummary {
    /// Host-authored activity for every unarchived stored session.
    #[serde(default)]
    pub activity: HashMap<String, SessionActivity>,
    /// Background title requests, including threads with no live provider.
    #[serde(default)]
    pub title_generating: HashSet<String>,
    /// Archived threads per project id.
    #[serde(default)]
    pub archived_counts: HashMap<String, usize>,
    pub worktree_shared: HashSet<String>,
    pub archived_revision: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ArchivedSessions {
    pub sessions: Vec<SessionMeta>,
    pub worktree_shared: HashSet<String>,
    pub revision: u64,
}

/// A transient runtime notification delivered to clients.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "content", rename_all = "snake_case")]
pub enum RuntimeNotification {
    Error(RuntimeError),
    Notice(RuntimeNotice),
    Toast(RuntimeToast),
    Effect(RuntimeEffect),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "content", rename_all = "snake_case")]
pub enum RuntimeEffect {
    ApplyLocale { language: Option<String> },
    CopyToClipboard { text: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct RuntimeOperationId(pub u64);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitActionRequest {
    pub session_id: String,
    pub action: GitAction,
    pub message: Option<String>,
    pub included: Option<Vec<String>>,
    pub feature_branch: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MergeWorktreeFailure {
    Missing,
    DirtyWorktree,
    DestinationDetached,
    DirtyDestination,
    DivergedConflict,
    Git,
}

#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "content", rename_all = "snake_case")]
pub enum RuntimeToast {
    /// A watched pull request woke its thread's agent, or its watch ended with a notice.
    PullRequestWatch {
        session_id: String,
        number: u64,
        notice: tcode_core::pull_request_watch::WatchNotice,
    },
    /// How a write to a native stack ended after its command was answered: a followed merge,
    /// a rebase, or an unconfirmed merge a sync has since read. `session_id` is the thread
    /// `target` is opened from.
    PullRequestStack {
        session_id: String,
        target: tcode_core::pull_request::PullRequestKey,
        stack: u64,
        layers: Vec<u64>,
        base: String,
        result: crate::PullRequestActionResult,
        /// A merge that ended after the host stopped following it.
        late: bool,
    },
    ProviderUpdatesAvailable {
        updates: Vec<ProviderUpdateAvailable>,
    },
    ProviderUpdateStarted {
        operation: RuntimeOperationId,
        run: ProviderUpdateRun,
    },
    ProviderUpdateProgress {
        operation: RuntimeOperationId,
        run: ProviderUpdateRun,
    },
    ProviderUpdateFinished {
        operation: RuntimeOperationId,
        run: ProviderUpdateRun,
    },
    GitBusy,
    GitStarted {
        operation: RuntimeOperationId,
        action: GitAction,
    },
    GitSucceeded {
        operation: RuntimeOperationId,
        action: GitAction,
    },
    GitFailed {
        operation: RuntimeOperationId,
        detail: String,
        retry: GitActionRequest,
    },
    CommitMessageGenerated {
        message: String,
    },
    CommitMessageFailed {
        detail: String,
    },
    AcpInstallStarted {
        operation: RuntimeOperationId,
        name: String,
    },
    AcpInstallSucceeded {
        operation: RuntimeOperationId,
        name: String,
    },
    AcpInstallFailed {
        operation: RuntimeOperationId,
        name: String,
        detail: String,
    },
    PluginOperationStarted {
        operation: RuntimeOperationId,
        profile_id: String,
        target: PluginOperationTarget,
    },
    PluginOperationSucceeded {
        operation: RuntimeOperationId,
        profile_id: String,
        target: PluginOperationTarget,
    },
    PluginOperationFailed {
        operation: RuntimeOperationId,
        profile_id: String,
        target: PluginOperationTarget,
        detail: String,
    },
}

#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "content", rename_all = "snake_case")]
pub enum RuntimeError {
    External(String),
    PersistSettings {
        error: String,
    },
    TerminalStart {
        error: String,
    },
    TerminalRestart {
        error: String,
    },
    PersistProject {
        error: String,
    },
    WorktreeRemove {
        error: String,
    },
    DeleteSession {
        error: String,
    },
    DeleteProject {
        error: String,
    },
    NativeRewindBlocked,
    PersistEvent {
        error: String,
    },
    WorktreeAdd {
        error: String,
    },
    PersistSession {
        error: String,
    },
    TitleGenerationFailed,
    TitleGenerationEmpty,
    ProcessGone,
    SteerUnsupported {
        agent: String,
    },
    DirtyTree,
    ProviderStart {
        error: String,
    },
    /// A provider could not start because the thread's working directory is
    /// gone; `project_id` lets a client offer to re-point or remove the project.
    WorkingDirectoryMissing {
        session_id: String,
        project_id: Option<String>,
        cwd: PathBuf,
    },
    ProviderClosed {
        reason: Option<String>,
    },
    PersistSessionIndex {
        error: String,
    },
    ProviderMessage(String),
}

#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "content", rename_all = "snake_case")]
pub enum RuntimeNotice {
    ProviderMessage(String),
    TcodeUpdateAvailable {
        version: String,
    },
    NativeRewindCompleted {
        mode: RewindMode,
    },
    SwitchedBranch {
        branch: String,
    },
    WorktreeSeeded {
        copied_files: usize,
        skipped: Vec<String>,
        limit_reached: bool,
    },
    WorktreeMergedFastForward,
    WorktreeMergedCommit,
    WorktreeMergeFailed {
        reason: MergeWorktreeFailure,
        detail: Option<String>,
    },
    /// A thread's model recorded a conversation review.
    ReviewRecorded {
        items: usize,
        coverage: String,
    },
    /// A thread's model referenced a thread in another project or of another provider, so
    /// that thread's content may now reach this thread's model vendor; the user is told once.
    ReferenceBoundaryCrossed {
        thread: String,
        thread_title: String,
        referenced: String,
        title: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoticeSeverity {
    Success,
    Warning,
}

impl RuntimeNotice {
    pub fn severity(&self) -> NoticeSeverity {
        match self {
            Self::ProviderMessage(_)
            | Self::WorktreeMergeFailed { .. }
            | Self::ReferenceBoundaryCrossed { .. } => NoticeSeverity::Warning,
            Self::TcodeUpdateAvailable { .. }
            | Self::NativeRewindCompleted { .. }
            | Self::SwitchedBranch { .. }
            | Self::WorktreeSeeded { .. }
            | Self::WorktreeMergedFastForward
            | Self::WorktreeMergedCommit
            | Self::ReviewRecorded { .. } => NoticeSeverity::Success,
        }
    }
}
