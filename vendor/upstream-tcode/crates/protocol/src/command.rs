use std::path::PathBuf;

use agent::{ApprovalDecision, PluginScope, ProviderKind, RewindMode};
use serde::{Deserialize, Serialize};
use tcode_core::{
    acp::AcpAgentPatch,
    git::GitAction,
    session::ReviewComment,
    settings::ProfileSettingsPatch,
    ui::{TerminalSplitDirection, WorkspaceMode},
};

pub use tcode_core::settings::SettingsPatch;

use crate::{ExternalThread, RuntimeOperationId};

/// Fallback cell metrics matching the host emulator's own defaults, used when
/// a client resizes without knowing its physical cell size.
fn default_cell_width() -> u16 {
    8
}

fn default_cell_height() -> u16 {
    17
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalSelection {
    pub line_start: usize,
    pub line_end: usize,
    pub text: String,
}

/// User-selectable thread export formats.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThreadExportFormat {
    /// A tcode metadata record followed by the session's native JSONL event log.
    Jsonl,
    /// A readable, deterministic rendering of the folded conversation timeline.
    Markdown,
}

/// A backend mutation requested by a client.
///
/// Variants correspond to serializable `AppState` mutations used by the UI.
/// UI-only consuming selectors are intentionally absent.
#[allow(clippy::large_enum_variant)] // Wire DTOs preserve direct, typed payload fields.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "content", rename_all = "snake_case")]
pub enum Command {
    TerminalInput {
        terminal_id: u64,
        #[serde(with = "crate::wire::base64_bytes")]
        bytes: Vec<u8>,
    },
    ResizeTerminal {
        terminal_id: u64,
        cols: u16,
        rows: u16,
        /// Physical cell size, which the host needs to answer a program's
        /// pixel-size queries (CSI 14 t).
        #[serde(default = "default_cell_width")]
        cell_width: u16,
        #[serde(default = "default_cell_height")]
        cell_height: u16,
    },
    /// Clear the host grid and its scrollback, keeping the current prompt line.
    ClearTerminal {
        terminal_id: u64,
    },
    PreviewReply {
        request_id: u64,
        response: Result<crate::PreviewResponse, String>,
    },
    /// Consume and apply the restart-continuity marker, returning the Settings
    /// section that the client should open.
    ApplyPendingRelaunch,
    /// Open the newest stored session without exposing the host's index.
    OpenLatestSession,
    /// Shut down every live provider and PTY, then acknowledge only after the
    /// FIFO store-write barrier has drained.
    ShutdownAllAndFlush,
    OrchestrateTurn {
        session_id: String,
        text: String,
        attachment_paths: Vec<PathBuf>,
    },
    ReloadProvider,
    SetHostToken {
        host: String,
        token: Option<String>,
        /// The account email a host that takes one authenticates the token with; with no
        /// token, the one already saved with an email goes on with this one.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        email: Option<String>,
    },
    RefreshHostCredentials,
    LinkPullRequest {
        session_id: String,
        reference: String,
    },
    UnlinkPullRequest {
        session_id: String,
        key: tcode_core::pull_request::PullRequestKey,
    },
    WatchPullRequest {
        session_id: String,
        key: tcode_core::pull_request::PullRequestKey,
        watching: bool,
    },
    /// Pauses every watch of the thread, or resumes them and catches up on what changed.
    SetPullRequestWatchesPaused {
        session_id: String,
        paused: bool,
    },
    /// Drops the host's reads of a linked pull request and syncs it, for a manual refresh.
    RefreshPullRequest {
        session_id: String,
        key: tcode_core::pull_request::PullRequestKey,
    },
    /// The signed-in account's viewed mark on files of a linked pull request.
    SetPullRequestFilesViewed {
        session_id: String,
        key: tcode_core::pull_request::PullRequestKey,
        paths: Vec<String>,
        viewed: bool,
    },
    /// A write to a linked pull request, answered with [`CommandResponse::PullRequestAction`].
    /// It is never retained for redelivery: a write resent after a lost answer could apply twice.
    RunPullRequestAction {
        session_id: String,
        key: tcode_core::pull_request::PullRequestKey,
        action: crate::PullRequestAction,
    },
    EditPullRequestReviewDraft {
        session_id: String,
        key: tcode_core::pull_request::PullRequestKey,
        edit: tcode_core::pull_request::PullRequestReviewDraftEdit,
    },
    SetProfileSecret {
        profile_id: String,
        name: String,
        value: Option<String>,
    },
    UpdateProfileSettings {
        profile_id: String,
        patch: ProfileSettingsPatch,
    },
    CreateThirdPartyProfile {
        name: String,
        base_url: String,
        model: Option<String>,
        api_key: String,
    },
    DeleteProfile {
        profile_id: String,
    },
    RefreshProviderStatus,
    /// Re-fetch account usage / rate-limit windows for every usage-capable
    /// provider profile (Codex, Claude Code).
    RefreshProviderUsage,
    CheckProviderVersions,
    UpdateProviders {
        providers: Vec<ProviderKind>,
    },
    SetSidebarCollapsed {
        collapsed: bool,
    },
    RunGitAction {
        session_id: String,
        action: GitAction,
        message: Option<String>,
        included: Option<Vec<String>>,
        feature_branch: Option<String>,
    },
    RefreshAcpRegistry,
    InstallAcpAgent {
        id: String,
    },
    RemoveAcpAgent {
        id: String,
    },
    AddCustomAcpAgent {
        name: String,
        command: String,
        args: Vec<String>,
        env: Vec<(String, String)>,
    },
    UpdateAcpAgent {
        id: String,
        patch: AcpAgentPatch,
    },
    SetActiveAcpAgent {
        session_id: String,
        id: String,
    },
    /// List a profile's native plugins as seen from `cwd` (a project
    /// directory), or from outside any project when `cwd` is `None`.
    RefreshProviderPlugins {
        profile_id: String,
        #[serde(default)]
        cwd: Option<PathBuf>,
    },
    InstallProviderPlugin {
        profile_id: String,
        entry_id: String,
        scope: PluginScope,
        #[serde(default)]
        cwd: Option<PathBuf>,
    },
    UninstallProviderPlugin {
        profile_id: String,
        entry_id: String,
        scope: PluginScope,
        #[serde(default)]
        cwd: Option<PathBuf>,
    },
    SetProviderPluginEnabled {
        profile_id: String,
        entry_id: String,
        scope: PluginScope,
        enabled: bool,
        #[serde(default)]
        cwd: Option<PathBuf>,
    },
    UpdateProviderPlugin {
        profile_id: String,
        entry_id: String,
        scope: PluginScope,
        #[serde(default)]
        cwd: Option<PathBuf>,
    },
    AddProviderMarketplace {
        profile_id: String,
        source: String,
        #[serde(default)]
        cwd: Option<PathBuf>,
    },
    RemoveProviderMarketplace {
        profile_id: String,
        name: String,
        #[serde(default)]
        cwd: Option<PathBuf>,
    },
    ResolvePluginChallenge {
        op_id: RuntimeOperationId,
        accept: bool,
    },
    ResetSettings,
    WriteRelaunchMarker {
        session_id: String,
        reopen_settings: String,
    },
    ClearRelaunchMarker,
    SetTerminalHeight {
        session_id: String,
        height: f32,
    },
    ToggleTerminalPanel {
        session_id: String,
    },
    CloseTerminalPanel {
        session_id: String,
    },
    RestartTerminal {
        session_id: String,
    },
    NewTerminal {
        session_id: String,
    },
    SplitTerminal {
        session_id: String,
        direction: TerminalSplitDirection,
    },
    ActivateTerminal {
        session_id: String,
        terminal_id: u64,
    },
    CloseTerminal {
        session_id: String,
        terminal_id: u64,
    },
    CaptureTerminalSelection {
        session_id: String,
        terminal_id: u64,
        /// Client-grid selection; absent retains the local-handle behavior.
        #[serde(default)]
        selection: Option<TerminalSelection>,
    },
    RemoveTerminalContext {
        session_id: String,
        context_id: u64,
    },
    AddReviewComment {
        session_id: String,
        comment: ReviewComment,
    },
    RemoveReviewComment {
        session_id: String,
        index: usize,
    },
    CycleProjectSort,
    /// A normalized PNG override, or None to use the project config again.
    SetProjectIcon {
        project_id: String,
        png: Option<Vec<u8>>,
    },
    /// Register a project rooted at `root`. The host validates the path against
    /// its own filesystem — a client never decides whether a host path is
    /// absolute or exists — and answers `invalid_project_root` when it is not an
    /// absolute, existing directory there.
    CreateProject {
        root: PathBuf,
    },
    CreateNewProject {
        name: String,
    },
    /// Point a project at `root`, carrying every thread's working directory
    /// under the old root along. With `move_files` the host moves the old
    /// directory to `root` first, which must not exist yet; without it `root`
    /// must already be a directory there. A project already at `root` absorbs
    /// this one, threads included, and this one's id disappears. Refused with
    /// `project_busy` while a thread of the project has a turn in flight.
    SetProjectRoot {
        project_id: String,
        root: PathBuf,
        move_files: bool,
    },
    /// Start an import run. Progress, completion and the finalized index are
    /// host-owned: subscribe to [`crate::Topic::ExternalImport`] before sending
    /// this, and read the outcome from that replicated status. Returns
    /// [`CommandResponse::ExternalImportStarted(false)`] for an unknown
    /// project, and an `import_in_progress` error for a second concurrent run.
    StartExternalImport {
        project_id: String,
        threads: Vec<ExternalThread>,
    },
    ToggleProjectCollapsed {
        project_id: String,
    },
    PatchSettings {
        patch: SettingsPatch,
    },
    SettleChildren {
        session_id: String,
    },
    SettleSession {
        session_id: String,
    },
    /// Set or clear the thread's own `tcode_control` grant.
    SetControlGrant {
        session_id: String,
        grant: Option<tcode_core::control::ControlGrant>,
    },
    /// Answer a grant the thread's model asked for with `request_grant`.
    RespondControlGrant {
        session_id: String,
        request_id: String,
        approve: bool,
    },
    AddThreadReference {
        session_id: String,
        thread_id: String,
    },
    RemoveThreadReference {
        session_id: String,
        thread_id: String,
    },
    SetFollowUp {
        session_id: String,
        follow_up: Option<tcode_core::control::FollowUp>,
    },
    SetParked {
        session_id: String,
        parked: bool,
    },
    DismissSettleSuggestion {
        session_id: String,
    },
    RevokeSelfSettle {
        session_id: String,
    },
    /// Orchestrate `cancel` for a dispatched child: stop it without
    /// delivering its result.
    CancelAgent {
        session_id: String,
    },
    SetAutoSettle {
        session_id: String,
        enabled: bool,
    },
    UnsettleSession {
        session_id: String,
    },
    /// Pin a thread, promoting it out of Settled. A thread pinned for the
    /// first time takes `order_key`; one already pinned keeps its own.
    PinSession {
        session_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        order_key: Option<String>,
    },
    UnpinSession {
        session_id: String,
    },
    ReorderPinned {
        session_id: String,
        order_key: String,
    },
    ReorderActive {
        session_id: String,
        order_key: String,
    },
    ArchiveSession {
        session_id: String,
    },
    UnarchiveSession {
        session_id: String,
    },
    RenameSession {
        session_id: String,
        title: String,
    },
    RegenerateSessionTitle {
        session_id: String,
    },
    ForkThread {
        id: String,
    },
    /// Move a thread into another project, continuing in `cwd` (the project root when
    /// absent; a relative path is taken from the root, and the directory must lie inside
    /// it). The next turn starts a fresh provider session there and hands it the
    /// conversation so far. Refused for archived threads, drafts, native subagent mirrors,
    /// threads with a dedicated worktree and busy threads.
    MoveSession {
        session_id: String,
        project_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cwd: Option<PathBuf>,
    },
    DeleteSession {
        session_id: String,
        remove_worktree: bool,
    },
    MergeWorktree {
        session_id: String,
    },
    DeleteProject {
        project_id: String,
    },
    MarkSessionUnread {
        session_id: String,
    },
    /// The client has loaded the thread's conversation and shown it as of
    /// the thread's `updated_at` value `through`. A subscription alone never
    /// marks a thread read: it may arrive long after the user gave up on a
    /// view that never loaded.
    MarkSessionRead {
        session_id: String,
        through: u64,
    },
    StartDraft {
        project_id: String,
        cwd: PathBuf,
    },
    StartScratchDraft,
    SetDraftWorkspace {
        session_id: String,
        mode: WorkspaceMode,
    },

