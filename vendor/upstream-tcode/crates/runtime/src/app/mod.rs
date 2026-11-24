//! Application state: session registry, active session runtime, event pump.

use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};
use std::fs;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use agent::{
    AgentError, AgentEvent, ApplyTiming, ApprovalDecision, Attachment, CatalogRefresh, ItemContent,
    ItemStatus, LaunchEnv, ModelSpec, OptionDescriptor, OptionDescriptors, OptionSelection,
    ProviderCommand, ProviderCommandKind, ProviderKind, RewindMode, SessionCommand, SessionHandle,
    SessionOptions, ThreadItem, TurnOptions, TurnStatus, list_models, permission_control,
};
use base64::Engine as _;
use serde::{Deserialize, Serialize};

use crate::host::{HostCx, HostEvent, HostTask};
use crate::terminal::{
    TerminalContext, TerminalProjection, TerminalRegistry, TerminalSplit, TerminalWorkspace,
};
use tcode_core::acp::{AcpAgentPatch, InstalledAcpAgent as InstalledAgent};
use tcode_core::attachments::mime_from_path;
use tcode_core::git::{GitAction, GitStatus, build_commit_prompt, sanitize_commit_message};
use tcode_core::project::{
    Project, SessionMeta, SettledOverride, WorktreeInfo, WorktreeSharing, descendant_session_ids,
    project_name_from_root, rebase_path,
};
use tcode_core::provider_status::ProviderSnapshot;
use tcode_core::relay::{
    RELAY_TRANSCRIPT_MAX_CHARS, assemble_relay_prompt, has_meaningful_history,
    render_relay_transcript,
};
use tcode_core::session::{
    Author, EntryContent, MessageOrigin, ReviewComment, Timeline, append_review_comments_to_prompt,
};
use tcode_core::settings::{
    ChildApprovalMode, EnvVar, OrchestrateSettings, ProfileSettingsPatch, ProviderProfile,
    ProviderSettings, ResolvedProfile, Settings,
};
use tcode_core::ui::{
    ConversationDestination, MAX_TERMINALS_PER_SESSION, TerminalSplitDirection, WorkspaceMode,
};
use tcode_protocol::{
    AcpMarketplaceItem, ArchivedSessions, EventEnvelope, ExternalImportState, ExternalImportStatus,
    ExternalThread, ForkAvailability, GitActionRequest, GitStatusStatus, IndexSnapshot,
    IndexSummary, MergeWorktreeFailure, PathEntry, ProtocolError,
    ProviderVersionStatus as ProtocolProviderVersionStatus, ProvidersStatus, QueryResponse,
    QueuedMessageStatus, RecentDir, RuntimeEffect, RuntimeError, RuntimeNotice,
    RuntimeNotification as RuntimeEvent, RuntimeOperationId, RuntimeToast, ServerEvent,
    SessionActivity, SessionEventRecord, SessionPlan, SessionSearchHit, SessionStatus,
    TcodeUpdateStatus, TerminalStatus, ThreadExportFormat, Topic,
};
use tcode_services::acp_registry::{
    Registry, RegistryAgent, cached, install, load, platform_key, resolve_recipe, uninstall,
    visible_agents,
};
use tcode_services::export;
#[cfg(test)]
use tcode_services::git::run_git;
use tcode_services::git::{
    CheckoutError, checkout_if_clean, commit_diff_context, list_git_branches, perform_action,
    read_git_branch, read_status, run_claude_headless,
};
use tcode_services::import::{
    ExternalRoots, ImportOutcome, existing_external_ids, import_thread, scan_recent_dirs,
};
use tcode_services::provider_probe::{default_program, probe_provider, run_capture_env};
use tcode_services::session_search::SessionSearch;
use tcode_services::settings::SettingsStore;
use tcode_services::store::{CommandsCacheKey, SessionStore, now_millis, now_secs};
use tcode_services::user_files;
use tcode_services::version_check::provider_updates::{
    self, CheckInput as ProviderCheckInput, Installation,
};
use tcode_services::version_check::{self as app_releases, ReleaseFeed};
use tcode_services::workspace::list_workspace;
use tcode_services::worktree::{
    MergeBackError, MergeBackOutcome, ProvisionError, cleanup_orphans, merge_back, provision,
    remove as remove_git_worktree,
};