    SendTurn {
        session_id: String,
        text: String,
        attachment_paths: Vec<PathBuf>,
    },
    /// Keep a user-authored turn in the session's in-memory queue until the
    /// given Unix timestamp. Scheduled turns deliberately share the ordinary
    /// queue and are not persisted as conversation events before delivery.
    ScheduleTurn {
        session_id: String,
        text: String,
        attachment_paths: Vec<PathBuf>,
        fire_at_unix_secs: u64,
    },
    ConfirmRelayAndSend {
        session_id: String,
        text: String,
        attachment_paths: Vec<PathBuf>,
    },
    Steer {
        session_id: String,
        text: String,
        attachment_paths: Vec<PathBuf>,
    },
    SteerQueued {
        session_id: String,
        id: u64,
    },
    DropQueued {
        session_id: String,
        id: u64,
    },
    /// Put queued message `id` just before `before`, or last when `None`.
    MoveQueued {
        session_id: String,
        id: u64,
        before: Option<u64>,
    },
    RetryTurn {
        session_id: String,
        turn_id: String,
    },
    Interrupt {
        session_id: String,
    },
    /// Ask the provider to compact the session's context now; only offered
    /// where the provider has a native operation (`Caps::manual_compaction`).
    CompactContext {
        session_id: String,
    },
    /// Set, replace, pause or resume the harness's standing goal; only offered
    /// where the provider has one (`Caps::goals`). `None` keeps the value.
    SetGoal {
        session_id: String,
        objective: Option<String>,
        status: Option<agent::GoalStatus>,
    },
    ClearGoal {
        session_id: String,
    },
    RespondApproval {
        session_id: String,
        request_id: String,
        decision: ApprovalDecision,
    },
    RespondUserInput {
        session_id: String,
        request_id: String,
        answers: serde_json::Map<String, serde_json::Value>,
    },
    SetActiveModel {
        session_id: String,
        provider: ProviderKind,
        model: Option<String>,
        profile_id: Option<String>,
    },
    SetActiveOption {
        session_id: String,
        id: String,
        value: Option<serde_json::Value>,
    },
    LoadBranches {
        session_id: String,
    },
    CheckoutBranch {
        session_id: String,
        branch: String,
    },
    ToggleFavoriteModel {
        model: String,
    },
    RewindTurn {
        session_id: String,
        turn: usize,
        mode: RewindMode,
    },
}