const TITLE_MAX_CHARS: usize = 40;
const TITLE_SOURCE_MAX_CHARS: usize = 8_000;
const AI_TITLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(45);
/// Anthropic's prompt cache expires after one hour, so a provider kept longer
/// cannot preserve a useful cached conversation prefix.
const RESIDENT_IDLE_GRACE: Duration = Duration::from_secs(60 * 60);
/// Runaway backstop for unusually large resident fleets; the grace reaper is
/// the primary bound, while this comfortably preserves typical orchestrate
/// fleets whose idle children may be re-messaged.
const MAX_IDLE_RESIDENTS: usize = 16;
const MAX_IDLE_REPLAY_RESIDENTS: usize = 4;
const MAX_IDLE_REPLAY_BYTES: usize = 32 * 1024 * 1024;

type ProviderLaunchFuture =
    Pin<Box<dyn Future<Output = Result<SessionHandle, AgentError>> + Send + 'static>>;

/// Internal seam for starting a provider adapter.
///
/// Production uses [`agent::start_session`]; runtime tests install a scripted
/// adapter while exercising the same command and event paths.
#[derive(Clone)]
pub struct ProviderLauncher(
    Arc<dyn Fn(ProviderKind, SessionOptions) -> ProviderLaunchFuture + Send + Sync>,
);

impl ProviderLauncher {
    fn launch(&self, provider: ProviderKind, options: SessionOptions) -> ProviderLaunchFuture {
        (self.0)(provider, options)
    }
}

impl Default for ProviderLauncher {
    fn default() -> Self {
        Self(Arc::new(|provider, options| {
            Box::pin(agent::start_session(provider, options))
        }))
    }
}

/// Test-controlled provider adapter paired with its launcher.
#[cfg(any(test, feature = "test-support"))]
pub struct ScriptedProvider {
    pub launcher: ProviderLauncher,
    pub commands: smol::channel::Receiver<SessionCommand>,
    pub events: smol::channel::Sender<AgentEvent>,
}

/// Build a provider launcher whose command and event channels are owned by the test.
#[cfg(any(test, feature = "test-support"))]
pub fn scripted_provider(provider: ProviderKind) -> ScriptedProvider {
    let (commands_tx, commands) = smol::channel::unbounded();
    let (events, events_rx) = smol::channel::unbounded();
    let launcher = ProviderLauncher(Arc::new(move |requested, _options| {
        let commands = commands_tx.clone();
        let events = events_rx.clone();
        Box::pin(async move {
            if requested != provider {
                return Err(AgentError::Protocol(format!(
                    "scripted provider expected {provider:?}, got {requested:?}"
                )));
            }
            Ok(SessionHandle {
                provider,
                commands,
                events,
            })
        })
    }));
    ScriptedProvider {
        launcher,
        commands,
        events,
    }
}

fn normalize_terminal_context_text(text: &str) -> String {
    text.replace("\r\n", "\n").trim_matches('\n').to_string()
}

fn append_terminal_contexts_to_prompt(prompt: &str, contexts: &[TerminalContext]) -> String {
    let prompt = prompt.trim();
    let mut lines = Vec::new();
    for context in contexts {
        let text = normalize_terminal_context_text(&context.text);
        if text.is_empty() || context.terminal_label.trim().is_empty() {
            continue;
        }
        let range = if context.line_start == context.line_end {
            format!("line {}", context.line_start)
        } else {
            format!("lines {}-{}", context.line_start, context.line_end)
        };
        if !lines.is_empty() {
            lines.push(String::new());
        }
        lines.push(format!("- {} {}:", context.terminal_label.trim(), range));
        lines.extend(
            text.lines()
                .enumerate()
                .map(|(index, line)| format!("  {} | {}", context.line_start + index, line)),
        );
    }
    if lines.is_empty() {
        return prompt.to_string();
    }
    let block = format!(
        "<terminal_context>\n{}\n</terminal_context>",
        lines.join("\n")
    );
    if prompt.is_empty() {
        block
    } else {
        format!("{prompt}\n\n{block}")
    }
}

#[derive(Debug, Clone, Copy)]
enum TimelineLoadTarget {
    Active { mark_idle: bool },
    Background,
}

/// One request to rebuild a resident session's timeline from its log.
#[derive(Debug, Clone, Copy)]
struct TimelineLoad {
    generation: u64,
    target: TimelineLoadTarget,
}