/// Correlated result of a [`Command`].
///
/// Most mutations return [`CommandResponse::Unit`]. Keeping the few
/// result-bearing operations on the command plane avoids disguising mutations
/// as queries.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "content", rename_all = "snake_case")]
pub enum CommandResponse {
    Unit,
    PullRequestLinked {
        key: tcode_core::pull_request::PullRequestKey,
        already_linked: bool,
    },
    PullRequestAction(crate::PullRequestActionResult),
    ProjectId(Option<String>),
    SessionId(Option<String>),
    PendingRelaunchSection {
        section: Option<String>,
        session_id: Option<String>,
    },
    ExternalImportStarted(bool),
    ChildrenSettled(SettleChildrenResult),
    /// `AddThreadReference`: `boundary_notice` is true when this call made a reference that
    /// crosses a project or provider boundary, which the client tells the user once.
    ThreadReferenceAdded {
        boundary_notice: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChildSettlement {
    pub thread_id: String,
    pub delivery: tcode_core::settlement::AgentDelivery,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SettleChildrenResult {
    pub settled: Vec<ChildSettlement>,
    pub skipped: Vec<ChildSettlement>,
}

impl Command {
    /// The thread whose state this command addresses, for delivery navigation.
    pub fn session_id(&self) -> Option<&str> {
        match self {
            Self::LinkPullRequest { session_id, .. }
            | Self::UnlinkPullRequest { session_id, .. }
            | Self::WatchPullRequest { session_id, .. }
            | Self::SetPullRequestWatchesPaused { session_id, .. }
            | Self::SetPullRequestFilesViewed { session_id, .. }
            | Self::RefreshPullRequest { session_id, .. }
            | Self::RunPullRequestAction { session_id, .. }
            | Self::EditPullRequestReviewDraft { session_id, .. }
            | Self::OrchestrateTurn { session_id, .. }
            | Self::RunGitAction { session_id, .. }
            | Self::SetActiveAcpAgent { session_id, .. }
            | Self::SetTerminalHeight { session_id, .. }
            | Self::ToggleTerminalPanel { session_id, .. }
            | Self::CloseTerminalPanel { session_id, .. }
            | Self::RestartTerminal { session_id, .. }
            | Self::NewTerminal { session_id, .. }
            | Self::SplitTerminal { session_id, .. }
            | Self::ActivateTerminal { session_id, .. }
            | Self::CloseTerminal { session_id, .. }
            | Self::CaptureTerminalSelection { session_id, .. }
            | Self::RemoveTerminalContext { session_id, .. }
            | Self::AddReviewComment { session_id, .. }
            | Self::RemoveReviewComment { session_id, .. }
            | Self::SettleChildren { session_id }
            | Self::SettleSession { session_id, .. }
            | Self::SetControlGrant { session_id, .. }
            | Self::RespondControlGrant { session_id, .. }
            | Self::AddThreadReference { session_id, .. }
            | Self::RemoveThreadReference { session_id, .. }
            | Self::SetFollowUp { session_id, .. }
            | Self::SetParked { session_id, .. }
            | Self::DismissSettleSuggestion { session_id }
            | Self::RevokeSelfSettle { session_id }
            | Self::CancelAgent { session_id }
            | Self::UnsettleSession { session_id, .. }
            | Self::PinSession { session_id, .. }
            | Self::UnpinSession { session_id }
            | Self::ReorderPinned { session_id, .. }
            | Self::ReorderActive { session_id, .. }
            | Self::SetAutoSettle { session_id, .. }
            | Self::ArchiveSession { session_id, .. }
            | Self::MoveSession { session_id, .. }
            | Self::UnarchiveSession { session_id, .. }
            | Self::RenameSession { session_id, .. }
            | Self::RegenerateSessionTitle { session_id }
            | Self::DeleteSession { session_id, .. }
            | Self::MergeWorktree { session_id, .. }
            | Self::MarkSessionUnread { session_id, .. }
            | Self::MarkSessionRead { session_id, .. }
            | Self::SetDraftWorkspace { session_id, .. }
            | Self::SendTurn { session_id, .. }
            | Self::ScheduleTurn { session_id, .. }
            | Self::ConfirmRelayAndSend { session_id, .. }
            | Self::Steer { session_id, .. }
            | Self::SteerQueued { session_id, .. }
            | Self::DropQueued { session_id, .. }
            | Self::MoveQueued { session_id, .. }
            | Self::RetryTurn { session_id, .. }
            | Self::Interrupt { session_id, .. }
            | Self::CompactContext { session_id, .. }
            | Self::SetGoal { session_id, .. }
            | Self::ClearGoal { session_id, .. }
            | Self::RespondApproval { session_id, .. }
            | Self::RespondUserInput { session_id, .. }
            | Self::SetActiveModel { session_id, .. }
            | Self::SetActiveOption { session_id, .. }
            | Self::LoadBranches { session_id, .. }
            | Self::CheckoutBranch { session_id, .. }
            | Self::RewindTurn { session_id, .. } => Some(session_id),
            Self::ForkThread { id } => Some(id),
            _ => None,
        }
    }

    /// Secret-bearing payloads may be retained in memory, never on disk.
    pub fn contains_secret(&self) -> bool {
        matches!(
            self,
            Self::SetHostToken { .. }
                | Self::SetProfileSecret { .. }
                | Self::CreateThirdPartyProfile { .. }
        )
    }

    /// Idempotent controls and reads do not need retained delivery, and a
    /// pull request action must not have it. All other variants are retained
    /// writes, including settings assignments: repeating an old assignment
    /// after a newer one would undo user intent.
    pub fn requires_delivery_key(&self) -> bool {
        !matches!(
            self,
            Self::ResizeTerminal { .. }
                | Self::PreviewReply { .. }
                | Self::ShutdownAllAndFlush
                | Self::OpenLatestSession
                | Self::RefreshHostCredentials
                | Self::RunPullRequestAction { .. }
                | Self::RefreshProviderStatus
                | Self::RefreshProviderUsage
                | Self::CheckProviderVersions
                | Self::RefreshAcpRegistry
                | Self::RefreshProviderPlugins { .. }
                | Self::LoadBranches { .. }
        )
    }
}