impl TimelineLoad {
    fn mark_idle(&self) -> bool {
        match self.target {
            TimelineLoadTarget::Active { mark_idle } => mark_idle,
            TimelineLoadTarget::Background => true,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
struct TerminalPreferences {
    open: bool,
    height: f32,
    count: usize,
}

#[derive(Debug, Clone, Copy)]
enum TerminalSpawnAction {
    Open,
    Restart {
        terminal_id: Option<u64>,
    },
    New,
    Split {
        first: u64,
        direction: TerminalSplitDirection,
    },
}

mod acp;
mod active_session;
mod approvals;
mod authorization;
mod command_output;
mod command_validation;
mod control;
mod diff_pass;
mod events;
mod git;
mod history;
mod original;
use history::{Hydration, SessionLog};
mod lifecycle;
mod options;
mod orchestrate;
mod plugins;
mod providers;
mod pull_request_stacks;
mod pull_request_watch;
mod pull_requests;
mod revisioned;
mod send;
mod session_index;
mod sessions;
mod settlement;
mod snapshots;
mod source_control;
mod store_write;
mod subagents;
mod terminals;

#[cfg(test)]
mod test_support;
#[cfg(test)]
mod tests;

pub use active_session::{ActiveSession, QueuedMessage};
use active_session::{
    PendingRelay, PendingSteer, Runtime, SendRouting, attachment_paths, conversation_destination,
    wire_text_with_placeholder,
};
use orchestrate::McpWiring;
pub use providers::ProviderCatalog;
use providers::{
    McpServers, computer_use_attaches, effort_selection, normalized_selections,
    provider_secret_names, session_options,
};
pub use revisioned::Revisioned;
pub use session_index::SessionIndex;
pub use sessions::ResidentSessions;
pub(crate) use snapshots::DomainDiff;
use store_write::{StoreWrite, StoreWriteFailure, StoreWriter};

/// The result of a provider version check.
#[derive(Debug, Clone, Default)]
pub struct ProviderVersionState {
    /// Rejects checks completed after a configuration change or update start.
    revision: u64,
    /// Installed version (raw string, e.g. `"2.1.206"`); `None` if `--version` failed.
    pub installed: Option<String>,
    /// Latest version offered by this installation's channel and constraints.
    pub latest: Option<String>,
    /// Whether `latest` is strictly newer than `installed`.
    pub update_available: bool,
    /// Whether a version check is currently running.
    pub checking: bool,
    /// Whether an installation update is currently running.
    pub updating: bool,
    /// Verified installation and the exact update plan presented to the user.
    pub installation: Option<Installation>,
}

/// The result of checking the running tcode build against GitHub Releases.
#[derive(Debug, Clone)]
pub struct TcodeUpdateState {
    pub current: String,
    pub latest: Option<String>,
    pub release_url: Option<String>,
    pub update_available: bool,
    pub checking: bool,
}

impl Default for TcodeUpdateState {
    fn default() -> Self {
        Self {
            current: option_env!("TCODE_BUILD_VERSION")
                .unwrap_or(env!("CARGO_PKG_VERSION"))
                .to_string(),
            latest: None,
            release_url: None,
            update_available: false,
            checking: false,
        }
    }
}

pub struct AppState {
    store: SessionStore,
    user_directories: user_files::UserDirectories,
    settings_store: SettingsStore,
    forge: Arc<dyn tcode_services::forge::Forge>,
    releases: Arc<ReleaseFeed>,
    host_generation: u64,
    pull_requests: pull_requests::PullRequestRuntime,
    pull_request_watches: pull_request_watch::WatchRuntime,
    store_writes: smol::channel::Sender<StoreWrite>,
    store_write_receiver: Option<smol::channel::Receiver<StoreWrite>>,
    store_write_failures: smol::channel::Sender<StoreWriteFailure>,
    store_write_failure_receiver: Option<smol::channel::Receiver<StoreWriteFailure>>,
    /// The store-writer thread, joined by [`AppState::close_store`].
    store_writer: Option<std::thread::JoinHandle<()>>,
    /// Set once the store failed and every thread was stopped.
    store_failed: bool,
    pub sessions: SessionIndex,
    archived_revision: u64,
    activity_pending: VecDeque<String>,
    activity_priority: VecDeque<String>,
    activity_queued: HashSet<String>,
    activity_in_flight: HashSet<String>,
    activity_deferred: HashSet<String>,
    activity_backfill_started: Option<Instant>,
    settlement_deadlines: HashMap<String, u64>,
    settlement_timer: Option<(u64, HostTask<()>)>,
    decision_revisions: HashMap<String, u64>,
    thread_activity: Revisioned<HashMap<String, tcode_core::settlement::ThreadActivity>>,
    space_scopes: HashMap<String, BTreeSet<String>>,
    space_policy_revisions: HashMap<String, u64>,
    space_archives_revision: Option<u64>,
    space_archives: HashMap<String, ArchivedSessions>,
    // Archive replies remain ordered after the last subscriber releases a projection.
    space_archive_revisions: HashMap<String, u64>,
    decision_authors: HashMap<(String, String), Author>,
    pub projects: Vec<Project>,
    pub residents: ResidentSessions,
    /// Terminal resources parked by conversation destination. Drawer chrome is
    /// client-owned; this map retains only PTYs, tabs, splits, and contexts.
    terminal_workspaces: HashMap<ConversationDestination, TerminalWorkspace>,
    /// Host-private index from terminal id to its PTY. Clients receive the
    /// replicated grid instead; nothing here crosses the pipe.
    terminal_registry: TerminalRegistry,
    /// The replicated grid published on `Topic::Terminal`, one per live PTY.
    terminal_projections: HashMap<u64, TerminalProjection>,
    /// The command output screens published on `Topic::CommandOutput`, one
    /// per subscribed width.
    command_output_feeds: HashMap<Topic, command_output::CommandOutputFeed>,
    next_command_output_feed: u64,
    preview_pending: HashMap<u64, async_channel::Sender<Result<preview_mcp::PreviewReply, String>>>,
    next_preview_request: u64,
    /// Provider-native rewind requested while a session is live or starting.
    /// Kept here (rather than in persisted session metadata) because the
    /// provider response is the only authority that can complete it.
    pending_native_rewinds: HashMap<String, (String, RewindMode)>,
    /// Provider-native subagent item ids mapped to their read-only mirror sessions.
    native_subagent_sessions: HashMap<(String, String), String>,
    /// Synthetic turn state survives eviction; false remembers a finished child.
    native_subagent_turns: HashMap<String, bool>,
    /// Subagent items recorded inside mirrors, by (session, item id): the
    /// spawns a nested mirror is created from once a grandchild item arrives.
    nested_subagent_spawns: HashMap<(String, String), subagents::NestedSpawn>,
    pub settings: Settings,
    pub providers: ProviderCatalog,
    terminal_preferences_path: PathBuf,
    terminal_preferences: HashMap<String, TerminalPreferences>,
    next_terminal_spawn_id: u64,
    pending_terminal_spawns: HashMap<String, HashMap<u64, TerminalSpawnAction>>,
    next_start_generation: u64,
    /// Invalidates detached scheduled-wake tasks whenever the earliest deadline
    /// changes; stale timers must never fire or reschedule superseded work.
    scheduler_generation: u64,
    resident_idle_grace: Duration,
    /// Kept off in unit tests so dispatching a synthetic turn never launches a
    /// real provider process. Production titles are generated in the background.
    ai_title_generation_enabled: bool,
    title_generating: Revisioned<HashSet<String>>,
    provider_launcher: ProviderLauncher,
    /// The ACP agent marketplace: the registry index (from the CDN, cached on
    /// disk with a one-hour TTL), whether a refresh is in flight, and the last
    /// failure to show when there is nothing cached to fall back on.
    pub acp_registry: Option<Registry>,
    pub acp_registry_loading: bool,
    pub acp_registry_error: Option<String>,
    /// Registry ids currently downloading (their marketplace row shows a spinner).
    pub acp_installing: std::collections::HashSet<String>,
    /// Native plugin catalogs by profile id, listed on demand.
    plugin_catalogs: HashMap<String, plugins::PluginCatalog>,
    mcp: McpWiring,
    control: control::ControlState,
    callback_approval_requests: HashSet<(String, String)>,
    callback_user_input_requests: HashSet<(String, String)>,
    /// Live provider approvals for every resident session. This is the sole
    /// host-side authority; persisted timeline approvals remain client state.
    approvals: Revisioned<HashMap<String, Vec<agent::ApprovalRequest>>>,
    /// Background-computed git state of the active session's cwd, driving the
    /// adaptive header quick-action button (`None` until the first refresh /
    /// with no active session). See [`AppState::refresh_git_status`].
    pub git_status: HashMap<String, GitStatus>,
    /// A git quick-action (commit/push/pull/…) is currently running, so the
    /// button is disabled with an in-progress hint.
    pub git_busy: HashSet<String>,
    /// Source of ids used to correlate semantic operation lifecycle events.
    next_operation_id: u64,
    /// Monotonic token so a stale background status refresh (from a session the
    /// user has since switched away from) is ignored.
    git_status_generation: HashMap<String, u64>,
    /// Per-session token used to discard superseded timeline loads.
    timeline_load_generations: HashMap<String, u64>,
    subscriptions: HashSet<Topic>,
    /// Resident sessions' event logs; see [`SessionLog`] for what is cached
    /// and when it is dropped.
    event_records: HashMap<String, SessionLog>,
    /// Logs being read off the mailbox, by session; never one that is in
    /// `event_records`.
    log_hydrations: HashMap<String, Hydration>,
    diff_pass: Option<diff_pass::DiffPassRun>,
    /// Composer-draft review notes, keyed by session id (in-memory only).
    review_comment_drafts: HashMap<String, Vec<ReviewComment>>,
    /// A restart-continuity marker taken at launch (see `tcode_services::relaunch`).
    /// Present only after an app-relaunch triggered by a permission grant; applied
    /// once by [`AppState::apply_pending_relaunch`] and then cleared.
    pending_relaunch: Option<tcode_services::relaunch::RelaunchMarker>,
    /// Latest external-import run per project. Only the current/latest run is
    /// retained, so this is a replicated status rather than a job log.
    external_imports: HashMap<String, ExternalImportStatus>,
    next_import_run_id: u64,
    /// Host-owned content index over this host's own session store. Its cache
    /// lock is only ever taken on the blocking executor, never on the mailbox.
    session_search: Arc<SessionSearch>,
}

fn emit_runtime(cx: &mut HostCx, event: RuntimeEvent) {
    cx.emit(HostEvent::Runtime(event));
}

fn permission_relaunch_marker(
    marker: Option<tcode_services::relaunch::RelaunchMarker>,
    permissions: computer_use_mcp::permissions::PermissionStatus,
) -> Option<tcode_services::relaunch::RelaunchMarker> {
    marker.filter(|marker| marker.reopen_settings != "computer_use" || permissions.screen_recording)
}

impl AppState {
    pub fn new(store: SessionStore) -> std::io::Result<Self> {
        Self::with_ai_titles(store, false, user_files::UserDirectories::default())
    }

    /// Take ownership of the data dir and load its threads. Fails while
    /// another host owns the directory, or when its database cannot be opened.
    pub(crate) fn with_ai_titles(
        store: SessionStore,
        ai_title_generation_enabled: bool,
        user_directories: user_files::UserDirectories,
    ) -> std::io::Result<Self> {
        store.open()?;
        let file = store.read_file()?;
        let mut sessions = file.sessions;
        sessions.sort_by_key(|b| std::cmp::Reverse(b.updated_at));
        let mut projects = file.projects;
        let scratch_root = user_directories.scratch_root().ok();
        for project in &mut projects {
            project.quick_chat = scratch_root.as_ref() == Some(&project.root);
        }
        let settings_store = SettingsStore::new(store.root().clone());
        let settings = settings_store.load();
        let review_records = settings_store.load_review_records();
        let control_activity = settings_store.load_control_activity();
        let forge = tcode_services::forge::connect(settings_store.clone(), std::env::vars());
        forge.configure(settings.source_control.hosts.clone());
        let releases = Arc::new(ReleaseFeed::new(tcode_services::github::GitHubApi::host(
            tcode_services::github::Credentials::new(settings_store.clone(), []),
        )));
        let provider_secret_names = provider_secret_names(&settings, &settings_store);
        // Push the loaded computer-use config to the (already-running) MCP layer
        // so the tools honor the persisted image-mode / allow-input choices from
        // the first call, not just after a settings change.
        computer_use_mcp::config::set(settings.computer_use.clone());
        // Consume any restart-continuity marker left by a permission grant.
        // A denied Screen Recording flow must not reopen Settings on a later,
        // unrelated launch even if the foreground cleanup never ran.
        let pending_relaunch = permission_relaunch_marker(
            tcode_services::relaunch::take(store.root()),
            computer_use_mcp::permissions::check(),
        );
        let terminal_preferences_path = store.root().join("terminal-ui.json");
        let terminal_preferences = std::fs::read(&terminal_preferences_path)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default();
        // Seed the model picker from the persisted cache so it is instant and
        // works offline; a background refresh (see `refresh_model_catalogs`)
        // updates it once the providers respond.
        let mut model_catalogs = HashMap::new();
        for provider in ProviderKind::NATIVE {
            let cached = store.load_models(provider);
            if !cached.is_empty() {
                model_catalogs.insert(provider, cached);
            }
        }
        log::info!(
            "loaded {} stored session(s) in {} project(s) from {}",
            sessions.len(),
            projects.len(),
            store.root().display()
        );
        let thread_activity = store.load_thread_activity()?;
        let missing_turn_indices: HashSet<_> =
            store.threads_without_turn_index()?.into_iter().collect();
        let activity_pending: VecDeque<_> = sessions
            .iter()
            .filter(|meta| {
                !thread_activity.contains_key(&meta.id) || missing_turn_indices.contains(&meta.id)
            })
            .map(|meta| meta.id.clone())
            .collect();
        log::info!(
            "thread activity: {} rows loaded, {} threads to backfill activity or turn index",
            thread_activity.len(),
            activity_pending.len()
        );
        let (store_writes, store_write_receiver) = smol::channel::unbounded();
        let (store_write_failures, store_write_failure_receiver) = smol::channel::unbounded();
        let session_search = Arc::new(SessionSearch::new(store.clone()));
        Ok(Self {
            store,
            user_directories,
            settings_store,
            forge: forge.clone(),
            releases,
            host_generation: 0,
            pull_requests: pull_requests::PullRequestRuntime::new(forge.clone()),
            pull_request_watches: pull_request_watch::WatchRuntime::new(forge),
            store_writes,
            store_write_receiver: Some(store_write_receiver),
            store_write_failures,
            store_write_failure_receiver: Some(store_write_failure_receiver),
            store_writer: None,
            store_failed: false,
            sessions: sessions.into(),
            projects,
            archived_revision: 0,
            activity_backfill_started: (!activity_pending.is_empty()).then(Instant::now),
            activity_queued: activity_pending.iter().cloned().collect(),
            activity_pending,
            activity_priority: VecDeque::new(),
            activity_in_flight: HashSet::new(),
            activity_deferred: HashSet::new(),
            settlement_deadlines: HashMap::new(),
            settlement_timer: None,
            decision_revisions: HashMap::new(),
            thread_activity: thread_activity.into(),
            space_scopes: HashMap::new(),
            space_policy_revisions: HashMap::new(),
            space_archives_revision: None,
            space_archives: HashMap::new(),
            space_archive_revisions: HashMap::new(),
            decision_authors: HashMap::new(),
            residents: ResidentSessions::default(),
            terminal_workspaces: HashMap::new(),
            terminal_registry: TerminalRegistry::default(),
            terminal_projections: HashMap::new(),
            command_output_feeds: HashMap::new(),
            next_command_output_feed: 0,
            preview_pending: HashMap::new(),
            next_preview_request: 0,
            pending_native_rewinds: HashMap::new(),
            native_subagent_sessions: HashMap::new(),
            native_subagent_turns: HashMap::new(),
            nested_subagent_spawns: HashMap::new(),
            settings,
            providers: ProviderCatalog::new(model_catalogs, provider_secret_names),
            terminal_preferences_path,
            terminal_preferences,
            next_terminal_spawn_id: 0,
            pending_terminal_spawns: HashMap::new(),
            next_start_generation: 0,
            scheduler_generation: 0,
            resident_idle_grace: RESIDENT_IDLE_GRACE,
            ai_title_generation_enabled,
            title_generating: Default::default(),
            provider_launcher: ProviderLauncher::default(),
            acp_registry: None,
            acp_registry_loading: false,
            acp_registry_error: None,
            acp_installing: std::collections::HashSet::new(),
            plugin_catalogs: HashMap::new(),
            mcp: McpWiring::default(),
            control: control::ControlState::new(review_records, control_activity),
            callback_approval_requests: HashSet::new(),
            callback_user_input_requests: HashSet::new(),
            approvals: Default::default(),
            git_status: HashMap::new(),
            git_busy: HashSet::new(),
            next_operation_id: 1,
            git_status_generation: HashMap::new(),
            timeline_load_generations: HashMap::new(),
            subscriptions: HashSet::new(),
            event_records: HashMap::new(),
            log_hydrations: HashMap::new(),
            diff_pass: None,
            review_comment_drafts: HashMap::new(),
            pending_relaunch,
            external_imports: HashMap::new(),
            next_import_run_id: 1,
            session_search,
        })
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn set_provider_launcher_for_test(&mut self, launcher: ProviderLauncher) {
        self.provider_launcher = launcher;
    }

    fn start_store_writer(&mut self, cx: &mut HostCx) {
        if let Some(writes) = self.store_write_receiver.take() {
            let writer = StoreWriter::new(
                self.store.clone(),
                self.settings_store.clone(),
                self.terminal_preferences_path.clone(),
                self.store_write_failures.clone(),
            );
            // Its own thread: every commit blocks on the database's fsync.
            match std::thread::Builder::new()
                .name("tcode-store-writer".into())
                .spawn(move || writer.run(writes))
            {
                Ok(handle) => self.store_writer = Some(handle),
                Err(error) => {
                    log::error!("could not start the session store writer: {error}");
                    self.store_writes.close();
                }
            }
        }
        if let Some(failures) = self.store_write_failure_receiver.take() {
            let host_cx = cx.clone();
            HostCx::spawn_detached(cx, async move {
                while let Ok(failure) = failures.recv().await {
                    host_cx.enqueue(move |state, cx| match failure {
                        StoreWriteFailure::Error(error) => state.report_error(error, cx),
                        StoreWriteFailure::Warning(message) => log::warn!("{message}"),
                        StoreWriteFailure::StoreFailed(reason) => state.store_failed(&reason, cx),
                    });
                }
            });
        }
    }

    fn enqueue_store_write(&mut self, write: StoreWrite, cx: &mut HostCx) {
        let activity_id = match &write {
            StoreWrite::UpsertMeta { meta, .. } if !self.thread_activity.contains_key(&meta.id) => {
                Some(meta.id.clone())
            }
            _ => None,
        };
        self.start_store_writer(cx);
        if let Err(rejected) = self.store_writes.try_send(write) {
            log::error!("session store writer stopped before accepting a write");
            if let Some(StoreWriteFailure::Error(error)) = rejected
                .into_inner()
                .reject("the session store writer has stopped")
            {
                self.report_error(error, cx);
            }
        }
        if let Some(id) = activity_id {
            self.queue_activity_load(&id, cx);
        }
    }

    /// The store stopped serving: stop every provider so nothing keeps
    /// producing events that can no longer be saved. The failed writes were
    /// reported with the reason.
    fn store_failed(&mut self, reason: &str, cx: &mut HostCx) {
        if std::mem::replace(&mut self.store_failed, true) {
            return;
        }
        log::error!("session store failed ({reason}); stopping every thread");
        self.shutdown_all(cx);
    }

    /// Drain the store writer, then checkpoint and close the database. Only
    /// after the host loop has stopped, so nothing enqueues behind it.
    pub(crate) fn close_store(&mut self) -> std::io::Result<()> {
        self.settlement_timer = None;
        self.store_writes.close();
        if let Some(writer) = self.store_writer.take()
            && writer.join().is_err()
        {
            log::error!("the session store writer panicked");
        }
        self.store.close()
    }

    fn enqueue_settings(&mut self, settings: &Settings, cx: &mut HostCx) {
        let mut settings = settings.clone();
        settings.source_control.status.clear();
        match serde_json::to_vec_pretty(&settings) {
            Ok(bytes) => self.enqueue_store_write(StoreWrite::WriteSettings(bytes), cx),
            Err(err) => self.report_error(
                RuntimeError::PersistSettings {
                    error: err.to_string(),
                },
                cx,
            ),
        }
    }

    fn persist_settings(&mut self, cx: &mut HostCx) {
        let settings = self.settings.clone();
        self.enqueue_settings(&settings, cx);
    }

    fn emit_domain(&self, topic: Topic, event: ServerEvent, cx: &mut HostCx) {
        cx.emit(HostEvent::Domain(EventEnvelope {
            request_id: None,
            topic,
            event,
        }));
    }
}
