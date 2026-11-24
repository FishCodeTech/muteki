use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::rc::Rc;

use gpui::{App, Context, Entity, EventEmitter, Subscription as GpuiSubscription, Task};
use tcode_client::{
    ConnectionState, HostLink,
    host::{ClientHost, ClientPreferences, LiveHost},
};
use tcode_core::{
    git::{GitFileEntry, MenuItem, QuickAction, menu_items, quick_action},
    project::{
        Project, ProjectGroup, SessionMeta, WorktreeInfo, descendant_session_ids, group_sessions,
    },
    provider_models::{ResolvedModel, picker_models, resolve_models},
    provider_status::ProviderSnapshot,
    session::{EntryContent, ReviewComment, StoredEvent, Timeline},
    settings::{
        BrowserSettings, ProjectSort, ProviderSettings, ResolvedProfile, Settings, SidebarLayout,
        ThemeMode,
    },
    ui::{ConversationDestination, RightTab},
};
use tcode_protocol::{AcpMarketplaceItem, RuntimeNotification as RuntimeEvent};
use tcode_protocol::{
    ArchivedSessions, Command, CommandResponse, EventEnvelope, ExternalImportStatus,
    ExternalThread, GitDiffResult, GitDiffScope, GitStatusStatus, IndexSummary, PathEntry,
    ProtocolError, ProviderVersionStatus, ProvidersStatus, Query, QueryResponse, RecentDir, Scope,
    ScopedProviderChoice, ServerEvent, SessionPlan, SessionSearchHit, SessionStatus, Subscription,
    Topic,
};
pub(crate) mod terminal;
pub(crate) use terminal::ClientTerminal;
use terminal::TerminalWorkspace;

use crate::conversation_ui::{ConversationUiState, DiffFocus};

mod command_output;
mod history;
pub(crate) use history::HISTORY_WINDOW_SCREENS;
pub(crate) mod images;
mod intents;
pub(crate) use images::{
    MAX_PREVIEW_PIXELS, MediaState, image_preview, item_image_preview, pull_request_media,
    pull_request_media_state, retry_failed_pull_request_media,
};
mod snapshots;

pub(crate) use snapshots::PanelState;
pub use snapshots::{ComposerQueue, ComposerState};
pub use tcode_protocol::ForkAvailability;

/// Payload-free topic discriminant used by views to subscribe only to the
/// store projections they render.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TopicKind {
    SessionEvents,
    SessionStatus,
    SessionPlan,
    Index,
    SpaceIndex,
    Scope,
    Settings,
    Providers,
    GitStatus,
    RuntimeEvents,
    ActiveSession,
    Terminal,
    CommandOutput,
    Preview,
    ExternalImport,
}

impl From<&Topic> for TopicKind {
    fn from(topic: &Topic) -> Self {
        match topic {
            Topic::SessionEvents { .. } => Self::SessionEvents,
            Topic::SessionStatus { .. } => Self::SessionStatus,
            Topic::SessionPlan { .. } => Self::SessionPlan,
            Topic::SpaceIndex { .. } => Self::Index,
            Topic::Scope => Self::Index,
            Topic::Index => Self::Index,
            Topic::Settings => Self::Settings,
            Topic::Providers => Self::Providers,
            Topic::GitStatus { .. } => Self::GitStatus,
            Topic::RuntimeEvents => Self::RuntimeEvents,
            Topic::Terminal { .. } => Self::Terminal,
            Topic::CommandOutput { .. } => Self::CommandOutput,
            Topic::Preview => Self::Preview,
            Topic::ExternalImport { .. } => Self::ExternalImport,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StoreChange {
    pub topic: TopicKind,
}

/// Observe selected store domains while keeping topic filtering out of views.
pub(crate) fn observe_store_topics<V: 'static>(
    store: &Entity<WorkspaceStore>,
    topics: &'static [TopicKind],
    cx: &mut Context<V>,
) -> GpuiSubscription {
    cx.subscribe(store, move |_, _, change: &StoreChange, cx| {
        if topics.contains(&change.topic) {
            cx.notify();
        }
    })
}

/// Identity fixed for one workspace attachment.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WorkspaceAttachment {
    Local,
    Remote { host_id: String, host_name: String },
}

/// Where a remote attachment's Preview goes: the paired machine and the
/// tunnels the attachment's transport opens to it.
#[cfg(preview_backend)]
pub(crate) type PreviewTarget = (
    tcode_client::pairing::PairedHost,
    std::sync::Arc<dyn tcode_client::host::TunnelOpener>,
);

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum WorkspaceScope {
    #[default]
    Full,
    Space {
        space_id: String,
        space_name: String,
    },
}

impl WorkspaceScope {
    pub fn is_full(&self) -> bool {
        matches!(self, Self::Full)
    }
}

/// The client-facing projection and command boundary for workspace state.
///
/// Views observe this entity and use its typed accessors instead of retaining
/// or reading the backend `AppState` entity directly.
pub struct WorkspaceStore {
    host: HostLink,
    scope: WorkspaceScope,
    scoped_providers: Vec<ScopedProviderChoice>,
    attachment: WorkspaceAttachment,
    client_host: Option<Rc<dyn ClientHost>>,
    /// The transport's live view of the attached machine: its authenticated
    /// pairing and how the connection is carried. `None` locally and in a
    /// browser.
    #[cfg_attr(not(preview_backend), allow(dead_code))]
    current_host: Option<LiveHost>,
    client_preferences: ClientPreferences,
    image_namespace: u64,
    attachment_tasks: Vec<Task<()>>,
    /// Replicated terminal grids, keyed by the host's terminal id.
    terminals: HashMap<u64, std::rc::Rc<ClientTerminal>>,
    /// Command output screens of the selected thread, keyed by item id.
    command_outputs: HashMap<String, command_output::CommandOutputReplica>,
    /// Preview requests routed to this client. Every client owns the channel:
    /// one without a backend still has to answer `unsupported` rather than
    /// leave the agent's call hanging.
    remote_preview: (
        async_channel::Sender<EventEnvelope>,
        async_channel::Receiver<EventEnvelope>,
    ),
    /// Latest host-published import status per project, replicated from
    /// [`Topic::ExternalImport`]. The dialog renders this rather than owning a
    /// second events consumer.
    import_statuses: HashMap<String, Option<ExternalImportStatus>>,
    connection_state: ConnectionState,
    index_replica: (Vec<SessionMeta>, Vec<Project>),
    index_summary: IndexSummary,
    /// Archived threads, which the index leaves out; loaded while a view
    /// holds them.
    archived_replica: Option<ArchivedSessions>,
    archived_holds: usize,
    archived_task: Option<Task<()>>,
    /// The thread the index event being applied removed, so the destination
    /// can still follow it to its parent.
    removed_session: Option<SessionMeta>,
    settings_replica: Settings,
    /// Whether `settings_replica` is the host's settings or still the local
    /// defaults it was constructed with. Views that copy a setting into an
    /// editable input must not treat the defaults as the host's answer.
    settings_hydrated: bool,
    baseline_topics: HashSet<Topic>,
    applied_scope_request: Option<u64>,
    index_hydrated: bool,
    /// The `updated_at` this view last reported read, so an acknowledgement
    /// is sent once per change rather than once per event until the host's
    /// visit echo arrives.
    read_acknowledged: Option<(String, u64)>,
    /// Whether the shell shows the selected thread's conversation. A compact
    /// window keeps the thread selected on the thread list it returned to.
    conversation_on_screen: bool,
    selected_session_id: Option<String>,
    /// The selected thread's replicas, and those of the threads left most
    /// recently ([`KEPT_THREADS`]).
    threads: HashMap<String, ThreadReplica>,
    selection_generation: u64,
    session_turn_offset: usize,
    history_task: Option<Task<()>>,
    /// Drops the pages fetched above the tail once the reader has stayed
    /// there ([`history::HISTORY_TRIM_DELAY`]).
    history_trim: Option<Task<()>>,
    history_error: Option<String>,
    history_pages_fetched: usize,
    history_logged_records: Option<usize>,
    session_catching_up: bool,
    session_replica: Option<(String, Timeline)>,
    session_status_replica: Option<SessionStatus>,
    providers_replica: ProvidersStatus,
    git_status_replica: GitStatusStatus,
    active_destination: Option<ConversationDestination>,
    /// One-shot turn navigation requested by a content-search hit or an
    /// anchor link.
    pending_chat_turn: Option<(String, usize)>,
    /// The host resolving an anchor link's position to the turn it opens at.
    anchor_turn_task: Option<Task<()>>,
    native_rewind_prefills: HashMap<String, String>,
    /// Text a pull request action put into a thread's composer for the user to edit and send.
    composer_appends: HashMap<String, String>,
    fallback_blocks: HashMap<String, FallbackBlock>,
    fallback_reviews: HashMap<String, FallbackReview>,
    conversation_ui: HashMap<ConversationDestination, ConversationUiState>,
    /// The pull request each thread has open, in this run only.
    open_pull_requests: HashMap<String, tcode_core::pull_request::PullRequestKey>,
    /// A project-draft fallback is in flight, so the reconcile step does not
    /// ask for one more draft per index event while it resolves.
    draft_fallback_pending: bool,
    creation_request_generation: u64,
}

/// How many threads the user left keep their replicas besides the selected
/// one. Re-selecting a kept thread sends its cursor and gets only what it
/// missed; any other thread gets a baseline, as on first open. A thread
/// receives nothing while it is not selected, so keeping one longer saves no
/// more than that baseline: the bound is on how many are kept, not for how
/// long.
const KEPT_THREADS: usize = 4;

/// What the client holds of one thread.
#[derive(Default)]
struct ThreadReplica {
    /// Absent until the first window of the thread's log arrives.
    history: Option<history::HeldHistory>,
    /// Total turns at the last complete window, absent during catch-up.
    total_turns: Option<usize>,
    status: Option<SessionStatus>,
    plan: Option<SessionPlan>,
    git: Option<GitStatusStatus>,
    /// The `selection_generation` the user left the thread at.
    left_at: u64,
}

/// A turn stopped by Claude Code's safety classifier, kept per session so the
/// composer can offer recovery after the turn already ended.
#[derive(Debug, Clone)]
pub struct FallbackBlock {
    pub category: Option<agent::ClassifierCategory>,
    /// The model that refused (or was expected, on a silent reroute).
    pub model: Option<String>,
    /// The model Claude rerouted to; `None` when the request was blocked.
    pub fallback_model: Option<String>,
    pub detail: String,
}

/// A second model's read on a classifier stop: whether it looks like a false
/// positive, plus a clarification the user may review, edit and send. Both are
/// suggestions — nothing here is sent without a click.
#[derive(Debug, Clone)]
pub struct FallbackReview {
    pub assessment: String,
    /// Empty when the reviewer did not judge the flag a false positive.
    pub draft: String,
}

/// A rendered thread export as it arrives from the host: complete bytes, a file
/// name that is legal on any client OS, and the type to hand a download or
/// share sheet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThreadExportArtifact {
    pub bytes: Vec<u8>,
    pub suggested_name: String,
    pub mime: String,
}

/// Settings → Archived Threads → Delete all: the listed archived threads, the
/// threads under them that are not archived and go with them, and the threads
/// whose deletion removes exactly that set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArchivedDeletion {
    pub archived: usize,
    pub unarchived: usize,
    roots: Vec<String>,
}

pub(crate) struct DiffActiveState {
    pub session: String,
    pub cwd: PathBuf,
    pub branches: Vec<String>,
}

pub(crate) struct CommitDialogState {
    pub files: Vec<GitFileEntry>,
    pub branch: Option<String>,
    pub on_default_branch: bool,
}

fn protocol_io_error(message: impl Into<String>) -> std::io::Error {
    std::io::Error::other(message.into())
}

fn effective_client_settings(host: &Settings, preferences: &ClientPreferences) -> Settings {
    let mut settings = host.clone();
    settings.theme_mode = match preferences.appearance.as_deref() {
        Some("system") => ThemeMode::System,
        Some("light") => ThemeMode::Light,
        Some("dark") => ThemeMode::Dark,
        _ => settings.theme_mode,
    };
    settings.language = match preferences.language.as_deref() {
        Some("system") => None,
        Some(language) => Some(language.to_owned()),
        None => settings.language,
    };
    settings
}

impl WorkspaceStore {
    fn destination(status: &SessionStatus) -> ConversationDestination {
        if status.draft
            && let Some(project_id) = status.project_id.clone()
        {
            ConversationDestination::ProjectDraft(project_id)
        } else {
            ConversationDestination::Thread(status.session_id.clone())
        }
    }

    /// Construct the complete projection for exactly one client link.
    ///
    /// `seed_blocking` waits for Scope and its allowed domain snapshots. Only the desktop composition root asks for it: it
    /// applies the locale and theme from `settings()` the instant the store
    /// exists. Every other client renders immediately and re-renders when the
    /// snapshots land, which is the only option on a single-threaded executor.
    pub fn new_attached(
        host: HostLink,
        attachment: WorkspaceAttachment,
        client_host: Option<Rc<dyn ClientHost>>,
        current_host: Option<LiveHost>,
        seed_blocking: bool,
        cx: &mut Context<Self>,
    ) -> Self {
        static NEXT_IMAGE_NAMESPACE: std::sync::atomic::AtomicU64 =
            std::sync::atomic::AtomicU64::new(1);
        let image_namespace =
            NEXT_IMAGE_NAMESPACE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        cx.set_global(images::HostImages {
            link: Some(host.clone()),
            namespace: image_namespace,
        });
        let client_preferences = client_host
            .as_ref()
            .map(|host| host.load_preferences())
            .unwrap_or_default();
        let remote = matches!(attachment, WorkspaceAttachment::Remote { .. });
        let store = Self {
            host: host.clone(),
            scope: WorkspaceScope::Full,
            scoped_providers: Vec::new(),
            attachment,
            client_host,
            current_host,
            client_preferences,
            image_namespace,
            attachment_tasks: Vec::new(),
            terminals: HashMap::new(),
            command_outputs: HashMap::new(),
            remote_preview: async_channel::unbounded(),
            import_statuses: HashMap::new(),
            connection_state: if remote {
                host.connection_state()
            } else {
                ConnectionState::Connected { path: None }
            },
            index_replica: (Vec::new(), Vec::new()),
            index_summary: IndexSummary::default(),
            archived_replica: None,
            archived_holds: 0,
            archived_task: None,
            removed_session: None,
            settings_replica: Settings::default(),
            settings_hydrated: false,
            baseline_topics: HashSet::new(),
            applied_scope_request: None,
            index_hydrated: false,
            read_acknowledged: None,
            conversation_on_screen: false,
            selected_session_id: None,
            threads: HashMap::new(),
            selection_generation: 0,
            session_turn_offset: 0,
            history_task: None,
            history_trim: None,
            history_error: None,
            history_pages_fetched: 0,
            history_logged_records: None,
            session_catching_up: false,
            session_replica: None,
            session_status_replica: None,
            providers_replica: ProvidersStatus::default(),
            git_status_replica: GitStatusStatus::default(),
            active_destination: None,
            pending_chat_turn: None,
            anchor_turn_task: None,
            native_rewind_prefills: HashMap::new(),
            composer_appends: HashMap::new(),
            fallback_blocks: HashMap::new(),
            fallback_reviews: HashMap::new(),
            conversation_ui: HashMap::new(),
            open_pull_requests: HashMap::new(),
            draft_fallback_pending: false,
            creation_request_generation: 0,
        };
        let mut store = store;

        let _ = host.subscribe(Subscription {
            known_index: None,
            topic: Topic::Scope,
            after: None,
        });
        #[cfg(not(test))]
        let events = host.events();
        #[cfg(not(any(test, target_family = "wasm")))]
        if seed_blocking {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            while !store.seed_ready() && std::time::Instant::now() < deadline {
                match events.try_recv() {
                    Ok(envelope) => {
                        if let ServerEvent::Runtime(event) = &envelope.event {
                            cx.emit(event.clone());
                        } else {
                            store.apply_domain_event(&envelope, cx);
                        }
                    }
                    Err(async_channel::TryRecvError::Empty) => {
                        std::thread::sleep(std::time::Duration::from_millis(1));
                    }
                    Err(async_channel::TryRecvError::Closed) => break,
                }
            }
            if !store.seed_ready() {
                log::error!("host scope snapshot seeding timed out");
            }
        }
        #[cfg(any(test, target_family = "wasm"))]
        let _ = seed_blocking;

        #[cfg(not(test))]
        {
            let event_messages = events;
            store.attachment_tasks.push(cx.spawn(async move |this, cx| {
                while let Ok(envelope) = event_messages.recv().await {
                    if this
                        .update(cx, |store, cx| {
                            if let ServerEvent::Runtime(event) = &envelope.event {
                                cx.emit(event.clone());
                            } else {
                                store.apply_domain_event(&envelope, cx);
                                cx.emit(StoreChange {
                                    topic: TopicKind::from(&envelope.topic),
                                });
                            }
                            cx.notify();
                        })
                        .is_err()
                    {
                        break;
                    }
                }
            }));
        }

        #[cfg(not(test))]
        {
            let delivery_changes = host.delivery_changes();
            store.attachment_tasks.push(cx.spawn(async move |this, cx| {
                while delivery_changes.recv().await.is_ok() {
                    if this
                        .update(cx, |_, cx| {
                            cx.emit(StoreChange {
                                topic: TopicKind::SessionEvents,
                            });
                            cx.notify();
                        })
                        .is_err()
                    {
                        break;
                    }
                }
            }));
        }

        if remote {
            let changes = host.connection_state_changes();
            store.attachment_tasks.push(cx.spawn(async move |this, cx| {
                while let Ok(state) = changes.recv().await {
                    if this
                        .update(cx, |store, cx| {
                            cx.emit(state.clone());
                            store.apply_connection_state(state);
                            cx.emit(StoreChange {
                                topic: TopicKind::Index,
                            });
                            cx.emit(StoreChange {
                                topic: TopicKind::SessionStatus,
                            });
                            cx.notify();
                        })
                        .is_err()
                    {
                        break;
                    }
                }
            }));
        }

        store
    }

    /// Compatibility path for the mobile coordinator until it adopts the
    /// attachment identity constructor.
    pub fn attach_remote(&mut self, host_name: String, cx: &mut Context<Self>) {
        self.attachment = WorkspaceAttachment::Remote {
            host_id: String::new(),
            host_name,
        };
        self.connection_state = self.host.connection_state();
        let changes = self.host.connection_state_changes();
        self.attachment_tasks.push(cx.spawn(async move |this, cx| {
            while let Ok(state) = changes.recv().await {
                if this
                    .update(cx, |store, cx| {
                        cx.emit(state.clone());
                        store.apply_connection_state(state);
                        cx.notify();
                    })
                    .is_err()
                {
                    break;
                }
            }
        }));
    }

    pub(crate) fn preview_reply(
        &mut self,
        request_id: u64,
        response: Result<tcode_protocol::PreviewResponse, String>,
    ) {
        self.dispatch(tcode_protocol::Command::PreviewReply {
            request_id,
            response,
        });
    }

    pub(crate) fn remote_preview_requests(&self) -> async_channel::Receiver<EventEnvelope> {
        self.remote_preview.1.clone()
    }

    /// The paired machine Preview reaches and the tunnels that reach it.
    #[cfg(preview_backend)]
    pub(crate) fn preview_proxy(&self) -> Result<Option<PreviewTarget>, String> {
        if !self.is_remote() {
            return Ok(None);
        }
        self.current_host
            .as_ref()
            .filter(|live| Some(live.snapshot().host_id.as_str()) == self.remote_host_id())
            .and_then(|live| Some((live.snapshot(), live.tunnels()?)))
            .map(Some)
            .ok_or_else(|| "remote preview requires a paired machine connection".into())
    }

    pub(crate) fn can_manage_host(&self) -> bool {
        self.scope.is_full() && self.baseline_topics.contains(&Topic::Scope)
    }

    pub fn scope(&self) -> &WorkspaceScope {
        &self.scope
    }

    /// Whether this client shows the Preview tab at all: only where it can
    /// embed a browser. A browser tab already reaches the machine by address,
    /// so a dev server listening on the LAN opens in a new tab there instead.
    pub(crate) fn offers_preview(&self) -> bool {
        self.scope.is_full() && crate::preview_panel::preview_backend()
    }

    fn index_topic(&self) -> Topic {
        match &self.scope {
            WorkspaceScope::Full => Topic::Index,
            WorkspaceScope::Space { space_id, .. } => Topic::SpaceIndex {
                space_id: space_id.clone(),
            },
        }
    }

    #[cfg(not(any(test, target_family = "wasm")))]
    fn seed_ready(&self) -> bool {
        self.baseline_topics.contains(&Topic::Scope)
            && self.baseline_topics.contains(&self.index_topic())
            && (!self.scope.is_full()
                || (self.baseline_topics.contains(&Topic::Settings)
                    && self.baseline_topics.contains(&Topic::Providers)))
    }

    pub fn machine_label(&self) -> String {
        let machine = self.remote_host_name().unwrap_or_default();
        match &self.scope {
            WorkspaceScope::Full => machine.to_owned(),
            WorkspaceScope::Space { space_name, .. } => {
                crate::tr!("member.connection", space = space_name, machine = machine).into_owned()
            }
        }
    }

    pub fn space_label(&self) -> Option<String> {
        match &self.scope {
            WorkspaceScope::Full => None,
            WorkspaceScope::Space { space_name, .. } => {
                Some(crate::tr!("member.space", space = space_name).into_owned())
            }
        }
    }

    fn member_settings_key(&self) -> String {
        match &self.scope {
            WorkspaceScope::Space { space_id, .. } => {
                format!("{}:{space_id}", self.remote_host_id().unwrap_or_default())
            }
            WorkspaceScope::Full => String::new(),
        }
    }

    fn save_member_settings(&self) {
        if let Some(host) = &self.client_host {
            let mut preferences = host.load_preferences();
            let navigation = preferences
                .navigation
                .get_or_insert_with(|| serde_json::json!({}));
            navigation["member_settings"][self.member_settings_key()] =
                serde_json::to_value(&self.settings_replica).unwrap();
            host.save_preferences(&preferences);
        }
    }

    fn apply_scope(&mut self, scope: &Scope, cx: &mut Context<Self>) {
        let next = match scope {
            Scope::Full => WorkspaceScope::Full,
            Scope::Space {
                space_id,
                space_name,
                ..
            } => WorkspaceScope::Space {
                space_id: space_id.clone(),
                space_name: space_name.clone(),
            },
        };
        let changed = self.scope != next;
        let needs_baseline = changed || self.applied_scope_request.is_none();
        if changed {
            for subscription in self.host.subscriptions() {
                if subscription.topic != Topic::Scope {
                    let _ = self.host.unsubscribe(subscription);
                }
            }
            self.leave_session();
            self.threads.clear();
            self.conversation_ui.clear();
            self.index_replica = Default::default();
            self.index_summary = Default::default();
            self.draft_fallback_pending = false;
        }
        if needs_baseline {
            self.archived_replica = None;
            self.archived_task = None;
            self.baseline_topics.clear();
            self.index_hydrated = false;
            self.settings_hydrated = false;
            self.providers_replica = Default::default();
            self.scoped_providers.clear();
        }
        self.baseline_topics.insert(Topic::Scope);
        self.scope = next;
        match scope {
            Scope::Full if needs_baseline => {
                for topic in [
                    Topic::Settings,
                    Topic::Index,
                    Topic::Providers,
                    Topic::RuntimeEvents,
                ] {
                    let _ = self.host.subscribe(Subscription {
                        known_index: (topic == Topic::Index)
                            .then(|| self.cached_index_fingerprint())
                            .flatten(),
                        topic,
                        after: None,
                    });
                }
                // A client with no preview backend must not become a competing
                // owner of the preview: it would win requests it can only
                // refuse. It still answers `unsupported` for anything that
                // reaches it through an already-open subscription.
                if crate::preview_panel::preview_backend() {
                    let _ = self.host.subscribe(Subscription {
                        known_index: None,
                        topic: Topic::Preview,
                        after: None,
                    });
                }
                if let Some(id) = self.selected_session_id.take() {
                    self.select_session(id);
                }
            }
            Scope::Full => {}
            Scope::Space {
                projects,
                providers,
                ..
            } => {
                if changed {
                    self.settings_replica = self
                        .client_host
                        .as_ref()
                        .and_then(|host| host.load_preferences().navigation)
                        .and_then(|navigation| {
                            navigation
                                .get("member_settings")?
                                .get(self.member_settings_key())
                                .cloned()
                        })
                        .and_then(|settings| serde_json::from_value(settings).ok())
                        .unwrap_or_default();
                }
                self.settings_hydrated = true;
                self.index_replica.1 = projects.clone();
                self.scoped_providers = providers.clone();
                self.providers_replica = Default::default();
                for choice in providers {
                    self.providers_replica
                        .model_catalogs
                        .entry(choice.provider)
                        .or_default()
                        .extend(choice.models.clone());
                }
                if needs_baseline {
                    let _ = self.host.subscribe(Subscription {
                        known_index: self.cached_index_fingerprint(),
                        topic: self.index_topic(),
                        after: None,
                    });
                }
            }
        }
        for topic in [TopicKind::Settings, TopicKind::Providers, TopicKind::Index] {
            cx.emit(StoreChange { topic });
        }
    }

    pub fn is_remote(&self) -> bool {
        matches!(self.attachment, WorkspaceAttachment::Remote { .. })
    }

    pub fn remote_host_name(&self) -> Option<&str> {
        match &self.attachment {
            WorkspaceAttachment::Local => None,
            WorkspaceAttachment::Remote { host_name, .. } => Some(host_name),
        }
    }

    pub fn remote_host_id(&self) -> Option<&str> {
        match &self.attachment {
            WorkspaceAttachment::Local => None,
            WorkspaceAttachment::Remote { host_id, .. } => Some(host_id),
        }
    }

    /// Connected only once the baseline is in: the transport's `Connected`
    /// says the host answers, the replayed snapshots say the screen is current.
    /// Either way the path is the transport's.
    pub fn connection_state(&self) -> ConnectionState {
        match &self.connection_state {
            ConnectionState::Connected { path } if !self.baseline_ready() => {
                ConnectionState::Syncing { path: path.clone() }
            }
            ConnectionState::Syncing { path } if self.baseline_ready() => {
                ConnectionState::Connected { path: path.clone() }
            }
            state => state.clone(),
        }
    }

    /// A `Syncing` that only renames the path of a sync already under way
    /// keeps the baseline collected so far.
    fn apply_connection_state(&mut self, state: ConnectionState) {
        let restarts = match &state {
            ConnectionState::Syncing { .. } => {
                !matches!(self.connection_state, ConnectionState::Syncing { .. })
            }
            ConnectionState::Reconnecting { .. } | ConnectionState::Offline { .. } => true,
            ConnectionState::Connected { .. } => matches!(
                self.connection_state,
                ConnectionState::Reconnecting { .. } | ConnectionState::Offline { .. }
            ),
        };
        // State notifications and events use separate queues; a delayed disconnect
        // must not clear baselines already applied for the current Scope request.
        let scope_is_current = self.applied_scope_request.is_some()
            && self.applied_scope_request == self.host.subscription_request_id(&Topic::Scope);
        let disconnected = matches!(
            self.host.connection_state(),
            ConnectionState::Reconnecting { .. } | ConnectionState::Offline { .. }
        );
        if restarts && (!scope_is_current || disconnected) {
            self.baseline_topics.clear();
            self.archived_task = None;
        }
        self.connection_state = state;
    }

    /// Outbox-derived navigation only; no host metadata is invented or cached.
    pub(crate) fn pending_sessions(&self) -> Vec<(String, String)> {
        let mut sessions = Vec::new();
        for (_, command) in self.host.pending_commands() {
            let Some(id) = command.session_id().map(str::to_owned) else {
                continue;
            };
            let preview = match command {
                Command::SendTurn { text, .. }
                | Command::ScheduleTurn { text, .. }
                | Command::Steer { text, .. }
                | Command::ConfirmRelayAndSend { text, .. }
                | Command::OrchestrateTurn { text, .. } => text,
                _ => String::new(),
            };
            if !sessions.iter().any(|(existing, _)| existing == &id) {
                sessions.push((id, preview));
            }
        }
        sessions
    }

    pub(crate) fn delivery_messages(&self) -> Vec<(String, String, Option<String>, bool)> {
        let active = self.active_session_id().unwrap_or_default();
        let message = |command: &Command| match command {
            Command::SendTurn {
                session_id, text, ..
            }
            | Command::ScheduleTurn {
                session_id, text, ..
            }
            | Command::Steer {
                session_id, text, ..
            }
            | Command::ConfirmRelayAndSend {
                session_id, text, ..
            }
            | Command::OrchestrateTurn {
                session_id, text, ..
            } if session_id == &active => Some(text.clone()),
            _ => None,
        };
        self.host
            .pending_commands()
            .into_iter()
            .filter_map(|(key, command)| message(&command).map(|text| (key, text, None, false)))
            .chain(
                self.host
                    .failed_commands()
                    .into_iter()
                    .filter_map(|(entry, error)| {
                        message(&entry.command)
                            .map(|text| (entry.key, text, Some(error.message), false))
                    }),
            )
            .chain(
                self.host
                    .acknowledged_messages()
                    .into_iter()
                    .filter_map(|entry| {
                        let text = message(&entry.command)?;
                        let thread = self.threads.get(&active);
                        let queued = thread
                            .and_then(|thread| thread.status.as_ref())
                            .is_some_and(|status| {
                                status.queued_messages.iter().any(|message| {
                                    message.delivery_key.as_deref() == Some(entry.key.as_str())
                                })
                            });
                        let recorded = thread
                            .and_then(|thread| thread.history.as_ref())
                            .is_some_and(|held| {
                                held.records.iter().any(|record| {
                                    Self::record_delivery_key(record) == Some(entry.key.as_str())
                                })
                            });
                        if queued || recorded {
                            // Once adopted by the host replica, a later rewind must not
                            // resurrect the acknowledged placeholder.
                            self.host.retire_acknowledged_message(&entry.key);
                            None
                        } else {
                            Some((entry.key, text, None, true))
                        }
                    }),
            )
            .collect()
    }

    fn record_delivery_key(record: &StoredEvent) -> Option<&str> {
        let id = match &record.event {
            agent::AgentEvent::SteerRequested { request_id, .. } => request_id.as_str(),
            agent::AgentEvent::ItemCompleted(item) | agent::AgentEvent::ItemStarted(item) => {
                item.id.as_str()
            }
            _ => return None,
        };
        id.strip_prefix("local-user-")
            .or_else(|| id.strip_prefix("local-steer-"))
    }

    fn retire_record_delivery(&self, session_id: &str, record: &StoredEvent) {
        if let Some(key) = Self::record_delivery_key(record) {
            self.retire_delivery_for(session_id, key);
        }
    }

    fn retire_delivery_for(&self, session_id: &str, key: &str) {
        let matches_session = self
            .host
            .acknowledged_messages()
            .iter()
            .any(|entry| entry.key == key && entry.command.session_id() == Some(session_id))
            || self
                .host
                .pending_commands()
                .iter()
                .any(|(pending_key, command)| {
                    pending_key == key && command.session_id() == Some(session_id)
                });
        if matches_session {
            self.host.retire_acknowledged_message(key);
        }
    }

    pub(crate) fn approval_delivery_pending(&self, request: &str) -> bool {
        self.host.pending_commands().iter().any(|(_, command)| matches!(command,
            Command::RespondApproval { request_id, session_id, .. }
            if request_id == request && Some(session_id.as_str()) == self.active_session_id().as_deref()))
    }

    pub(crate) fn retry_delivery(&self, key: &str) {
        if let Err(error) = self.host.retry_failed(key) {
            log::error!("retry failed: {}", error.message);
        }
    }

    pub(crate) fn discard_delivery(&self, key: &str) {
        self.host.discard_failed(key);
    }

    pub fn queued_outgoing(&self) -> usize {
        self.host.queued_outgoing()
    }

    pub fn baseline_ready(&self) -> bool {
        self.baseline_topics.contains(&Topic::Scope)
            && self.baseline_topics.contains(&self.index_topic())
            && (!self.scope.is_full() || self.baseline_topics.contains(&Topic::Settings))
            && self.selected_session_id.as_ref().is_none_or(|id| {
                self.baseline_topics.contains(&Topic::SessionStatus {
                    session_id: id.clone(),
                }) && self.baseline_topics.contains(&Topic::SessionPlan {
                    session_id: id.clone(),
                }) && self.baseline_topics.contains(&Topic::SessionEvents {
                    session_id: id.clone(),
                })
            })
    }

    /// Cached content remains readable while a new baseline is replayed.
    pub fn threads_loading(&self) -> bool {
        !self.index_hydrated
            || !self.settings_hydrated
            || (!self.connection_state().is_connected()
                && self.index_replica.0.is_empty()
                && self.index_replica.1.is_empty())
    }

    pub fn chat_loading(&self) -> bool {
        if !self.delivery_messages().is_empty() {
            return false;
        }
        if !self.index_hydrated || !self.settings_hydrated {
            return true;
        }
        if self.selected_session_id.is_some() {
            self.session_loading()
        } else {
            self.threads_loading()
        }
    }

    /// End this store's one-link lifetime before its views are replaced.
    pub fn detach(&mut self, cx: &mut App) {
        self.history_task = None;
        self.attachment_tasks.clear();
        for subscription in self.host.subscriptions() {
            let _ = self.host.unsubscribe(subscription);
        }
        self.host.close();
        self.remote_preview.0.close();
        self.remote_preview.1.close();
        if let Some(images) = cx.try_global::<images::HostImages>()
            && images.namespace == self.image_namespace
        {
            cx.set_global(images::HostImages {
                link: None,
                namespace: self.image_namespace,
            });
        }
    }

    pub fn sync_active_conversation_ui(&mut self) {
        let destination = self.session_status_replica.as_ref().map(Self::destination);
        if let (
            Some(tcode_core::ui::ConversationDestination::ProjectDraft(draft)),
            Some(tcode_core::ui::ConversationDestination::Thread(_)),
        ) = (&self.active_destination, &destination)
            && self
                .session_status_replica
                .as_ref()
                .is_some_and(|status| status.project_id.as_deref() == Some(draft.as_str()))
            && let Some(ui) = self
                .conversation_ui
                .remove(self.active_destination.as_ref().unwrap())
        {
            self.conversation_ui
                .insert(destination.clone().unwrap(), ui);
        }
        if let Some((destination, status)) = destination
            .clone()
            .zip(self.session_status_replica.as_ref())
        {
            self.conversation_ui.entry(destination).or_insert_with(|| {
                ConversationUiState::new(
                    self.settings_replica.word_wrap_diffs,
                    status.terminal_open,
                    status.terminal_height,
                )
            });
        }
        self.active_destination = destination;
    }

    fn apply_domain_event(&mut self, envelope: &EventEnvelope, cx: &mut Context<Self>) {
        if !self.host.subscription_reply_is_current(envelope) {
            return;
        }
        if envelope.topic == Topic::Scope {
            if let ServerEvent::ScopeSnapshot(scope) = &envelope.event
                && envelope.request_id.is_some()
                && envelope.request_id != self.applied_scope_request
                && !matches!(
                    self.host.connection_state(),
                    ConnectionState::Reconnecting { .. } | ConnectionState::Offline { .. }
                )
            {
                self.baseline_topics.clear();
                self.apply_scope(scope, cx);
                self.applied_scope_request = envelope.request_id;
            }
            return;
        }
        if matches!(envelope.topic, Topic::Index | Topic::SpaceIndex { .. })
            && envelope.topic != self.index_topic()
        {
            return;
        }
        if !self.scope.is_full()
            && matches!(
                envelope.topic,
                Topic::Settings
                    | Topic::Providers
                    | Topic::RuntimeEvents
                    | Topic::Preview
                    | Topic::ExternalImport { .. }
            )
        {
            return;
        }
        let index_topic = self.index_topic();
        let topic = if envelope.topic == index_topic {
            &Topic::Index
        } else {
            &envelope.topic
        };
        match (topic, &envelope.event) {
            (Topic::SessionStatus { session_id }, ServerEvent::SessionStatusReplaced(status))
                if status.session_id == *session_id =>
            {
                for message in &status.queued_messages {
                    if let Some(key) = &message.delivery_key {
                        self.retire_delivery_for(session_id, key);
                    }
                }
            }
            (Topic::SessionEvents { session_id }, ServerEvent::SessionEvent(record)) => {
                self.retire_record_delivery(session_id, record);
            }
            (Topic::SessionEvents { session_id }, ServerEvent::SessionSnapshot { records, .. }) => {
                for record in records {
                    self.retire_record_delivery(session_id, record);
                }
            }
            _ => {}
        }
        match (topic, &envelope.event) {
            (Topic::Preview, ServerEvent::PreviewRequest { .. }) => {
                let _ = self.remote_preview.0.try_send(envelope.clone());
            }
            (
                Topic::Terminal { terminal_id },
                ServerEvent::TerminalFrame {
                    terminal_id: frame_id,
                    frame,
                },
            ) if terminal_id == frame_id => self.apply_terminal_frame(*terminal_id, frame),
            (
                Topic::Terminal { terminal_id },
                ServerEvent::TerminalDelta {
                    terminal_id: delta_id,
                    delta,
                },
            ) if terminal_id == delta_id => self.apply_terminal_delta(*terminal_id, delta),
            (
                Topic::CommandOutput {
                    session_id,
                    item_id,
                    cols,
                },
                ServerEvent::CommandOutputReplaced(frame),
            ) => self.apply_command_output(session_id, item_id, *cols, frame),
            (Topic::Index, ServerEvent::IndexUpsertSession(meta)) => {
                if let Some(archived) = &mut self.archived_replica {
                    archived.sessions.retain(|archived| archived.id != meta.id);
                }
                match self
                    .index_replica
                    .0
                    .iter_mut()
                    .find(|existing| existing.id == meta.id)
                {
                    Some(existing) => *existing = meta.clone(),
                    None => self.index_replica.0.push(meta.clone()),
                }
                self.index_replica
                    .0
                    .sort_by_key(|meta| std::cmp::Reverse(meta.updated_at));
                self.close_unshown_pull_requests();
            }
            (Topic::Index, ServerEvent::IndexUpsertProject(project)) => {
                images::invalidate_project_icon(project, cx);
                match self
                    .index_replica
                    .1
                    .iter_mut()
                    .find(|existing| existing.id == project.id)
                {
                    Some(existing) => *existing = project.clone(),
                    None => self.index_replica.1.push(project.clone()),
                }
            }
            (Topic::Index, ServerEvent::IndexRemoveSession { session_id }) => {
                if let Some(position) = self
                    .index_replica
                    .0
                    .iter()
                    .position(|meta| meta.id == *session_id)
                {
                    self.removed_session = Some(self.index_replica.0.remove(position));
                }
                self.native_rewind_prefills.remove(session_id);
                self.fallback_blocks.remove(session_id);
                self.fallback_reviews.remove(session_id);
                self.conversation_ui
                    .remove(&ConversationDestination::Thread(session_id.clone()));
                self.open_pull_requests.remove(session_id);
            }
            (Topic::Index, ServerEvent::IndexRemoveProject { project_id }) => {
                self.index_replica
                    .1
                    .retain(|project| project.id != *project_id);
                self.import_statuses.remove(project_id);
                self.conversation_ui
                    .remove(&ConversationDestination::ProjectDraft(project_id.clone()));
            }
            (
                Topic::ExternalImport { project_id },
                ServerEvent::ExternalImportStatusReplaced {
                    project_id: replaced,
                    status,
                },
            ) if project_id == replaced => {
                self.import_statuses
                    .insert(project_id.clone(), status.clone());
            }
            (Topic::Index, ServerEvent::IndexSnapshot(snapshot)) => {
                self.index_hydrated = true;
                let fresh_baseline = self.baseline_topics.insert(index_topic.clone());
                for project in &snapshot.projects {
                    if fresh_baseline
                        || self
                            .index_replica
                            .1
                            .iter()
                            .any(|old| old.id == project.id && old.icon_path != project.icon_path)
                    {
                        images::invalidate_project_icon(project, cx);
                    }
                }
                self.index_replica = (snapshot.sessions.clone(), snapshot.projects.clone());
                self.close_unshown_pull_requests();
                // Client state for a conversation the index no longer lists has
                // nothing left to return to: a deleted project takes its draft's
                // state, a deleted or archived thread its own.
                self.conversation_ui
                    .retain(|destination, _| match destination {
                        ConversationDestination::ProjectDraft(project_id) => snapshot
                            .projects
                            .iter()
                            .any(|project| project.id == *project_id),
                        ConversationDestination::Thread(session_id) => {
                            snapshot.sessions.iter().any(|meta| meta.id == *session_id)
                        }
                    });
                self.threads.retain(|session_id, _| {
                    self.selected_session_id.as_ref() == Some(session_id)
                        || snapshot.sessions.iter().any(|meta| meta.id == *session_id)
                });
                self.restore_index_selection();
                self.apply_index_summary(&snapshot.summary, cx);
                if self.archived_holds > 0 {
                    self.load_archived_sessions(cx);
                }
            }
            (
                Topic::Index,
                ServerEvent::IndexUnchanged {
                    fingerprint,
                    archived_revision,
                },
            ) => {
                if self.cached_index_fingerprint().as_ref() == Some(fingerprint) {
                    self.index_summary.archived_revision = *archived_revision;
                    if self.baseline_topics.insert(index_topic.clone()) {
                        for project in &self.index_replica.1 {
                            images::invalidate_project_icon(project, cx);
                        }
                    }
                    self.restore_index_selection();
                    if self.archived_holds > 0 {
                        self.load_archived_sessions(cx);
                    }
                } else {
                    let _ = self.host.subscribe(Subscription {
                        topic: index_topic.clone(),
                        after: None,
                        known_index: None,
                    });
                }
            }
            (Topic::Index, ServerEvent::IndexSummaryReplaced(summary)) => {
                self.apply_index_summary(summary, cx);
            }
            (Topic::Index, ServerEvent::IndexActivitiesChanged { upserts, removed }) => {
                if self.baseline_topics.contains(&index_topic) {
                    for id in removed {
                        self.index_summary.activity.remove(id);
                    }
                    self.index_summary.activity.extend(
                        upserts
                            .iter()
                            .map(|(id, activity)| (id.clone(), activity.clone())),
                    );
                }
            }
            (Topic::Settings, ServerEvent::LastVisitedChanged(visits)) => {
                self.settings_replica
                    .last_visited
                    .extend(visits.iter().map(|(id, at)| (id.clone(), *at)));
            }
            (Topic::Settings, ServerEvent::SettingsReplaced(settings))
            | (Topic::Settings, ServerEvent::SettingsSnapshot(settings)) => {
                self.settings_replica = settings.clone();
                self.settings_hydrated = true;
                if matches!(envelope.event, ServerEvent::SettingsSnapshot(_)) {
                    self.baseline_topics.insert(Topic::Settings);
                }
            }
            (Topic::Providers, ServerEvent::ProvidersReplaced(status)) => {
                self.providers_replica = status.clone();
                self.baseline_topics.insert(Topic::Providers);
            }
            (Topic::GitStatus { session_id }, ServerEvent::GitStatusReplaced(status)) => {
                if let Some(thread) = self.threads.get_mut(session_id) {
                    thread.git = Some(status.clone());
                }
                if self.selected_session_id.as_ref() == Some(session_id) {
                    self.git_status_replica = status.clone();
                }
            }
            (Topic::SessionStatus { session_id }, ServerEvent::SessionStatusReplaced(status))
                if status.session_id == *session_id =>
            {
                self.baseline_topics.insert(envelope.topic.clone());
                if let Some(thread) = self.threads.get_mut(session_id) {
                    thread.status = Some(status.as_ref().clone());
                }
                if self.selected_session_id.as_ref() == Some(session_id) {
                    let mut status = status.as_ref().clone();
                    status.native_rewind_prefill_available =
                        self.native_rewind_prefills.contains_key(session_id);
                    self.session_status_replica = Some(status);
                    if let Some((replica_id, mut timeline)) = self.session_replica.take() {
                        if replica_id == *session_id {
                            self.settle_running_turn(&mut timeline);
                        }
                        self.session_replica = Some((replica_id, timeline));
                    }
                    self.sync_terminal_topics();
                    self.sync_active_conversation_ui();
                }
            }
            (
                Topic::SessionStatus { session_id },
                ServerEvent::SessionUsageChanged {
                    usage,
                    context_window,
                },
            ) if self.baseline_topics.contains(&envelope.topic) => {
                if let Some(status) = self
                    .threads
                    .get_mut(session_id)
                    .and_then(|thread| thread.status.as_mut())
                {
                    status.usage = *usage;
                    status.context_window = *context_window;
                }
                if let Some(status) = self
                    .session_status_replica
                    .as_mut()
                    .filter(|status| status.session_id == *session_id)
                {
                    status.usage = *usage;
                    status.context_window = *context_window;
                }
            }
            (Topic::SessionPlan { session_id }, ServerEvent::SessionPlanReplaced(plan))
                if plan.session_id == *session_id =>
            {
                self.baseline_topics.insert(envelope.topic.clone());
                if let Some(thread) = self.threads.get_mut(session_id) {
                    thread.plan = Some(plan.clone());
                }
            }
            (Topic::SessionEvents { session_id }, ServerEvent::SessionHistoryError(error))
                if self.selected_session_id.as_ref() == Some(session_id) =>
            {
                self.history_error = Some(history::history_error_message(error.clone()));
            }
            (
                Topic::SessionEvents { session_id },
                ServerEvent::SessionSnapshot {
                    from,
                    end,
                    records,
                    total,
                    total_turns,
                    ..
                },
            ) => {
                if self.selected_session_id.as_ref() != Some(session_id) {
                    return;
                }
                let after = match envelope.request_id {
                    Some(_) => match self.host.consume_session_snapshot(envelope) {
                        Some(after) => after,
                        None => return,
                    },
                    None => None,
                };
                let continuation = after.filter(|after| *after <= *total);
                let thread = self.threads.entry(session_id.clone()).or_default();
                let retain_timeline = continuation == Some(*from)
                    && records.is_empty()
                    && thread.total_turns.is_some()
                    && *from != 0
                    && thread
                        .history
                        .as_ref()
                        .is_some_and(|held| held.end == *from)
                    && self
                        .session_replica
                        .as_ref()
                        .is_some_and(|(id, _)| id == session_id);
                thread.total_turns = None;
                let held = &mut thread.history;
                match held {
                    // Continues the held cursor.
                    Some(held) if continuation == Some(*from) && held.end == *from => {
                        held.extend(records, *end);
                    }
                    _ => *held = Some(history::HeldHistory::new(*from, *end, records)),
                }
                let after = *end;
                self.session_catching_up = after < *total;
                if self.session_catching_up {
                    let _ = self.host.update_after(&envelope.topic, after);
                    return;
                }
                self.host.remember_after(&envelope.topic, after);
                self.threads
                    .get_mut(session_id)
                    .expect("snapshot thread")
                    .total_turns = Some(*total_turns as usize);
                self.baseline_topics.insert(envelope.topic.clone());
                if retain_timeline {
                    let timeline = &self.session_replica.as_ref().expect("retained timeline").1;
                    self.session_turn_offset =
                        (*total_turns as usize).saturating_sub(timeline.turns.len());
                    return;
                }
                let mut timeline = self.fold_held_records(session_id);
                self.session_turn_offset =
                    (*total_turns as usize).saturating_sub(timeline.turns.len());
                self.settle_running_turn(&mut timeline);
                self.session_replica = Some((session_id.clone(), timeline));
            }
            (Topic::SessionEvents { session_id }, ServerEvent::SessionEvent(record)) => {
                if self.selected_session_id.as_ref() != Some(session_id) {
                    return;
                }
                // Until its window arrives the thread may hold an earlier
                // visit's records, which the window continues: a record sent
                // before the window is part of it.
                if self.session_catching_up
                    || !self.baseline_topics.contains(&envelope.topic)
                    || self.session_replica.is_none()
                {
                    return;
                }
                let Some(held) = self
                    .threads
                    .get_mut(session_id)
                    .and_then(|thread| thread.history.as_mut())
                else {
                    return;
                };
                held.extend(std::slice::from_ref(record), held.end + 1);
                let after = held.end;
                self.host.remember_after(&envelope.topic, after);
                // A new turn means the user moved on; the recovery card for the
                // stopped one is stale.
                if matches!(record.event, agent::AgentEvent::TurnStarted { .. }) {
                    self.fallback_blocks.remove(session_id);
                    self.fallback_reviews.remove(session_id);
                }
                self.apply_conversation_event(session_id, &record.event);
                if let Some((replica_id, mut timeline)) = self.session_replica.take() {
                    if replica_id == *session_id {
                        timeline.apply_stored(record);
                        self.settle_running_turn(&mut timeline);
                        self.threads
                            .get_mut(session_id)
                            .expect("live thread")
                            .total_turns = Some(self.session_turn_offset + timeline.turns.len());
                    }
                    self.session_replica = Some((replica_id, timeline));
                }
            }
            (
                Topic::SessionStatus { session_id },
                ServerEvent::NativeRewindPrefill {
                    session_id: event_session,
                    text,
                },
            ) if session_id == event_session => {
                self.native_rewind_prefills
                    .insert(session_id.clone(), text.clone());
                if let Some(status) = self
                    .session_status_replica
                    .as_mut()
                    .filter(|status| status.session_id == *session_id)
                {
                    status.native_rewind_prefill_available = true;
                }
            }
            (
                Topic::SessionStatus { session_id },
                ServerEvent::ModelFallbackBlocked {
                    session_id: event_session,
                    category,
                    model,
                    fallback_model,
                    detail,
                },
            ) if session_id == event_session => {
                self.fallback_blocks.insert(
                    session_id.clone(),
                    FallbackBlock {
                        category: category.clone(),
                        model: model.clone(),
                        fallback_model: fallback_model.clone(),
                        detail: detail.clone(),
                    },
                );
            }
            (
                Topic::SessionStatus { session_id },
                ServerEvent::FallbackReviewReady {
                    session_id: event_session,
                    assessment,
                    draft,
                },
            ) if session_id == event_session => {
                self.fallback_reviews.insert(
                    session_id.clone(),
                    FallbackReview {
                        assessment: assessment.clone(),
                        draft: draft.clone(),
                    },
                );
            }
            _ => {}
        }
        self.load_pending_chat_history(cx);
        if envelope.topic == index_topic {
            self.host
                .remember_known_index(&index_topic, self.cached_index_fingerprint());
            self.reconcile_destination(cx);
            self.removed_session = None;
            // A thread kept on screen (its failed send still offers Retry)
            // keeps its replicas until the user leaves it.
            if let ServerEvent::IndexRemoveSession { session_id } = &envelope.event
                && self.selected_session_id.as_ref() != Some(session_id)
            {
                self.threads.remove(session_id);
            }
        }
        self.acknowledge_read(cx);
    }

    pub(crate) fn set_conversation_on_screen(&mut self, on_screen: bool, cx: &mut Context<Self>) {
        if !self.scope.is_full() && on_screen && !self.conversation_on_screen {
            self.read_acknowledged = None;
        }
        self.conversation_on_screen = on_screen;
        self.acknowledge_read(cx);
    }

    /// Report the thread on screen read through its current `updated_at`
    /// once its conversation has loaded. Only this marks a thread read: a
    /// subscription can reach the host long after the user left a view that
    /// never loaded.
    fn acknowledge_read(&mut self, cx: &mut Context<Self>) {
        if !self.conversation_on_screen {
            return;
        }
        let Some(session_id) = self
            .session_replica
            .as_ref()
            .map(|(id, _)| id)
            .filter(|id| self.selected_session_id.as_ref() == Some(id))
            .cloned()
        else {
            return;
        };
        if self.session_catching_up
            || !self.baseline_topics.contains(&Topic::SessionEvents {
                session_id: session_id.clone(),
            })
        {
            return;
        }
        let Some(through) = self
            .index_replica
            .0
            .iter()
            .find(|meta| meta.id == session_id)
            .map(|meta| meta.updated_at)
        else {
            return;
        };
        let acknowledged = self
            .read_acknowledged
            .as_ref()
            .filter(|(id, _)| *id == session_id)
            .map(|(_, at)| *at)
            .max(self.settings_replica.last_visited.get(&session_id).copied());
        if acknowledged.is_some_and(|at| at >= through) {
            return;
        }
        self.read_acknowledged = Some((session_id.clone(), through));
        self.dispatch(Command::MarkSessionRead {
            session_id,
            through,
        });
        self.local_settings_changed(cx);
    }

    /// Decide what the workspace shows after the index changed.
    ///
    /// Only the conversation on screen is reconciled, so archiving a
    /// background thread (an Orchestrate sibling completing, a sweep) never
    /// steals navigation. When the thread on screen leaves the visible index —
    /// archived (alone or with its lead), deleted — the workspace
    /// follows its still-visible parent; with no such parent it falls back to
    /// the last interacted project's draft, which is also what an empty
    /// workspace opens.
    fn reconcile_destination(&mut self, cx: &mut Context<Self>) {
        // A deletion can arrive before the rejected Ack. Keep the authored
        // write on screen so its failure still has Retry and Discard controls.
        if !self.delivery_messages().is_empty() {
            return;
        }
        match &self.session_status_replica {
            // A draft has no index entry of its own; it stays until the user
            // navigates away. Its project leaving the index is the exception:
            // there is nothing left to draft into, so it is treated like a
            // vanished thread.
            Some(status) if status.draft => {
                let project_gone = status.project_id.as_ref().is_some_and(|project_id| {
                    !self
                        .index_replica
                        .1
                        .iter()
                        .any(|project| project.id == *project_id)
                });
                if !project_gone {
                    return;
                }
                self.leave_session();
            }
            Some(status) => {
                let session_id = status.session_id.clone();
                if self.session_visible(&session_id) {
                    return;
                }
                let parent = self
                    .index_replica
                    .0
                    .iter()
                    .chain(&self.removed_session)
                    .find(|meta| meta.id == session_id)
                    .and_then(|meta| meta.parent_session_id.clone())
                    .filter(|parent| self.session_visible(parent));
                if let Some(parent) = parent {
                    self.select_session(parent);
                    return;
                }
                self.leave_session();
            }
            // Selected, but its first status has not arrived: nothing to
            // decide yet. Without a selection this is the empty workspace.
            None if self.selected_session_id.is_some() => return,
            None => {}
        }
        self.open_last_project_draft(cx);
    }

    fn session_visible(&self, session_id: &str) -> bool {
        self.index_replica
            .0
            .iter()
            .any(|meta| meta.id == session_id && meta.archived_at.is_none())
    }

    /// Open the new-thread draft of the project the user last interacted with,
    /// so an empty workspace offers a composer instead of a dead end. The
    /// runtime returns that project's standing draft when it already has one,
    /// keeping its composer attachments. A remembered project that is gone
    /// falls back to the first listed one; with no projects at all the chat
    /// view keeps its add-project state.
    fn open_last_project_draft(&mut self, cx: &mut Context<Self>) {
        if self.draft_fallback_pending {
            return;
        }
        let remembered = self.settings_replica.last_project_id.as_deref();
        let Some(project) = self
            .index_replica
            .1
            .iter()
            .filter(|project| !project.quick_chat)
            .find(|project| Some(project.id.as_str()) == remembered)
            .or_else(|| {
                self.index_replica
                    .1
                    .iter()
                    .find(|project| !project.quick_chat)
            })
            .cloned()
        else {
            return;
        };
        self.start_draft(project.id, project.root, cx);
    }

    fn apply_conversation_event(&mut self, session_id: &str, event: &agent::AgentEvent) {
        let destination = ConversationDestination::Thread(session_id.to_string());
        let Some(ui) = self.conversation_ui.get_mut(&destination) else {
            return;
        };
        match event {
            agent::AgentEvent::TurnStarted { .. } => {
                ui.auto_open_task_suppressed = false;
            }
            agent::AgentEvent::PlanUpdated { .. }
                if self.settings_replica.auto_open_task_panel
                    && !ui.auto_open_task_suppressed
                    && !(ui.right_panel_open && ui.right_tab == RightTab::Plan) =>
            {
                ui.right_panel_open = true;
                ui.right_tab = RightTab::Plan;
            }
            agent::AgentEvent::TurnCompleted { .. } | agent::AgentEvent::RewindCompleted { .. } => {
                ui.refresh_diff()
            }
            _ => {}
        }
    }

    pub fn all_provider_profiles(&self) -> Vec<ResolvedProfile> {
        if !self.scope.is_full() {
            return self
                .scoped_providers
                .iter()
                .map(|choice| ResolvedProfile {
                    id: choice.profile_id.clone().unwrap_or_else(|| {
                        tcode_core::settings::provider_key(choice.provider).to_owned()
                    }),
                    kind: choice.provider,
                    settings: ProviderSettings {
                        enabled: true,
                        ..Default::default()
                    },
                })
                .collect();
        }
        agent::ProviderKind::NATIVE
            .into_iter()
            .flat_map(|kind| self.settings_replica.profiles_for_kind(kind))
            .collect()
    }

    pub fn enabled_profiles(&self) -> Vec<ResolvedProfile> {
        self.all_provider_profiles()
            .into_iter()
            .filter(|profile| profile.settings.enabled)
            .collect()
    }

    fn scoped_choice(&self, profile_id: &str) -> Option<&ScopedProviderChoice> {
        self.scoped_providers.iter().find(|choice| {
            choice
                .profile_id
                .as_deref()
                .unwrap_or(tcode_core::settings::provider_key(choice.provider))
                == profile_id
        })
    }

    pub fn profile_catalog(&self, profile_id: &str) -> Vec<agent::ModelSpec> {
        if !self.scope.is_full() {
            return self
                .scoped_choice(profile_id)
                .map(|choice| choice.models.clone())
                .unwrap_or_default();
        }
        if Settings::is_builtin_profile_id(profile_id) {
            let kind = self
                .settings_replica
                .resolved_profile(profile_id)
                .map(|profile| profile.kind)
                .unwrap_or(agent::ProviderKind::ClaudeCode);
            self.providers_replica
                .model_catalogs
                .get(&kind)
                .cloned()
                .unwrap_or_default()
        } else {
            Vec::new()
        }
    }

    #[cfg(test)]
    pub(crate) fn drain_host_events_for_test(&mut self, cx: &mut Context<Self>) {
        let events = self.host.events();
        while let Ok(envelope) = events.try_recv() {
            if let ServerEvent::Runtime(event) = &envelope.event {
                cx.emit(event.clone());
            } else {
                self.apply_domain_event(&envelope, cx);
                cx.emit(StoreChange {
                    topic: TopicKind::from(&envelope.topic),
                });
            }
            cx.notify();
        }
    }

    pub fn working_sessions_count(&self) -> usize {
        self.index_summary
            .activity
            .values()
            .filter(|activity| activity.working)
            .count()
    }

    fn active_conversation_ui(&self) -> Option<&crate::conversation_ui::ConversationUiState> {
        self.conversation_ui.get(self.active_destination.as_ref()?)
    }

    fn active_conversation_ui_mut(
        &mut self,
    ) -> Option<&mut crate::conversation_ui::ConversationUiState> {
        let destination = self.active_destination.clone()?;
        self.conversation_ui.get_mut(&destination)
    }

    fn active_turn_running(&self) -> bool {
        self.session_status_replica
            .as_ref()
            .is_some_and(|status| status.activity.turn_running)
    }

    fn fold_held_records(&self, session_id: &str) -> Timeline {
        Timeline::fold_stored(
            self.threads
                .get(session_id)
                .and_then(|thread| thread.history.as_ref())
                .into_iter()
                .flat_map(|held| &held.records),
        )
    }

    /// Records folded after their provider stopped still end running, and a
    /// window cut inside the running turn misses its start: the host's status
    /// places the running turn among the held ones. Status and events are
    /// separate topics, so each settles the replica whenever it changes.
    fn settle_running_turn(&self, timeline: &mut Timeline) {
        timeline.turn_retry = self
            .session_status_replica
            .as_ref()
            .and_then(|status| status.turn_retry.clone());
        let running = self
            .session_status_replica
            .as_ref()
            .and_then(|status| status.running_turn);
        timeline.settle_running_turn(
            self.active_turn_running(),
            running.and_then(|running| {
                usize::try_from(running.turn)
                    .ok()?
                    .checked_sub(self.session_turn_offset)
            }),
            running.and_then(|running| running.started_at),
        );
    }

    fn suppress_task_auto_open_if_running(&mut self) {
        let running = self.active_turn_running();
        if running && let Some(ui) = self.active_conversation_ui_mut() {
            ui.auto_open_task_suppressed = true;
        }
    }

    pub fn toggle_diff_panel(&mut self, cx: &mut Context<Self>) {
        let closing = self
            .active_conversation_ui()
            .is_some_and(|ui| ui.right_panel_open && ui.right_tab == RightTab::Diff);
        if let Some(ui) = self.active_conversation_ui_mut() {
            if closing {
                ui.right_panel_open = false;
                ui.pending_diff_focus = None;
            } else {
                ui.right_panel_open = true;
                ui.right_tab = RightTab::Diff;
                ui.refresh_diff();
            }
        }
        if closing {
            self.suppress_task_auto_open_if_running();
        }
        cx.notify();
    }

    pub fn open_diff_for_turn(&mut self, turn: usize, cx: &mut Context<Self>) {
        if let Some(ui) = self.active_conversation_ui_mut() {
            ui.pending_diff_focus = None;
            ui.right_panel_open = true;
            ui.right_tab = RightTab::Diff;
            ui.diff_selected_turn = Some(turn);
            ui.refresh_diff();
            cx.notify();
        }
    }

    pub fn open_diff_for_file(&mut self, turn: usize, path: String, cx: &mut Context<Self>) {
        let session = self
            .session_status_replica
            .as_ref()
            .map(|status| status.session_id.clone());
        if let (Some(session), Some(ui)) = (session, self.active_conversation_ui_mut()) {
            ui.right_panel_open = true;
            ui.right_tab = RightTab::Diff;
            ui.diff_selected_turn = Some(turn);
            ui.pending_diff_focus = Some(DiffFocus {
                session,
                turn,
                path,
            });
            ui.refresh_diff();
            cx.notify();
        }
    }

    pub fn select_diff_turn(&mut self, turn: usize, cx: &mut Context<Self>) {
        if let Some(ui) = self.active_conversation_ui_mut() {
            ui.pending_diff_focus = None;
            ui.diff_selected_turn = Some(turn);
            ui.refresh_diff();
            cx.notify();
        }
    }

    pub fn discard_diff_focus(&mut self, cx: &mut Context<Self>) {
        if let Some(ui) = self.active_conversation_ui_mut() {
            ui.discard_diff_focus();
            cx.notify();
        }
    }

    pub fn close_diff_panel(&mut self, cx: &mut Context<Self>) {
        if let Some(ui) = self.active_conversation_ui_mut() {
            ui.pending_diff_focus = None;
            ui.right_panel_open = false;
        }
        self.suppress_task_auto_open_if_running();
        cx.notify();
    }

    pub fn toggle_diff_expanded(&mut self, cx: &mut Context<Self>) {
        if let Some(ui) = self.active_conversation_ui_mut() {
            ui.right_panel_expanded = !ui.right_panel_expanded;
            cx.notify();
        }
    }

    pub fn set_right_tab(&mut self, tab: RightTab, cx: &mut Context<Self>) {
        if tab == RightTab::Preview && !self.offers_preview() {
            return;
        }
        if let Some(ui) = self.active_conversation_ui_mut() {
            if tab == RightTab::Diff && ui.right_tab != RightTab::Diff {
                ui.refresh_diff();
            }
            ui.right_tab = tab;
            cx.notify();
        }
    }

    fn toggle_tab_panel(&mut self, tab: RightTab, cx: &mut Context<Self>) {
        let closing = self
            .active_conversation_ui()
            .is_some_and(|ui| ui.right_panel_open && ui.right_tab == tab);
        if let Some(ui) = self.active_conversation_ui_mut() {
            ui.right_panel_open = !closing;
            ui.right_tab = tab;
        }
        if closing {
            self.suppress_task_auto_open_if_running();
        }
        cx.notify();
    }

    pub fn toggle_pull_requests_panel(&mut self, cx: &mut Context<Self>) {
        self.toggle_tab_panel(RightTab::PullRequests, cx);
    }
    pub fn toggle_plan_panel(&mut self, cx: &mut Context<Self>) {
        self.toggle_tab_panel(RightTab::Plan, cx);
    }

    pub fn toggle_agents_panel(&mut self, cx: &mut Context<Self>) {
        self.toggle_tab_panel(RightTab::Agents, cx);
    }

    pub fn toggle_preview_panel(&mut self, cx: &mut Context<Self>) {
        if !self.offers_preview() {
            return;
        }
        self.toggle_tab_panel(RightTab::Preview, cx);
    }

    pub fn close_preview_panel(&mut self, cx: &mut Context<Self>) {
        let showing = self
            .active_conversation_ui()
            .is_some_and(|ui| ui.right_panel_open && ui.right_tab == RightTab::Preview);
        if showing && let Some(ui) = self.active_conversation_ui_mut() {
            ui.right_panel_open = false;
        }
        if showing {
            self.suppress_task_auto_open_if_running();
            cx.notify();
        }
    }

    pub fn open_preview_panel(&mut self, cx: &mut Context<Self>) {
        if !self.offers_preview() {
            return;
        }
        if let Some(ui) = self.active_conversation_ui_mut()
            && !(ui.right_panel_open && ui.right_tab == RightTab::Preview)
        {
            ui.right_panel_open = true;
            ui.right_tab = RightTab::Preview;
            cx.notify();
        }
    }

    pub fn open_tab_for(&mut self, session_id: &str, tab: RightTab, cx: &mut Context<Self>) {
        if tab == RightTab::Preview && !self.offers_preview() {
            return;
        }
        let destination = if self
            .session_status_replica
            .as_ref()
            .is_some_and(|status| status.session_id == session_id)
        {
            self.active_destination
                .clone()
                .unwrap_or_else(|| ConversationDestination::Thread(session_id.to_string()))
        } else {
            ConversationDestination::Thread(session_id.to_string())
        };
        let ui = self.conversation_ui.entry(destination).or_insert_with(|| {
            ConversationUiState::new(self.settings_replica.word_wrap_diffs, false, 240.)
        });
        ui.right_panel_open = true;
        ui.right_tab = tab;
        cx.notify();
    }

    /// The repository a bare pull request number refers to in this thread, when known.
    pub fn github_repository(&self, session_id: &str) -> Option<String> {
        (self.selected_session_id.as_deref() == Some(session_id))
            .then_some(self.git_status_replica.status.as_ref())
            .flatten()
            .and_then(|status| status.github_repository.clone())
    }

    /// The thread's index entry.
    pub fn thread_meta(&self, session_id: &str) -> Option<&SessionMeta> {
        self.index_replica
            .0
            .iter()
            .find(|meta| meta.id == session_id)
    }

    /// The thread's links as its index entry carries them.
    pub fn pull_requests(
        &self,
        session_id: &str,
    ) -> &[tcode_core::pull_request::ThreadPullRequestLink] {
        self.thread_meta(session_id)
            .map_or(&[], |meta| meta.pull_requests.as_slice())
    }

    /// The pull request the thread has open, read inside its Pull requests tab or page.
    pub fn open_pull_request(
        &self,
        session_id: &str,
    ) -> Option<&tcode_core::pull_request::PullRequestKey> {
        self.open_pull_requests.get(session_id)
    }

    /// A pull request the thread no longer shows, unlinked or out of its stack, is not read
    /// on through it: its view closes back to the list.
    fn close_unshown_pull_requests(&mut self) {
        let open = std::mem::take(&mut self.open_pull_requests);
        self.open_pull_requests = open
            .into_iter()
            .filter(|(session_id, key)| {
                tcode_core::pull_request::shown(self.pull_requests(session_id), key)
            })
            .collect();
    }

    /// Whether this device folded the stack map's note about GitHub stacks away.
    pub fn stack_note_collapsed(&self) -> bool {
        self.client_preferences
            .navigation
            .as_ref()
            .and_then(|navigation| navigation["stack_note_collapsed"].as_bool())
            .unwrap_or(false)
    }

    pub fn set_stack_note_collapsed(&mut self, collapsed: bool, cx: &mut Context<Self>) {
        self.client_preferences
            .navigation
            .get_or_insert_with(|| serde_json::json!({}))["stack_note_collapsed"] =
            serde_json::json!(collapsed);
        if let Some(host) = &self.client_host {
            let mut preferences = host.load_preferences();
            preferences
                .navigation
                .get_or_insert_with(|| serde_json::json!({}))["stack_note_collapsed"] =
                serde_json::json!(collapsed);
            host.save_preferences(&preferences);
        }
        cx.notify();
    }

    pub fn set_open_pull_request(
        &mut self,
        session_id: &str,
        key: Option<tcode_core::pull_request::PullRequestKey>,
        cx: &mut Context<Self>,
    ) {
        let changed = match key {
            Some(key) => {
                self.open_pull_requests
                    .insert(session_id.to_owned(), key.clone())
                    != Some(key)
            }
            None => self.open_pull_requests.remove(session_id).is_some(),
        };
        if changed {
            cx.notify();
        }
    }

    fn conversation_ui_by_key(&self, key: &str) -> Option<&ConversationUiState> {
        self.conversation_ui
            .iter()
            .find_map(|(destination, ui)| (destination.preference_key() == key).then_some(ui))
    }

    fn conversation_ui_by_key_mut(&mut self, key: &str) -> Option<&mut ConversationUiState> {
        self.conversation_ui
            .iter_mut()
            .find_map(|(destination, ui)| (destination.preference_key() == key).then_some(ui))
    }

    pub fn preview_url(&self, key: &str) -> Option<String> {
        self.conversation_ui_by_key(key)
            .and_then(|ui| ui.preview_url.clone())
    }

    pub fn set_preview_url(&mut self, key: &str, url: String, cx: &mut Context<Self>) {
        if let Some(ui) = self.conversation_ui_by_key_mut(key) {
            ui.preview_url = Some(url);
            cx.notify();
        }
    }

    pub fn preview_canvas(&self, key: &str) -> Option<(u32, u32)> {
        self.conversation_ui_by_key(key)
            .and_then(|ui| ui.preview_canvas)
    }

    pub fn set_preview_canvas(
        &mut self,
        key: &str,
        canvas: Option<(u32, u32)>,
        cx: &mut Context<Self>,
    ) {
        if let Some(ui) = self.conversation_ui_by_key_mut(key) {
            ui.preview_canvas = canvas;
            cx.notify();
        }
    }

    pub fn clear_preview_chrome(&mut self, key: &str, cx: &mut Context<Self>) {
        if let Some(ui) = self.conversation_ui_by_key_mut(key) {
            ui.preview_url = None;
            ui.preview_canvas = None;
            cx.notify();
        }
    }

    /// The project view: projects ordered by the activity of every thread
    /// they hold, agents included, each listing its roster threads newest
    /// updated first. Pinning, arranging and settling order only the threads
    /// view.
    pub fn grouped_sessions(&self) -> Vec<ProjectGroup> {
        let unarchived: Vec<_> = self
            .index_replica
            .0
            .iter()
            .filter(|meta| meta.archived_at.is_none())
            .cloned()
            .collect();
        let mut groups = group_sessions(
            &self.index_replica.1,
            &unarchived,
            self.settings_replica.project_sort,
        );
        for group in &mut groups {
            group.sessions.retain(tcode_core::thread_sort::in_roster);
            group
                .sessions
                .sort_by_key(|meta| std::cmp::Reverse(meta.updated_at));
        }
        groups
    }

    pub fn settings(&self) -> Settings {
        effective_client_settings(&self.settings_replica, &self.client_preferences)
    }

    pub fn title_generating(&self, session_id: &str) -> bool {
        self.index_summary.title_generating.contains(session_id)
    }

    fn cached_index_fingerprint(&self) -> Option<String> {
        self.index_hydrated
            .then(|| {
                tcode_protocol::index_fingerprint(
                    &self.index_summary,
                    &self.index_replica.0,
                    &self.index_replica.1,
                )
                .ok()
            })
            .flatten()
    }

    fn restore_index_selection(&mut self) {
        if let Some(id) = self.selected_session_id.clone() {
            let visible = self.index_replica.0.iter().any(|meta| meta.id == id)
                || self.session_status_replica.as_ref().is_some_and(|status| {
                    status.draft
                        && status.project_id.as_ref().is_some_and(|project| {
                            self.index_replica.1.iter().any(|p| &p.id == project)
                        })
                });
            if visible
                && !self
                    .host
                    .subscribed_topics()
                    .contains(&Topic::SessionStatus {
                        session_id: id.clone(),
                    })
            {
                self.selected_session_id = None;
                self.select_session(id);
            } else if !visible && !self.scope.is_full() {
                self.leave_session();
            }
        }
    }

    fn apply_index_summary(&mut self, summary: &IndexSummary, cx: &mut Context<Self>) {
        self.index_summary = summary.clone();
        if self.archived_holds > 0
            && self
                .archived_replica
                .as_ref()
                .is_none_or(|archived| archived.revision != summary.archived_revision)
        {
            self.load_archived_sessions(cx);
        }
    }

    fn load_archived_sessions(&mut self, cx: &mut Context<Self>) {
        if self.archived_task.is_some() {
            return;
        }
        let host = self.host.clone();
        self.archived_task = Some(cx.spawn(async move |this, cx| {
            let result = host.query(Query::ArchivedSessions).await;
            let _ = this.update(cx, |store, cx| {
                store.archived_task = None;
                match result {
                    Ok(QueryResponse::ArchivedSessions(archived)) => {
                        if archived.revision < store.index_summary.archived_revision {
                            store.load_archived_sessions(cx);
                        } else {
                            store.archived_replica = Some(archived);
                        }
                    }
                    Ok(other) => log::warn!("unexpected archived-sessions response: {other:?}"),
                    Err(error) => log::warn!("archived sessions failed: {}", error.message),
                }
                cx.notify();
            });
        }));
    }

    /// Keep the archived threads current until a matching release. Every
    /// view that shows them holds them, so one closing never empties another.
    pub fn hold_archived_sessions(&mut self, cx: &mut Context<Self>) {
        self.archived_holds += 1;
        if self.archived_holds == 1 {
            self.load_archived_sessions(cx);
        }
    }

    pub fn release_archived_sessions(&mut self) {
        self.archived_holds = self.archived_holds.saturating_sub(1);
        if self.archived_holds == 0 {
            self.archived_replica = None;
            self.archived_task = None;
        }
    }

    pub fn archived_loading(&self) -> bool {
        self.archived_replica.is_none()
    }

    pub fn archived_sessions(&self) -> &[SessionMeta] {
        self.archived_replica
            .as_ref()
            .map(|archived| archived.sessions.as_slice())
            .unwrap_or_default()
    }

    /// Whether the Index baseline has arrived, including an empty Index.
    pub fn index_hydrated(&self) -> bool {
        self.index_hydrated
    }

    /// Whether [`WorkspaceStore::settings`] reflects the host yet.
    pub fn settings_hydrated(&self) -> bool {
        self.settings_hydrated
    }

    /// Whether this client can hand a produced file to the platform (a browser
    /// download, a share sheet).
    pub fn supports_artifact_delivery(&self) -> bool {
        self.client_host
            .as_ref()
            .is_some_and(|host| host.supports_artifact_delivery())
    }

    pub fn deliver_artifact(&self, name: &str, mime: &str, bytes: &[u8]) -> Result<(), String> {
        match &self.client_host {
            Some(host) => host.deliver_artifact(name, mime, bytes),
            None => Err("this device cannot save files".into()),
        }
    }

    /// Open a *client-local* path in the user's editor. `None` when this client
    /// has no editor integration at all.
    pub fn open_in_editor(&self, path: &std::path::Path) -> Option<Result<(), String>> {
        self.client_host.as_ref()?.open_in_editor(path)
    }

    pub fn client_theme_override(&self) -> Option<ThemeMode> {
        match self.client_preferences.appearance.as_deref() {
            Some("system") => Some(ThemeMode::System),
            Some("light") => Some(ThemeMode::Light),
            Some("dark") => Some(ThemeMode::Dark),
            _ => None,
        }
    }

    pub fn set_client_theme(&mut self, mode: Option<ThemeMode>) {
        self.client_preferences.appearance = mode.map(|mode| match mode {
            ThemeMode::System => "system".to_owned(),
            ThemeMode::Light => "light".to_owned(),
            ThemeMode::Dark => "dark".to_owned(),
        });
        self.save_client_preferences();
    }

    pub fn client_language_override(&self) -> Option<&str> {
        self.client_preferences.language.as_deref()
    }

    pub fn set_client_language(&mut self, language: Option<String>) {
        self.client_preferences.language = language;
        self.save_client_preferences();
    }

    pub fn client_device_name_override(&self) -> Option<&str> {
        self.client_preferences.device_name.as_deref()
    }

    pub fn client_device_name(&self) -> String {
        self.client_preferences
            .device_name
            .clone()
            .filter(|name| !name.trim().is_empty())
            .or_else(|| self.client_host.as_ref().map(|host| host.device_name()))
            .unwrap_or_else(|| crate::tr!("app.name").into_owned())
    }

    pub fn set_client_device_name(&mut self, name: Option<String>) {
        self.client_preferences.device_name = name.filter(|name| !name.trim().is_empty());
        self.save_client_preferences();
    }

    /// The device's own per-image ceiling for attachments sent across the
    /// internet, in bytes.
    pub fn client_remote_attachment_limit_bytes(&self) -> u64 {
        self.client_preferences
            .remote_attachment_limit_mib
            .map_or(tcode_core::attachments::DEFAULT_REMOTE_BYTES, |mib| {
                u64::from(mib) * 1024 * 1024
            })
    }

    pub fn client_remote_attachment_limit_override(&self) -> Option<u32> {
        self.client_preferences.remote_attachment_limit_mib
    }

    pub fn set_client_remote_attachment_limit_mib(&mut self, limit: Option<u32>) {
        self.client_preferences.remote_attachment_limit_mib = limit.filter(|limit| *limit > 0);
        self.save_client_preferences();
    }

    /// How an attachment from this client would reach the machine right now.
    pub fn attachment_link(&self) -> crate::attachments::TransferLink {
        use crate::attachments::TransferLink;
        if !self.is_remote() {
            return TransferLink::Local;
        }
        match self.connection_state() {
            tcode_client::ConnectionState::Connected { path }
            | tcode_client::ConnectionState::Syncing { path } => {
                path.map_or(TransferLink::Unknown, |path| match path.kind() {
                    tcode_protocol::PathKind::Lan => TransferLink::Lan,
                    tcode_protocol::PathKind::Tunnel => TransferLink::Tunnel,
                    tcode_protocol::PathKind::Relay { .. } => TransferLink::Relay,
                })
            }
            _ => TransferLink::Unknown,
        }
    }

    pub fn reset_client_preferences(&mut self) {
        self.client_preferences = ClientPreferences::default();
        self.save_client_preferences();
    }

    fn save_client_preferences(&self) {
        if let Some(host) = &self.client_host {
            let mut preferences = self.client_preferences.clone();
            // Navigation is written by the shell while this store is alive.
            // Appearance edits must not replace it with our startup snapshot.
            preferences.navigation = host.load_preferences().navigation;
            host.save_preferences(&preferences);
        }
    }

    pub fn live_command_panel(&self) -> bool {
        !self.settings_replica.live_command_panel_disabled
    }

    /// The `0xRRGGBB` color of `meta`'s sidebar provider mark, `None` while
    /// the Provider marks setting is off.
    pub fn provider_color(&self, meta: &SessionMeta) -> Option<u32> {
        self.settings_replica.sidebar_provider_marks.then(|| {
            self.settings_replica
                .provider_color(&meta.provider_color_key())
        })
    }

    pub fn archived_groups(&self) -> Vec<ProjectGroup> {
        let archived = self
            .archived_replica
            .as_ref()
            .map(|archived| archived.sessions.as_slice())
            .unwrap_or_default();
        let mut groups = group_sessions(
            &self.index_replica.1,
            archived,
            self.settings_replica.project_sort,
        );
        for group in &mut groups {
            group
                .sessions
                .sort_by_key(|meta| std::cmp::Reverse(meta.archived_at));
        }
        groups.retain(|group| !group.sessions.is_empty());
        groups
    }

    pub fn project_sort(&self) -> ProjectSort {
        self.settings_replica.project_sort
    }

    pub fn sidebar_layout(&self) -> SidebarLayout {
        self.settings_replica.sidebar_layout
    }

    pub fn flat_sessions(&self) -> Vec<SessionMeta> {
        let mut visible: Vec<_> = self
            .index_replica
            .0
            .iter()
            .filter(|meta| tcode_core::thread_sort::in_roster(meta))
            .cloned()
            .collect();
        tcode_core::thread_sort::sort_threads(&mut visible);
        visible
    }

    pub(crate) fn project(&self, id: &str) -> Option<&Project> {
        self.index_replica.1.iter().find(|project| project.id == id)
    }

    pub fn projects(&self) -> Vec<Project> {
        self.index_replica
            .1
            .iter()
            .filter(|project| !project.quick_chat)
            .cloned()
            .collect()
    }

    pub fn is_project_collapsed(&self, project_id: &str) -> bool {
        self.settings_replica
            .collapsed_projects
            .iter()
            .any(|id| id == project_id)
    }

    pub fn active_session_id(&self) -> Option<String> {
        self.selected_session_id.clone()
    }

    /// The threads-list row of the selected thread: the thread itself, or
    /// for an agent, the lead it was reached from.
    pub fn roster_session_id(&self) -> Option<String> {
        let mut id = self.selected_session_id.clone()?;
        for _ in 0..self.index_replica.0.len() {
            match self
                .index_replica
                .0
                .iter()
                .find(|meta| meta.id == id)
                .and_then(|meta| meta.parent_session_id.clone())
            {
                Some(parent) => id = parent,
                None => break,
            }
        }
        Some(id)
    }

    pub fn turn_running_for(&self, session_id: &str) -> bool {
        self.index_summary
            .activity
            .get(session_id)
            .is_some_and(|activity| activity.working)
    }

    pub fn failed_for(&self, session_id: &str) -> bool {
        self.index_summary
            .activity
            .get(session_id)
            .is_some_and(|activity| activity.failed)
    }

    /// A dispatched agent's execution and delivery, as its host reports them.
    pub fn agent_status(&self, session_id: &str) -> Option<tcode_protocol::AgentStatus> {
        self.index_summary
            .activity
            .get(session_id)
            .and_then(|activity| activity.agent)
    }

    pub fn waiting_for(&self, session_id: &str) -> bool {
        self.index_summary
            .activity
            .get(session_id)
            .is_some_and(|activity| activity.waiting)
    }

    pub fn session_unread(&self, session_id: &str) -> bool {
        if !self.scope.is_full() {
            return self
                .index_replica
                .0
                .iter()
                .find(|meta| meta.id == session_id)
                .is_some_and(|meta| {
                    !meta.is_settled()
                        && self
                            .settings_replica
                            .last_visited
                            .get(session_id)
                            .copied()
                            .unwrap_or(0)
                            < meta.updated_at
                });
        }
        self.index_summary
            .activity
            .get(session_id)
            .is_some_and(|activity| activity.unread)
    }

    pub fn pending_approval_for(&self, session_id: &str) -> bool {
        self.index_summary
            .activity
            .get(session_id)
            .is_some_and(|activity| activity.waiting_for_approval)
    }

    pub fn pending_user_input_for(&self, session_id: &str) -> bool {
        self.index_summary
            .activity
            .get(session_id)
            .is_some_and(|activity| activity.waiting_for_input)
    }

    pub fn fork_availability(&self, session_id: &str) -> ForkAvailability {
        self.index_summary
            .activity
            .get(session_id)
            .map_or(ForkAvailability::Available, |activity| activity.fork)
    }

    pub fn sidebar_sessions(&self) -> Vec<SessionMeta> {
        self.index_replica.0.clone()
    }

    pub fn settings_installed_acp_agents(&self) -> Vec<tcode_core::acp::InstalledAcpAgent> {
        self.settings_replica
            .installed_acp_agents()
            .into_iter()
            .cloned()
            .collect()
    }

    pub fn providers_checked_at(&self) -> Option<u64> {
        self.providers_replica.providers_checked_at
    }

    pub fn providers_checking(&self) -> bool {
        self.providers_replica.providers_checking
    }

    /// The latest usage fetch for a provider profile, when one has landed.
    pub fn provider_usage(&self, profile_id: &str) -> Option<tcode_core::usage::ProviderUsage> {
        self.providers_replica
            .provider_usage
            .get(profile_id)
            .cloned()
    }

    /// A usage fetch is in flight for this profile.
    pub fn usage_checking(&self, profile_id: &str) -> bool {
        self.providers_replica.usage_checking.contains(profile_id)
    }

    pub fn window_caption_state(&self) -> (bool, tcode_core::ui::RightTab) {
        self.active_conversation_ui()
            .map(|ui| (ui.right_panel_open, ui.right_tab))
            .unwrap_or((false, RightTab::default()))
    }

    pub fn shell_window_title(&self) -> String {
        if !self.scope.is_full() {
            return self.machine_label();
        }
        match self.session_status_replica.as_ref() {
            Some(status) if status.draft => crate::tr!("chat.new_thread").into_owned(),
            Some(status) => status.title.clone(),
            None => crate::tr!("app.name").into_owned(),
        }
    }

    pub fn preview_active_identity(&self) -> Option<(String, String)> {
        self.session_status_replica.as_ref().map(|status| {
            (
                status.session_id.clone(),
                Self::destination(status).preference_key(),
            )
        })
    }

    pub(crate) fn preview_live_keys(&self) -> HashSet<String> {
        let mut keys = self
            .index_replica
            .0
            .iter()
            .map(|session| session.id.clone())
            .collect::<HashSet<_>>();
        if let Some(destination) = &self.active_destination {
            keys.insert(destination.preference_key());
        }
        keys
    }

    pub fn preview_panel_showing(&self) -> bool {
        self.active_conversation_ui()
            .is_some_and(|ui| ui.right_panel_open && ui.right_tab == RightTab::Preview)
    }

    pub fn preview_browser_settings(&self) -> BrowserSettings {
        self.settings_replica.browser.clone()
    }

    pub fn plugin_management(&self) -> &tcode_core::settings::PluginManagementSettings {
        &self.settings_replica.plugins
    }

    pub fn provider_profile_kind(&self, profile_id: &str) -> agent::ProviderKind {
        if let Some(choice) = self.scoped_choice(profile_id) {
            return choice.provider;
        }
        self.settings_replica
            .resolved_profile(profile_id)
            .map(|profile| profile.kind)
            .unwrap_or(agent::ProviderKind::ClaudeCode)
    }

    pub fn provider_profile_settings(&self, profile_id: &str) -> ProviderSettings {
        self.settings_replica
            .resolved_profile(profile_id)
            .map(|profile| profile.settings)
            .unwrap_or_default()
    }

    pub fn provider_model_catalog(&self, provider: agent::ProviderKind) -> Vec<agent::ModelSpec> {
        self.providers_replica
            .model_catalogs
            .get(&provider)
            .cloned()
            .unwrap_or_default()
    }

    pub(crate) fn models_loading(&self, provider: agent::ProviderKind) -> bool {
        self.providers_replica.models_loading.get(&provider) == Some(&true)
            && self
                .providers_replica
                .model_catalogs
                .get(&provider)
                .is_none_or(Vec::is_empty)
    }

    pub fn picker_models_for_profile(&self, profile_id: &str) -> Vec<ResolvedModel> {
        picker_models(
            &self.profile_catalog(profile_id),
            &self.provider_profile_settings(profile_id),
            &self.settings_replica.favorite_models,
        )
    }

    pub fn provider_profile_display_name(&self, profile_id: &str) -> String {
        if let Some(choice) = self.scoped_choice(profile_id) {
            return choice.name.clone();
        }
        self.settings_replica.profile_display_name(profile_id)
    }

    pub fn provider_profile_snapshot(&self, profile_id: &str) -> Option<ProviderSnapshot> {
        self.providers_replica
            .provider_snapshots
            .get(profile_id)
            .cloned()
    }

    pub fn provider_version_status(
        &self,
        provider: agent::ProviderKind,
    ) -> Option<ProviderVersionStatus> {
        self.providers_replica
            .provider_versions
            .get(&provider)
            .cloned()
    }

    pub fn provider_update_run(&self) -> Option<tcode_protocol::ProviderUpdateRun> {
        self.providers_replica.update_run.clone()
    }

    pub fn automatic_provider_updates(&self) -> Vec<agent::ProviderKind> {
        agent::ProviderKind::NATIVE
            .into_iter()
            .filter(|provider| {
                self.providers_replica
                    .provider_versions
                    .get(provider)
                    .is_some_and(|status| {
                        status.update_available
                            && status.update_command.is_some()
                            && !status.update_requires_terminal
                    })
            })
            .collect()
    }

    pub fn tcode_update_status(&self) -> tcode_protocol::TcodeUpdateStatus {
        self.providers_replica.tcode_update.clone()
    }

    pub fn provider_profile_accent(&self, profile_id: &str) -> Option<u32> {
        self.settings_replica
            .resolved_profile(profile_id)?
            .settings
            .accent_rgb()
    }

    pub fn provider_profile_stored_secret_names(&self, profile_id: &str) -> HashSet<String> {
        self.providers_replica
            .secret_names
            .get(profile_id)
            .cloned()
            .unwrap_or_default()
    }

    pub fn provider_dialog_models(
        &self,
        profile_id: &str,
        custom_models: &[String],
        hidden_models: &[String],
    ) -> Vec<ResolvedModel> {
        let mut settings = self.provider_profile_settings(profile_id);
        settings.custom_models = custom_models.to_vec();
        settings.hidden_models = hidden_models.to_vec();
        resolve_models(
            &self.profile_catalog(profile_id),
            &settings,
            &self.settings_replica.favorite_models,
        )
    }

    pub fn installed_acp_agent(
        &self,
        agent_id: &str,
    ) -> Option<tcode_core::acp::InstalledAcpAgent> {
        self.settings_replica.acp_agent(agent_id).cloned()
    }

    pub fn acp_marketplace_items(&self) -> Vec<AcpMarketplaceItem> {
        self.providers_replica.acp_marketplace_items.clone()
    }

    pub fn provider_plugin_catalog(
        &self,
        profile_id: &str,
    ) -> Option<&tcode_protocol::ProviderPluginCatalog> {
        self.providers_replica
            .plugins
            .iter()
            .find(|catalog| catalog.profile_id == profile_id)
    }

    /// The working directory of the thread or draft the window has open.
    pub fn active_session_cwd(&self) -> Option<PathBuf> {
        self.session_status_replica
            .as_ref()
            .map(|status| status.cwd.clone())
    }

    pub fn acp_registry_loading(&self) -> bool {
        self.providers_replica.acp_registry_loading
    }

    pub fn acp_registry_error(&self) -> Option<String> {
        self.providers_replica.acp_registry_error.clone()
    }

    pub fn acp_installing(&self, agent_id: &str) -> bool {
        self.providers_replica.acp_installing.contains(agent_id)
    }

    pub fn project_ids(&self) -> Vec<String> {
        self.index_replica
            .1
            .iter()
            .map(|project| project.id.clone())
            .collect()
    }

    pub fn project_summary(&self, project_id: &str) -> Option<(String, usize)> {
        let project = self
            .index_replica
            .1
            .iter()
            .find(|project| project.id == project_id)?;
        let count = self
            .index_replica
            .0
            .iter()
            .filter(|meta| meta.project_id.as_deref() == Some(project_id))
            .count()
            + self
                .index_summary
                .archived_counts
                .get(project_id)
                .copied()
                .unwrap_or_default();
        Some((project.name.clone(), count))
    }

    pub fn project_root(&self, project_id: &str) -> Option<PathBuf> {
        self.index_replica
            .1
            .iter()
            .find(|project| project.id == project_id)
            .map(|project| project.root.clone())
    }

    /// Scan the *host's* external-agent histories. A failure is returned rather
    /// than logged away: an empty list and a broken host look identical to the
    /// user otherwise.
    pub fn scan_external_history(&self, cx: &mut App) -> Task<Result<Vec<RecentDir>, String>> {
        if !self.can_manage_host() {
            return cx.spawn(async |_| Err(crate::tr!("member.unavailable").into_owned()));
        }
        let host = self.host.clone();
        cx.spawn(
            async move |_| match host.query(Query::ScanExternalHistory).await {
                Ok(QueryResponse::ExternalHistory(recent)) => Ok(recent),
                Ok(other) => Err(format!("unexpected external-history response: {other:?}")),
                Err(error) => Err(error.message),
            },
        )
    }

    /// Subscribe to a project's import status. Callers must do this *before*
    /// starting a run: a fast completion is only recoverable through the
    /// subscription snapshot, not through the start reply.
    pub fn watch_external_import(&self, project_id: &str) {
        if !self.scope.is_full() || !self.baseline_topics.contains(&Topic::Scope) {
            return;
        }
        if let Err(error) = self.host.subscribe(Subscription {
            known_index: None,
            after: None,
            topic: Topic::ExternalImport {
                project_id: project_id.to_string(),
            },
        }) {
            log::error!("failed to watch import status: {}", error.message);
        }
    }

    pub fn unwatch_external_import(&mut self, project_id: &str) {
        let _ = self.host.unsubscribe(Subscription {
            known_index: None,
            after: None,
            topic: Topic::ExternalImport {
                project_id: project_id.to_string(),
            },
        });
        self.import_statuses.remove(project_id);
    }

    /// The latest host-published status for a project, or `None` while the
    /// snapshot is still in flight or no run has ever started.
    pub fn external_import_status(&self, project_id: &str) -> Option<&ExternalImportStatus> {
        self.import_statuses.get(project_id)?.as_ref()
    }

    pub fn start_external_import(
        &self,
        project_id: &str,
        threads: Vec<ExternalThread>,
        cx: &mut App,
    ) -> Task<Result<CommandResponse, ProtocolError>> {
        self.command(
            tcode_protocol::Command::StartExternalImport {
                project_id: project_id.to_string(),
                threads,
            },
            cx,
        )
    }

    /// Ask the host to search its own stored sessions. The index, cache and
    /// session order live there; this client keeps only the answer.
    pub fn search_session_content(
        &self,
        query: String,
        limit: u32,
        cx: &mut App,
    ) -> Task<Vec<SessionSearchHit>> {
        let host = self.host.clone();
        cx.spawn(async move |_| {
            match host
                .query(Query::SearchSessionContent { query, limit })
                .await
            {
                Ok(QueryResponse::SessionContentHits(hits)) => hits,
                Ok(other) => {
                    log::error!("unexpected session-content response: {other:?}");
                    Vec::new()
                }
                Err(error) => {
                    log::error!("session-content query failed: {}", error.message);
                    Vec::new()
                }
            }
        })
    }

    pub(crate) fn commit_dialog_state(&self) -> CommitDialogState {
        CommitDialogState {
            files: self
                .git_status_replica
                .status
                .as_ref()
                .map(|status| status.changed_files.clone())
                .unwrap_or_default(),
            branch: self
                .git_status_replica
                .status
                .as_ref()
                .and_then(|status| status.branch.clone()),
            on_default_branch: self
                .git_status_replica
                .status
                .as_ref()
                .is_some_and(|status| status.is_default_branch),
        }
    }

    pub(crate) fn diff_active_state(&self) -> Option<DiffActiveState> {
        self.session_status_replica
            .as_ref()
            .map(|status| DiffActiveState {
                session: status.session_id.clone(),
                cwd: status.cwd.clone(),
                branches: status.branches.clone(),
            })
    }

    pub fn diff_turns(&self) -> Vec<usize> {
        self.with_active_timeline(|timeline| {
            timeline
                .turns
                .iter()
                .enumerate()
                .filter_map(|(turn, meta)| {
                    meta.changes
                        .as_ref()
                        .is_some_and(|changes| !changes.changes.is_empty())
                        .then_some(turn)
                })
                .collect()
        })
        .unwrap_or_default()
    }

    pub fn diff_selected_turn(&self) -> Option<usize> {
        let turns = self.diff_turns();
        let explicit = self
            .active_conversation_ui()
            .and_then(|ui| ui.diff_selected_turn);
        match explicit {
            Some(turn) if turns.contains(&turn) => Some(turn),
            _ => turns.last().copied(),
        }
    }

    pub fn with_diff_turn_changes<R>(
        &self,
        turn: usize,
        read: impl FnOnce(&[agent::FileChange], agent::ChangeCompleteness) -> R,
    ) -> Option<R> {
        self.with_active_timeline(|timeline| {
            let changes = timeline.turns.get(turn)?.changes.as_ref()?;
            Some(read(&changes.changes, changes.completeness))
        })
        .flatten()
    }

    pub(crate) fn pending_diff_focus(&self) -> Option<DiffFocus> {
        self.active_conversation_ui()
            .and_then(|ui| ui.pending_diff_focus.clone())
    }

    /// UI-only consuming selector. The underlying diff focus is replica state;
    /// this does not cross the host boundary.
    pub(crate) fn take_diff_focus(&mut self, session: &str, turn: usize) -> Option<DiffFocus> {
        self.active_conversation_ui_mut()?
            .take_diff_focus(session, turn)
    }

    pub fn diff_refresh_generation(&self) -> u64 {
        self.active_conversation_ui()
            .map(|ui| ui.diff_refresh_generation)
            .unwrap_or(0)
    }

    pub fn diff_word_wrap(&self) -> bool {
        self.active_conversation_ui()
            .map(|ui| ui.diff_wrap)
            .unwrap_or(self.settings_replica.word_wrap_diffs)
    }

    pub fn diff_split(&self) -> bool {
        self.active_conversation_ui()
            .is_some_and(|ui| ui.diff_split)
    }

    pub fn set_diff_split(&mut self, split: bool, cx: &mut Context<Self>) {
        if let Some(ui) = self.active_conversation_ui_mut() {
            ui.diff_split = split;
            cx.notify();
        }
    }

    pub fn toggle_diff_wrap(&mut self, cx: &mut Context<Self>) {
        if let Some(ui) = self.active_conversation_ui_mut() {
            ui.diff_wrap = !ui.diff_wrap;
            cx.notify();
        }
    }

    pub(crate) fn panel_state(&self) -> PanelState {
        snapshots::panel_state(
            self.active_conversation_ui(),
            self.session_replica.as_ref().map(|(_, timeline)| timeline),
        )
    }

    pub fn review_comments(&self) -> Vec<ReviewComment> {
        self.session_status_replica
            .as_ref()
            .map(|status| status.review_comment_drafts.clone())
            .unwrap_or_default()
    }

    pub fn load_git_diff(
        &self,
        cwd: &std::path::Path,
        scope: GitDiffScope,
        base: Option<&str>,
        ignore_whitespace: bool,
        cx: &mut App,
    ) -> Task<GitDiffResult> {
        let host = self.host.clone();
        let query = Query::LoadGitDiff {
            cwd: cwd.to_path_buf(),
            scope,
            base: base.map(str::to_string),
            ignore_whitespace,
        };
        cx.spawn(async move |_| match host.query(query).await {
            Ok(QueryResponse::GitDiff(diff)) => diff,
            Ok(other) => GitDiffResult {
                error: Some(format!("unexpected git-diff response: {other:?}")),
                ..GitDiffResult::default()
            },
            Err(error) => GitDiffResult {
                error: Some(error.message),
                ..GitDiffResult::default()
            },
        })
    }

    /// Ask the host to render a stored thread into transferable bytes. Nothing
    /// is written anywhere: the host owns rendering and its store-flush barrier,
    /// this client owns where the artifact goes.
    pub fn render_thread_export(
        &self,
        session_id: String,
        format: tcode_protocol::ThreadExportFormat,
        cx: &mut App,
    ) -> Task<Result<ThreadExportArtifact, String>> {
        let host = self.host.clone();
        cx.spawn(async move |_| {
            match host
                .query(Query::RenderThreadExport { session_id, format })
                .await
            {
                Ok(QueryResponse::ThreadExport {
                    bytes,
                    suggested_name,
                    mime,
                }) => Ok(ThreadExportArtifact {
                    bytes,
                    suggested_name,
                    mime,
                }),
                Ok(other) => Err(format!("unexpected thread-export response: {other:?}")),
                Err(error) => Err(error.message),
            }
        })
    }

    /// The whole output of an item whose history record carried a preview.
    pub fn read_item_output(
        &self,
        session_id: String,
        item_id: String,
        cx: &mut App,
    ) -> Task<Result<String, String>> {
        let host = self.host.clone();
        cx.spawn(async move |_| {
            match host
                .query(Query::ReadItemOutput {
                    session_id,
                    item_id,
                })
                .await
            {
                Ok(QueryResponse::ItemOutput(output)) => Ok(output),
                Ok(other) => Err(format!("unexpected item-output response: {other:?}")),
                Err(error) => Err(error.message),
            }
        })
    }

    pub fn computer_use_permissions(
        &self,
        cx: &mut App,
    ) -> Task<Result<tcode_core::permissions::ComputerUsePermissions, String>> {
        if !self.scope.is_full() || !self.baseline_topics.contains(&Topic::Scope) {
            return cx.spawn(async |_| Err(crate::tr!("member.unavailable").into_owned()));
        }
        let host = self.host.clone();
        cx.spawn(
            async move |_| match host.query(Query::ComputerUsePermissions).await {
                Ok(QueryResponse::ComputerUsePermissions(status)) => Ok(status),
                Ok(_) => Err("unexpected computer-use permissions response".into()),
                Err(error) => Err(error.message),
            },
        )
    }

    pub fn hosting(
        &self,
        action: tcode_protocol::HostingAction,
        cx: &mut App,
    ) -> Task<Result<tcode_protocol::HostingState, String>> {
        if !self.scope.is_full() || !self.baseline_topics.contains(&Topic::Scope) {
            return cx.spawn(async |_| Err(crate::tr!("member.unavailable").into_owned()));
        }
        let host = self.host.clone();
        cx.spawn(
            async move |_| match host.query(Query::Hosting { action }).await {
                Ok(QueryResponse::Hosting(state)) => Ok(state),
                Ok(_) => Err("unexpected hosting response".into()),
                Err(error) => Err(error.message),
            },
        )
    }

    /// The latest `tcode_control` writes across every thread, newest first.
    pub fn control_activity(
        &self,
        limit: u32,
        cx: &mut App,
    ) -> Task<Result<Vec<tcode_protocol::ControlActivityEntry>, String>> {
        if !self.scope.is_full() || !self.baseline_topics.contains(&Topic::Scope) {
            return cx.spawn(async |_| Err(crate::tr!("member.unavailable").into_owned()));
        }
        let host = self.host.clone();
        cx.spawn(
            async move |_| match host.query(Query::ControlActivity { limit }).await {
                Ok(QueryResponse::ControlActivity(entries)) => Ok(entries),
                Ok(_) => Err("unexpected control activity response".into()),
                Err(error) => Err(error.message),
            },
        )
    }

    pub fn read_file_preview(&self, path: PathBuf, cx: &mut App) -> Task<std::io::Result<Vec<u8>>> {
        let host = self.host.clone();
        cx.spawn(
            async move |_| match host.query(Query::ReadFilePreview { path }).await {
                Ok(QueryResponse::FileBytes(bytes)) => Ok(bytes),
                Ok(other) => Err(protocol_io_error(format!(
                    "unexpected file-bytes response: {other:?}"
                ))),
                Err(error) => Err(protocol_io_error(error.message)),
            },
        )
    }

    pub fn read_pull_request(
        &self,
        session_id: String,
        key: tcode_core::pull_request::PullRequestKey,
        read: tcode_protocol::PullRequestRead,
        cx: &mut App,
    ) -> Task<Result<(tcode_protocol::PullRequestReadResponse, u64), ProtocolError>> {
        let host = self.host.clone();
        cx.spawn(async move |_| {
            match host
                .query(Query::PullRequest {
                    session_id,
                    key,
                    read,
                })
                .await?
            {
                QueryResponse::PullRequest {
                    response,
                    expires_at,
                } => Ok((*response, expires_at)),
                _ => Err(ProtocolError {
                    code: "invalid_pull_request_response".into(),
                    message: "Unexpected pull request response".into(),
                }),
            }
        })
    }

    pub(crate) fn message_byline(&self, entry_id: &str) -> Option<String> {
        let held = self
            .threads
            .get(self.selected_session_id.as_ref()?)?
            .history
            .as_ref()?;
        let attribution = &held.attribution;
        // The lead's messages to a child carry no author: they are no member's.
        if attribution.origins.get(entry_id) == Some(&tcode_core::session::MessageOrigin::Agent) {
            return Some(crate::tr!("chat.from_orchestrator").into_owned());
        }
        let author = attribution.authors.get(entry_id)?;
        let own_id = self.client_host.as_ref().map(|host| host.device_id());
        // The host attributes only space members' messages. An unattributed
        // one is the owner's, which is someone else only from inside a space.
        let other = author.as_ref().map_or(!self.scope.is_full(), |author| {
            own_id.as_deref() != Some(author.device_id.as_str())
        });
        (attribution.distinct_authors.len() > 1 || other).then(|| {
            author
                .as_ref()
                .map(|author| author.name.clone())
                .unwrap_or_else(|| crate::tr!("member.owner").into_owned())
        })
    }

    pub fn with_active_timeline<R>(&self, read: impl FnOnce(&Timeline) -> R) -> Option<R> {
        self.session_replica
            .as_ref()
            .map(|(_, timeline)| read(timeline))
    }

    pub(crate) fn pending_chat_turn(&self, session_id: &str) -> Option<usize> {
        self.pending_chat_turn
            .as_ref()
            .filter(|(id, _)| id == session_id)
            .and_then(|(_, turn)| turn.checked_sub(self.session_turn_offset))
    }

    pub(crate) fn take_pending_chat_turn(&mut self, session_id: &str, turn: usize) {
        if self.pending_chat_turn.as_ref()
            == Some(&(session_id.to_string(), turn + self.session_turn_offset))
        {
            self.pending_chat_turn = None;
        }
    }

    #[cfg(test)]
    pub(crate) fn set_session_replica_for_test(
        &mut self,
        session_id: String,
        timeline: Timeline,
        _cx: &mut Context<Self>,
    ) {
        if self.selected_session_id.as_ref() != Some(&session_id) {
            self.select_session(session_id.clone());
        }
        self.session_replica = Some((session_id, timeline));
    }

    pub fn with_composer_destination<R>(
        &self,
        read: impl FnOnce(bool, &str, Option<&str>) -> R,
    ) -> Option<R> {
        self.session_status_replica.as_ref().map(|status| {
            read(
                status.draft,
                &status.session_id,
                status.project_id.as_deref(),
            )
        })
    }

    pub(crate) fn native_subagent_readonly(&self) -> bool {
        self.session_status_replica
            .as_ref()
            .is_some_and(|status| status.conversation_read_only)
    }

    pub fn composer_state(&self) -> ComposerState {
        snapshots::composer_state(
            self.session_status_replica.as_ref(),
            self.session_replica.as_ref().map(|(_, timeline)| timeline),
            &self.settings_replica,
            &self.providers_replica,
        )
    }

    /// Puts `text` into the thread's composer after whatever is typed there; nothing is sent.
    pub fn append_to_composer(&mut self, session_id: String, text: String, cx: &mut Context<Self>) {
        let pending = self.composer_appends.entry(session_id).or_default();
        if !pending.is_empty() {
            pending.push_str("\n\n");
        }
        pending.push_str(&text);
        cx.emit(StoreChange {
            topic: TopicKind::SessionStatus,
        });
        cx.notify();
    }

    /// Consume the active session's text from [`Self::append_to_composer`].
    pub fn take_composer_append(&mut self) -> Option<String> {
        let active_id = self.session_status_replica.as_ref()?.session_id.clone();
        self.composer_appends.remove(&active_id)
    }

    /// Consume the active session's prefill delivered by `NativeRewindPrefill`.
    pub fn take_native_rewind_prefill(&mut self) -> Option<String> {
        let active_id = self.session_status_replica.as_ref()?.session_id.clone();
        let prefill = self.native_rewind_prefills.remove(&active_id)?;
        if let Some(status) = self.session_status_replica.as_mut() {
            status.native_rewind_prefill_available = false;
        }
        Some(prefill)
    }

    /// The classifier stop the active session is currently showing, if any.
    pub fn active_fallback_block(&self) -> Option<&FallbackBlock> {
        let status = self.session_status_replica.as_ref()?;
        self.fallback_blocks.get(&status.session_id)
    }

    pub fn dismiss_fallback_block(&mut self) {
        if let Some(status) = self.session_status_replica.as_ref() {
            self.fallback_blocks.remove(&status.session_id);
        }
    }

    /// The advisory review of the active session's classifier stop, if any.
    pub fn active_fallback_review(&self) -> Option<&FallbackReview> {
        let status = self.session_status_replica.as_ref()?;
        self.fallback_reviews.get(&status.session_id)
    }

    pub fn dismiss_fallback_review(&mut self) {
        if let Some(status) = self.session_status_replica.as_ref() {
            self.fallback_reviews.remove(&status.session_id);
        }
    }

    /// The active session's last user message: its turn index and the words the
    /// user actually typed (any injected context prefix stripped).
    pub fn last_user_message(&self) -> Option<(usize, String)> {
        self.with_active_timeline(|timeline| {
            timeline.entries.iter().rev().find_map(|entry| {
                let EntryContent::Item(agent::ItemContent::UserMessage {
                    text, context_len, ..
                }) = &entry.content
                else {
                    return None;
                };
                let visible = context_len
                    .filter(|len| *len <= text.len() && text.is_char_boundary(*len))
                    .map_or(text.as_str(), |len| &text[len..]);
                Some((entry.turn, visible.to_string()))
            })
        })
        .flatten()
    }

    /// Build renderer handles from replicated layout. Local affordances keep
    /// direct PTY/grid access; otherwise the handles wrap client emulators.
    pub fn with_terminal_workspace<R>(
        &self,
        read: impl FnOnce(&TerminalWorkspace) -> R,
    ) -> Option<R> {
        let status = self.session_status_replica.as_ref()?;
        let workspace = TerminalWorkspace::from_replica(status, |id| self.client_terminal(id));
        Some(read(&workspace))
    }

    pub(crate) fn browse_icon_images(
        &self,
        directory: PathBuf,
        cx: &mut App,
    ) -> Task<Result<QueryResponse, ProtocolError>> {
        let host = self.host.clone();
        cx.spawn(async move |_| host.query(Query::BrowseIconImages { directory }).await)
    }

    pub(crate) fn set_project_icon(
        &self,
        project_id: String,
        path: Option<PathBuf>,
        cx: &mut App,
    ) -> Task<Result<tcode_protocol::CommandResponse, ProtocolError>> {
        if !self.scope.is_full() || !self.baseline_topics.contains(&Topic::Scope) {
            return cx.spawn(async |_| {
                Err(ProtocolError::out_of_scope(
                    "project icons are managed by the space owner",
                ))
            });
        }
        let host = self.host.clone();
        cx.spawn(async move |_| {
            let png = if let Some(path) = path {
                match host.query(Query::ReadIconImage { path }).await? {
                    QueryResponse::FileBytes(bytes) => Some(bytes),
                    _ => {
                        return Err(ProtocolError {
                            code: "invalid_image_response".into(),
                            message: "Unexpected image response".into(),
                        });
                    }
                }
            } else {
                None
            };
            host.command(tcode_protocol::Command::SetProjectIcon { project_id, png })
                .await
        })
    }

    pub fn list_active_workspace(&self, cx: &mut App) -> Task<Vec<PathEntry>> {
        let session_id = self.active_session_id().unwrap_or_default();
        let host = self.host.clone();
        cx.spawn(
            async move |_| match host.query(Query::ListActiveWorkspace { session_id }).await {
                Ok(QueryResponse::ActiveWorkspace(entries)) => entries,
                Ok(other) => {
                    log::error!("unexpected active-workspace response: {other:?}");
                    Vec::new()
                }
                Err(error) => {
                    log::error!("active-workspace query failed: {}", error.message);
                    Vec::new()
                }
            },
        )
    }

    pub fn save_attachment_to_dir(
        &self,
        dir: PathBuf,
        bytes: Vec<u8>,
        ext: String,
        cx: &mut App,
    ) -> Task<std::io::Result<PathBuf>> {
        let host = self.host.clone();
        cx.spawn(
            async move |_| match host.query(Query::SaveAttachment { dir, bytes, ext }).await {
                Ok(QueryResponse::SavedAttachment(path)) => Ok(path),
                Ok(other) => Err(protocol_io_error(format!(
                    "unexpected save-attachment response: {other:?}"
                ))),
                Err(error) => Err(protocol_io_error(error.message)),
            },
        )
    }

    pub fn remove_user_file(&self, path: PathBuf, cx: &mut App) -> Task<std::io::Result<()>> {
        let host = self.host.clone();
        cx.spawn(
            async move |_| match host.query(Query::RemoveUserFile { path }).await {
                Ok(QueryResponse::UserFileRemoved) => Ok(()),
                Ok(other) => Err(protocol_io_error(format!(
                    "unexpected remove-file response: {other:?}"
                ))),
                Err(error) => Err(protocol_io_error(error.message)),
            },
        )
    }

    pub(crate) fn session_loading(&self) -> bool {
        self.selected_session_id.is_some()
            && (self.session_replica.is_none() || self.session_status_replica.is_none())
    }

    pub fn chat_active_session(&self) -> Option<(String, PathBuf, bool)> {
        self.session_status_replica
            .as_ref()
            .map(|status| (status.title.clone(), status.cwd.clone(), status.draft))
            .or_else(|| {
                let selected = self.selected_session_id.as_ref()?;
                self.index_replica
                    .0
                    .iter()
                    .find(|meta| &meta.id == selected)
                    .map(|meta| (meta.title.clone(), meta.cwd.clone(), false))
            })
            .or_else(|| {
                (!self.delivery_messages().is_empty()).then(|| {
                    (
                        crate::tr!("chat.waiting_connection").into_owned(),
                        PathBuf::new(),
                        false,
                    )
                })
            })
    }

    pub fn chat_requested_model(&self) -> Option<String> {
        self.session_status_replica
            .as_ref()
            .and_then(|status| status.requested_model.clone())
    }

    pub fn chat_turn_changes(
        &self,
        turn: usize,
    ) -> (Vec<agent::FileChange>, agent::ChangeCompleteness) {
        self.with_active_timeline(|timeline| {
            timeline
                .turns
                .get(turn)
                .and_then(|turn| turn.changes.as_ref())
                .map(|changes| (changes.changes.clone(), changes.completeness))
        })
        .flatten()
        .unwrap_or((Vec::new(), agent::ChangeCompleteness::Partial))
    }

    pub fn chat_native_rewind_state(&self, turn: usize) -> Option<(bool, bool)> {
        let status = self.session_status_replica.as_ref()?;
        let has_checkpoint = self
            .with_active_timeline(|timeline| {
                timeline
                    .turns
                    .get(turn)
                    .and_then(|turn| turn.provider_checkpoint_id.as_ref())
                    .is_some()
            })
            .unwrap_or(false);
        Some((has_checkpoint, status.native_rewind_blocked))
    }

    pub fn chat_git_controls(&self) -> Option<(QuickAction, Vec<MenuItem>)> {
        self.git_status_replica.status.as_ref().map(|status| {
            (
                quick_action(status, self.git_status_replica.busy),
                menu_items(status, self.git_status_replica.busy),
            )
        })
    }

    pub fn generate_commit_message(
        &self,
        included: Option<Vec<String>>,
        cx: &mut App,
    ) -> Task<Result<String, String>> {
        let session_id = self.active_session_id().unwrap_or_default();
        let host = self.host.clone();
        cx.spawn(async move |_| {
            match host
                .query(Query::GenerateCommitMessage {
                    session_id,
                    included,
                })
                .await
            {
                Ok(QueryResponse::CommitMessage(message)) => Ok(message),
                Ok(other) => Err(format!("unexpected commit-message response: {other:?}")),
                Err(error) => Err(error.message),
            }
        })
    }

    pub fn absolute_turn(&self, local: usize) -> usize {
        local + self.session_turn_offset
    }

    pub fn session_plan(&self) -> Option<&SessionPlan> {
        self.threads
            .get(self.selected_session_id.as_deref()?)?
            .plan
            .as_ref()
    }

    pub fn session_status(&self) -> Option<&SessionStatus> {
        self.session_status_replica.as_ref()
    }

    /// How many threads deleting `session_id` removes (the host deletes every
    /// thread under it too), while the archived threads are held: archived
    /// descendants are not in the index.
    pub fn held_deletion_count(&self, session_id: &str) -> Option<usize> {
        let archived = self.archived_replica.as_ref()?;
        Some(self.deletion_count(session_id, &archived.sessions))
    }

    /// [`Self::held_deletion_count`] over the archived threads, fetched once.
    pub fn fetch_deletion_count(&self, session_id: &str, cx: &mut Context<Self>) -> Task<usize> {
        let host = self.host.clone();
        let session_id = session_id.to_string();
        cx.spawn(async move |this, cx| {
            let archived = match host.query(Query::ArchivedSessions).await {
                Ok(QueryResponse::ArchivedSessions(archived)) => archived.sessions,
                Ok(other) => {
                    log::warn!("unexpected archived-sessions response: {other:?}");
                    Vec::new()
                }
                Err(error) => {
                    log::warn!("archived sessions failed: {}", error.message);
                    Vec::new()
                }
            };
            this.read_with(cx, |store, _| store.deletion_count(&session_id, &archived))
                .unwrap_or(1)
        })
    }

    /// What deleting every listed archived thread removes, while the archived
    /// threads are held. Only the archived threads with no archived ancestor
    /// are sent: the host deletes each one's tree, so a thread under one is
    /// neither sent nor counted twice.
    pub fn archived_deletion(&self) -> Option<ArchivedDeletion> {
        let held = self.archived_replica.as_ref()?;
        let listed: HashSet<String> = self
            .archived_groups()
            .into_iter()
            .flat_map(|group| group.sessions.into_iter().map(|meta| meta.id))
            .collect();
        let all = || self.index_replica.0.iter().chain(&held.sessions);
        let parents: HashMap<&str, &str> = all()
            .filter_map(|meta| Some((meta.id.as_str(), meta.parent_session_id.as_deref()?)))
            .collect();
        let under_archived = |id: &str| {
            let mut seen = HashSet::from([id]);
            let mut current = id;
            while let Some(&parent) = parents.get(current) {
                if !seen.insert(parent) {
                    return false;
                }
                if listed.contains(parent) {
                    return true;
                }
                current = parent;
            }
            false
        };
        let roots: Vec<String> = held
            .sessions
            .iter()
            .filter(|meta| listed.contains(&meta.id) && !under_archived(&meta.id))
            .map(|meta| meta.id.clone())
            .collect();
        let deleted: HashSet<String> = roots
            .iter()
            .flat_map(|root| descendant_session_ids(all(), root))
            .collect();
        Some(ArchivedDeletion {
            archived: listed.len(),
            unarchived: deleted.iter().filter(|id| !listed.contains(*id)).count(),
            roots,
        })
    }

    fn deletion_count(&self, session_id: &str, archived: &[SessionMeta]) -> usize {
        descendant_session_ids(self.index_replica.0.iter().chain(archived), session_id)
            .len()
            .max(1)
    }

    pub fn worktree_orphaned_by_delete(&self, session_id: &str) -> Option<WorktreeInfo> {
        let (meta, shared) = if let Some(meta) = self
            .index_replica
            .0
            .iter()
            .find(|meta| meta.id == session_id)
        {
            (meta, &self.index_summary.worktree_shared)
        } else {
            let archived = self.archived_replica.as_ref()?;
            (
                archived
                    .sessions
                    .iter()
                    .find(|meta| meta.id == session_id)?,
                &archived.worktree_shared,
            )
        };
        (!shared.contains(session_id))
            .then(|| meta.worktree.clone())
            .flatten()
    }
}

impl Drop for WorkspaceStore {
    fn drop(&mut self) {
        for subscription in self.host.subscriptions() {
            let _ = self.host.unsubscribe(subscription);
        }
        self.host.close();
    }
}

impl EventEmitter<RuntimeEvent> for WorkspaceStore {}
impl EventEmitter<StoreChange> for WorkspaceStore {}
impl EventEmitter<ConnectionState> for WorkspaceStore {}

#[cfg(test)]
pub(crate) mod tests {
    use agent::{AgentEvent, ItemContent, ProviderKind, ThreadItem, TurnStatus};
    use gpui::{AppContext as _, TestAppContext};
    use tcode_core::{
        git::{GitFileEntry, GitStatus},
        project::{Project, SessionMeta},
        session::{EntryContent, ReviewComment, ReviewSide},
        settings::{Settings, ThemeMode},
    };
    use tcode_protocol::{
        Command, EventEnvelope, ServerEvent, SessionEventRecord, Subscription, Topic,
    };
    use tcode_runtime::host::HostEvent;
    use tcode_runtime::pipe::{HostServices, SpawnedHost, spawn_host};
    use tcode_services::store::SessionStore;

    use super::{
        ConversationDestination, WorkspaceAttachment, WorkspaceStore, effective_client_settings,
        history::HeldHistory,
    };

    pub(crate) fn seed_full_scope(
        store: &gpui::Entity<WorkspaceStore>,
        incoming: &async_channel::Sender<String>,
        deferred: Vec<String>,
        cx: &mut TestAppContext,
    ) {
        seed_full_scope_reply(store, incoming, 1, deferred, cx);
    }

    pub(crate) fn seed_full_scope_reply(
        store: &gpui::Entity<WorkspaceStore>,
        incoming: &async_channel::Sender<String>,
        request_id: u64,
        deferred: Vec<String>,
        cx: &mut TestAppContext,
    ) {
        incoming
            .try_send(
                tcode_protocol::encode_line(&tcode_protocol::HostMessage::Event(EventEnvelope {
                    request_id: Some(request_id),
                    topic: Topic::Scope,
                    event: ServerEvent::ScopeSnapshot(tcode_protocol::Scope::Full),
                }))
                .unwrap(),
            )
            .unwrap();
        let link = store.read_with(cx, |store, _| store.host.clone());
        let mut pump = std::pin::pin!(link.pump());
        let mut task_cx = std::task::Context::from_waker(std::task::Waker::noop());
        assert!(std::future::Future::poll(pump.as_mut(), &mut task_cx).is_pending());
        store.update(cx, |store, cx| store.drain_host_events_for_test(cx));
        for line in deferred {
            incoming.try_send(line).unwrap();
        }
    }

    struct ScriptedSpace {
        link: tcode_client::HostLink,
        scope: std::sync::Arc<std::sync::Mutex<tcode_protocol::Scope>>,
        requests: std::sync::Arc<std::sync::Mutex<Vec<tcode_protocol::ClientMessage>>>,
        worker: Option<std::thread::JoinHandle<()>>,
        driver: LocalHostDriver,
    }

    impl ScriptedSpace {
        fn new() -> Self {
            use tcode_protocol::{ClientPayload, CommandResponse, HostMessage, Scope};
            let (to_host, requests) = async_channel::unbounded();
            let (replies, from_host) = async_channel::unbounded();
            let link = tcode_client::HostLink::new(to_host, from_host);
            let project = project_at("shared", std::path::Path::new("/shared"));
            let scope = std::sync::Arc::new(std::sync::Mutex::new(Scope::Space {
                space_id: "space-one".into(),
                space_name: "Team".into(),
                projects: vec![project],
                providers: vec![tcode_protocol::ScopedProviderChoice {
                    provider: ProviderKind::Codex,
                    profile_id: Some("team-agent".into()),
                    name: "Team agent".into(),
                    models: vec![agent::ModelSpec {
                        id: "team-model".into(),
                        display_name: "Team model".into(),
                        is_default: true,
                        options: Vec::new(),
                    }],
                }],
            }));
            let recorded = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
            let worker_scope = scope.clone();
            let worker_recorded = recorded.clone();
            let worker = std::thread::spawn(move || {
                smol::block_on(async move {
                    while let Ok(line) = requests.recv().await {
                        let request = tcode_protocol::decode_client_line(&line).unwrap();
                        worker_recorded.lock().unwrap().push(request.clone());
                        let scope = worker_scope.lock().unwrap().clone();
                        let response = match request.payload {
                            ClientPayload::Subscribe(subscription)
                                if matches!(
                                    subscription.topic,
                                    Topic::RuntimeEvents | Topic::Preview
                                ) && scope == Scope::Full =>
                            {
                                HostMessage::Ack {
                                    id: request.id,
                                    result: Ok(CommandResponse::Unit),
                                }
                            }
                            ClientPayload::Subscribe(subscription) => {
                                let event = match (&subscription.topic, &scope) {
                                    (Topic::Scope, _) => ServerEvent::ScopeSnapshot(scope.clone()),
                                    (
                                        Topic::SpaceIndex { space_id },
                                        Scope::Space {
                                            space_id: scoped_id,
                                            projects,
                                            ..
                                        },
                                    ) if space_id == scoped_id => {
                                        let sessions = projects
                                            .first()
                                            .map(|project| {
                                                let mut session = thread(
                                                    &project.root,
                                                    "shared-thread",
                                                    &project.id,
                                                    None,
                                                );
                                                session.updated_at = 100;
                                                session
                                            })
                                            .into_iter()
                                            .collect();
                                        ServerEvent::IndexSnapshot(tcode_protocol::IndexSnapshot {
                                            projects: projects.clone(),
                                            sessions,
                                            summary: Default::default(),
                                        })
                                    }
                                    (Topic::Index, Scope::Full) => {
                                        ServerEvent::IndexSnapshot(tcode_protocol::IndexSnapshot {
                                            projects: vec![],
                                            sessions: vec![],
                                            summary: Default::default(),
                                        })
                                    }
                                    (Topic::Settings, Scope::Full) => {
                                        ServerEvent::SettingsSnapshot(Settings::default())
                                    }
                                    (Topic::Providers, Scope::Full) => {
                                        ServerEvent::ProvidersReplaced(
                                            tcode_protocol::ProvidersStatus::default(),
                                        )
                                    }
                                    _ => {
                                        let rejection = HostMessage::Ack {
                                            id: request.id,
                                            result: Err(
                                                tcode_protocol::ProtocolError::out_of_scope(
                                                    "subscription outside current space",
                                                ),
                                            ),
                                        };
                                        if replies
                                            .send(tcode_protocol::encode_line(&rejection).unwrap())
                                            .await
                                            .is_err()
                                        {
                                            break;
                                        }
                                        continue;
                                    }
                                };
                                HostMessage::Event(EventEnvelope {
                                    request_id: Some(request.id),
                                    topic: subscription.topic,
                                    event,
                                })
                            }
                            ClientPayload::Command(Command::StartDraft { .. }) => {
                                HostMessage::Ack {
                                    id: request.id,
                                    result: Ok(CommandResponse::SessionId(None)),
                                }
                            }
                            ClientPayload::Command(_) => HostMessage::Ack {
                                id: request.id,
                                result: Ok(CommandResponse::Unit),
                            },
                            ClientPayload::Query(tcode_protocol::Query::Hosting { action }) => {
                                let created = matches!(
                                    action,
                                    tcode_protocol::HostingAction::Spaces(
                                        tcode_protocol::SpaceAction::Create { .. }
                                    )
                                );
                                let shared = matches!(
                                    action,
                                    tcode_protocol::HostingAction::Spaces(
                                        tcode_protocol::SpaceAction::SetProjects { .. }
                                    )
                                );
                                let spaces = ["created", "concurrent"]
                                    .into_iter()
                                    .map(|id| tcode_protocol::SpaceInfo {
                                        id: id.into(),
                                        name: id.into(),
                                        created_unix: 1,
                                        project_ids: if shared && id == "created" {
                                            vec!["shared".into()]
                                        } else {
                                            vec![]
                                        },
                                        link: None,
                                        link_enabled: true,
                                        link_dead: false,
                                        members: vec![],
                                    })
                                    .collect();
                                HostMessage::QueryResult {
                                    id: request.id,
                                    result: Ok(tcode_protocol::QueryResponse::Hosting(
                                        tcode_protocol::HostingState {
                                            created_space_id: created.then(|| "created".into()),
                                            spaces: if matches!(
                                                action,
                                                tcode_protocol::HostingAction::State
                                            ) {
                                                vec![]
                                            } else {
                                                spaces
                                            },
                                            enabled: true,
                                            expires_in_secs: 0,
                                            host_id: "machine".into(),
                                            host_name: "Studio".into(),
                                            invite: None,
                                            devices: vec![],
                                            traverse: vec![],
                                            group: None,
                                        },
                                    )),
                                }
                            }
                            ClientPayload::Query(tcode_protocol::Query::Ping) => {
                                HostMessage::QueryResult {
                                    id: request.id,
                                    result: Ok(tcode_protocol::QueryResponse::Pong),
                                }
                            }
                            ClientPayload::Query(tcode_protocol::Query::ReadProjectIcon {
                                ..
                            }) => HostMessage::QueryResult {
                                id: request.id,
                                result: Err(tcode_protocol::ProtocolError {
                                    code: "not_found".into(),
                                    message: "no project icon".into(),
                                }),
                            },
                            ClientPayload::Unsubscribe(_) => continue,
                            other => panic!("unexpected member request: {other:?}"),
                        };
                        let line = tcode_protocol::encode_line(&response).unwrap();
                        if replies.send(line).await.is_err() {
                            break;
                        }
                    }
                });
            });
            Self {
                driver: LocalHostDriver::from_link(link.clone()),
                link,
                scope,
                requests: recorded,
                worker: Some(worker),
            }
        }

        fn store(&mut self, cx: &mut TestAppContext) -> gpui::Entity<WorkspaceStore> {
            let workspace = cx.new(|cx| {
                WorkspaceStore::new_attached(
                    self.link.clone(),
                    WorkspaceAttachment::Remote {
                        host_id: "machine".into(),
                        host_name: "Studio".into(),
                    },
                    None,
                    None,
                    false,
                    cx,
                )
            });
            self.driver
                .wait_until(cx, &workspace, "space baseline", |cx| {
                    workspace.read_with(cx, |store, _| store.baseline_ready())
                });
            workspace
        }

        fn barrier(&mut self, cx: &mut TestAppContext, workspace: &gpui::Entity<WorkspaceStore>) {
            self.driver
                .await_with_events(cx, workspace, self.link.query(tcode_protocol::Query::Ping))
                .unwrap();
        }
    }

    impl Drop for ScriptedSpace {
        fn drop(&mut self) {
            self.link.close();
            self.worker.take().unwrap().join().unwrap();
        }
    }

    #[gpui::test]
    fn member_scope_seeds_only_the_space_index_and_reseeds_on_reconnect(cx: &mut TestAppContext) {
        cx.update(crate::theme::init);
        cx.update(|cx| cx.set_reduce_motion(true));
        let mut host = ScriptedSpace::new();
        let workspace = host.store(cx);
        host.driver
            .wait_until(cx, &workspace, "member baseline", |cx| {
                workspace.read_with(cx, |store, _| store.baseline_ready())
            });
        host.barrier(cx, &workspace);
        workspace.read_with(cx, |store, _| {
            assert_eq!(
                store
                    .projects()
                    .iter()
                    .map(|p| p.id.as_str())
                    .collect::<Vec<_>>(),
                ["shared"]
            );
            assert_eq!(store.grouped_sessions()[0].sessions[0].id, "shared-thread");
            assert_eq!(store.enabled_profiles()[0].id, "team-agent");
            assert_eq!(
                store.picker_models_for_profile("team-agent")[0].id,
                "team-model"
            );
            assert!(store.settings_hydrated());
        });
        let subscriptions = host
            .requests
            .lock()
            .unwrap()
            .iter()
            .filter_map(|request| match &request.payload {
                tcode_protocol::ClientPayload::Subscribe(subscription) => {
                    Some(subscription.topic.clone())
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            subscriptions,
            [
                Topic::Scope,
                Topic::SpaceIndex {
                    space_id: "space-one".into()
                }
            ]
        );
        workspace.update(cx, |store, _| {
            store.set_sidebar_layout(tcode_core::settings::SidebarLayout::Grouped)
        });
        let window_state = cx.new(|_| crate::window_state::WindowState::new(false));
        let (_sidebar, cx) = cx.add_window_view(|_, cx| {
            crate::sidebar::SessionsSidebar::new(workspace.clone(), window_state, cx)
        });
        cx.simulate_resize(gpui::size(gpui::px(360.), gpui::px(800.)));
        cx.update(|window, cx| {
            let _ = window.draw(cx);
        });
        assert!(
            cx.debug_bounds("project-header-shared").is_some(),
            "the scoped project renders in the sidebar"
        );
        assert!(
            cx.debug_bounds("sidebar-thread-shared-thread").is_some(),
            "the scoped conversation renders in the sidebar"
        );
        *host.scope.lock().unwrap() = tcode_protocol::Scope::Space {
            space_id: "space-two".into(),
            space_name: "Moved".into(),
            projects: vec![project_at("other", std::path::Path::new("/other"))],
            providers: Vec::new(),
        };
        workspace.update(cx, |store, _| {
            let state = tcode_client::ConnectionState::Reconnecting {
                attempt: 1,
                reason: None,
            };
            store.host.set_connection_state(state.clone());
            store.apply_connection_state(state);
            let state = tcode_client::ConnectionState::Syncing { path: None };
            store.host.set_connection_state(state.clone());
            store.apply_connection_state(state);
        });
        host.driver
            .wait_until(cx, &workspace, "new space baseline", |cx| {
                workspace.read_with(cx, |store, _| {
                    store.projects().first().is_some_and(|p| p.id == "other")
                        && store.index_hydrated()
                })
            });
        host.barrier(cx, &workspace);
        let requests = host.requests.lock().unwrap();
        let new_space = requests
            .iter()
            .find_map(|request| match &request.payload {
                tcode_protocol::ClientPayload::Subscribe(subscription)
                    if subscription.topic
                        == (Topic::SpaceIndex {
                            space_id: "space-two".into(),
                        }) =>
                {
                    Some(subscription)
                }
                _ => None,
            })
            .expect("new space index subscription");
        assert_eq!(
            new_space.known_index, None,
            "a new space needs its own baseline"
        );
        drop(requests);
        workspace.read_with(cx, |store, _| {
            assert!(store.enabled_profiles().is_empty());
            assert_eq!(
                store.index_topic(),
                Topic::SpaceIndex {
                    space_id: "space-two".into()
                }
            );
        });
        assert!(
            host.requests
                .lock()
                .unwrap()
                .iter()
                .all(|request| !matches!(
                    &request.payload,
                    tcode_protocol::ClientPayload::Subscribe(tcode_protocol::Subscription {
                        topic: Topic::Index
                            | Topic::Settings
                            | Topic::Providers
                            | Topic::RuntimeEvents
                            | Topic::Preview
                            | Topic::ExternalImport { .. },
                        ..
                    })
                ))
        );
        assert!(
            cx.update(|_, cx| crate::remote::spaces::for_store(&workspace, cx))
                .is_none()
        );
        *host.scope.lock().unwrap() = tcode_protocol::Scope::Full;
        workspace.update(cx, |store, _| {
            let state = tcode_client::ConnectionState::Reconnecting {
                attempt: 1,
                reason: None,
            };
            store.host.set_connection_state(state.clone());
            store.apply_connection_state(state);
            let state = tcode_client::ConnectionState::Syncing { path: None };
            store.host.set_connection_state(state.clone());
            store.apply_connection_state(state);
        });
        host.driver
            .wait_until(cx, &workspace, "full scope baseline", |cx| {
                workspace.read_with(cx, |store, _| {
                    store.scope().is_full() && store.baseline_ready()
                })
            });

        let spaces = cx
            .update(|_, cx| crate::remote::spaces::for_store(&workspace, cx))
            .expect("Full may manage its host without pair-time metadata");
        let created = spaces.update(cx, |spaces, cx| {
            spaces.create("Created".into(), Some("shared".into()), cx)
        });
        let result = std::rc::Rc::new(std::cell::RefCell::new(None));
        let captured = result.clone();
        cx.spawn(async move |_| {
            *captured.borrow_mut() = Some(created.await);
        })
        .detach();
        host.driver
            .wait_until(cx, &workspace, "created space and project share", |_| {
                result.borrow().is_some()
            });
        assert_eq!(
            result.borrow().as_ref().unwrap().as_ref().unwrap().id,
            "created"
        );
        assert!(host.requests.lock().unwrap().iter().any(|request| matches!(&request.payload,
            tcode_protocol::ClientPayload::Query(tcode_protocol::Query::Hosting {
                action: tcode_protocol::HostingAction::Spaces(tcode_protocol::SpaceAction::SetProjects { id, .. })
            }) if id == "created")));
        assert_eq!(
            spaces.read_with(cx, |spaces, _| spaces.sharing("shared")),
            ["created"]
        );
        *host.scope.lock().unwrap() = tcode_protocol::Scope::Space {
            space_id: "space-one".into(),
            space_name: "Current".into(),
            projects: vec![],
            providers: vec![],
        };
        workspace.update(cx, |store, cx| {
            let state = tcode_client::ConnectionState::Reconnecting {
                attempt: 1,
                reason: None,
            };
            store.host.set_connection_state(state.clone());
            store.apply_connection_state(state);
            store.apply_domain_event(
                &EventEnvelope {
                    request_id: None,
                    topic: Topic::Scope,
                    event: ServerEvent::ScopeSnapshot(tcode_protocol::Scope::Full),
                },
                cx,
            );
            assert!(
                !store.can_manage_host(),
                "an uncorrelated scope cannot establish a role while pending"
            );
            cx.notify();
        });
        cx.run_until_parked();
        assert!(
            cx.update(|_, cx| crate::remote::spaces::for_store(&workspace, cx))
                .is_none()
        );
        assert!(
            spaces.read_with(cx, |spaces, _| spaces.spaces().is_empty()),
            "the cached share badges are invalidated"
        );
        workspace.update(cx, |store, _| {
            let state = tcode_client::ConnectionState::Syncing { path: None };
            store.host.set_connection_state(state.clone());
            store.apply_connection_state(state);
        });
        host.driver
            .wait_until(cx, &workspace, "moved member baseline", |cx| {
                workspace.read_with(cx, |store, _| {
                    !store.scope().is_full() && store.baseline_ready()
                })
            });
        workspace.update(cx, |store, cx| {
            store.apply_domain_event(
                &EventEnvelope {
                    request_id: None,
                    topic: Topic::Scope,
                    event: ServerEvent::ScopeSnapshot(tcode_protocol::Scope::Full),
                },
                cx,
            );
            assert!(!store.scope().is_full());
            assert!(store.machine_label().contains("Current"));
        });
        assert!(
            cx.update(|_, cx| crate::remote::spaces::for_store(&workspace, cx))
                .is_none()
        );
    }

    #[gpui::test]
    fn member_sidebar_and_read_preferences_change_without_host_commands(cx: &mut TestAppContext) {
        let mut host = ScriptedSpace::new();
        let workspace = host.store(cx);
        host.driver
            .wait_until(cx, &workspace, "member baseline", |cx| {
                workspace.read_with(cx, |store, _| store.baseline_ready())
            });
        host.barrier(cx, &workspace);
        host.requests.lock().unwrap().clear();
        workspace.update(cx, |store, cx| {
            store.toggle_project_collapsed("shared".into(), cx);
            store.set_sidebar_collapsed(true, cx);
            store.cycle_project_sort(cx);
            store.toggle_favorite_model("team-model".into(), cx);
            store.mark_session_unread("shared-thread".into(), cx);
            assert!(store.session_unread("shared-thread"));
            store.dispatch(Command::MarkSessionRead {
                session_id: "shared-thread".into(),
                through: 100,
            });
            assert!(!store.session_unread("shared-thread"));
            assert!(store.is_project_collapsed("shared"));
            assert!(store.settings().sidebar_collapsed);
            assert_eq!(
                store.project_sort(),
                tcode_core::settings::ProjectSort::NameAsc
            );
            assert_eq!(store.settings().favorite_models, ["team-model"]);
        });
        host.barrier(cx, &workspace);
        assert!(
            host.requests
                .lock()
                .unwrap()
                .iter()
                .all(|request| !matches!(
                    &request.payload,
                    tcode_protocol::ClientPayload::Command(_)
                ))
        );
    }

    #[cfg(preview_backend)]
    #[gpui::test]
    fn preview_reads_the_attachment_route_when_saved_hosts_are_stale(cx: &mut TestAppContext) {
        use std::rc::Rc;
        use tcode_client::host::{ClientHost as _, LiveHost};
        let root = crate::store::tests::scratch_root("preview-route");
        let client = Rc::new(tcode_traverse::NativeClientHost::new(root.clone(), "phone"));
        let mut host = tcode_client::pairing::PairedHost {
            host_id: "09e5c592f3234ef329634a0baa6bfe06193be6b64801121d84242af1d27dbecf".into(),
            name: "Machine".into(),
            traverse: Vec::new(),
            relay: None,
            addrs: vec!["192.168.31.5:47420".into()],
            last_connected_unix: None,
            group_id: None,
            space_id: None,
            space_name: None,
        };
        client.remember_host(host.clone());
        struct NoTunnels;
        impl tcode_client::host::TunnelOpener for NoTunnels {
            fn open(&self, _host: &str, _port: u16) -> tcode_client::host::TunnelFuture {
                Box::pin(async { Err(std::io::Error::from(std::io::ErrorKind::NotConnected)) })
            }
        }
        let current_host = LiveHost::with_tunnels(host.clone(), std::sync::Arc::new(NoTunnels));
        let (to_host, _outgoing) = async_channel::unbounded();
        let (_incoming, from_host) = async_channel::unbounded();
        let store = cx.new(|cx| {
            WorkspaceStore::new_attached(
                tcode_client::HostLink::new(to_host, from_host),
                WorkspaceAttachment::Remote {
                    host_id: host.host_id.clone(),
                    host_name: host.name.clone(),
                },
                Some(client.clone()),
                Some(current_host.clone()),
                false,
                cx,
            )
        });
        host.addrs = vec!["192.168.1.161:47420".into()];
        current_host.authenticated(&host);
        cx.update(|cx| assert_eq!(store.read(cx).preview_proxy().unwrap().unwrap().0, host));
        assert_eq!(client.load_hosts()[0].addrs, ["192.168.31.5:47420"]);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[gpui::test]
    fn preview_requests_for_an_unviewed_thread_reach_the_panel(cx: &mut TestAppContext) {
        cx.update(crate::theme::init);
        cx.update(crate::markdown::init);
        let (to_host, _requests) = async_channel::unbounded();
        let (_replies, from_host) = async_channel::unbounded();
        let link = tcode_client::HostLink::new(to_host, from_host);
        let store = cx.new(|cx| {
            WorkspaceStore::new_attached(
                link.clone(),
                WorkspaceAttachment::Local,
                None,
                None,
                false,
                cx,
            )
        });
        let requests = store.read_with(cx, |store, _| store.remote_preview_requests());
        store.update(cx, |store, cx| {
            store.selected_session_id = Some("viewed".into());
            store.apply_domain_event(
                &EventEnvelope {
                    request_id: None,
                    topic: Topic::Preview,
                    event: ServerEvent::PreviewRequest {
                        request_id: 7,
                        session_id: "background".into(),
                        request: tcode_protocol::PreviewRequest::Status,
                    },
                },
                cx,
            );
        });
        let forwarded = requests
            .try_recv()
            .expect("the panel answers for a background thread");
        assert!(matches!(
            forwarded.event,
            ServerEvent::PreviewRequest { request_id: 7, ref session_id, .. } if session_id == "background"
        ));
    }

    #[gpui::test]
    fn scripted_host_send_waits_for_ack_and_rejection_offers_retry(cx: &mut TestAppContext) {
        cx.update(crate::theme::init);
        cx.update(crate::markdown::init);
        let (to_host, requests) = async_channel::unbounded();
        let (replies, from_host) = async_channel::unbounded();
        let deferred = std::iter::from_fn(|| from_host.try_recv().ok()).collect();
        let link = tcode_client::HostLink::new(to_host, from_host);
        let store = cx.new(|cx| {
            WorkspaceStore::new_attached(
                link.clone(),
                WorkspaceAttachment::Local,
                None,
                None,
                false,
                cx,
            )
        });
        crate::store::tests::seed_full_scope(&store, &replies, deferred, cx);
        store.update(cx, |store, _| {
            store.selected_session_id = Some("scripted".into());
            store.send_turn("hello".into(), Vec::new());
        });
        let request = loop {
            let request =
                tcode_protocol::decode_client_line(&requests.recv_blocking().unwrap()).unwrap();
            if matches!(
                request.payload,
                tcode_protocol::ClientPayload::Command(Command::SendTurn { .. })
            ) {
                break request;
            }
        };
        let key = request.key.clone().unwrap();
        assert_eq!(
            store.read_with(cx, |store, _| store.delivery_messages()),
            vec![(key.clone(), "hello".into(), None, false)]
        );
        replies
            .send_blocking(
                tcode_protocol::encode_line(&tcode_protocol::HostMessage::Ack {
                    id: request.id,
                    result: Err(tcode_protocol::ProtocolError {
                        code: "rejected".into(),
                        message: "Host refused this send".into(),
                    }),
                })
                .unwrap(),
            )
            .unwrap();
        // Drive the production pump on the test thread; no mocked HostLink.
        let mut pump = std::pin::pin!(link.pump());
        let mut task_cx = std::task::Context::from_waker(std::task::Waker::noop());
        assert!(std::future::Future::poll(pump.as_mut(), &mut task_cx).is_pending());
        assert_eq!(
            store.read_with(cx, |store, _| store.delivery_messages()),
            vec![(
                key.clone(),
                "hello".into(),
                Some("Host refused this send".into()),
                false,
            )]
        );
        assert!(
            !store.read_with(cx, |store, _| store.chat_loading()),
            "a rejected write must be visible without a snapshot"
        );
        let window_state = cx.new(|_| crate::window_state::WindowState::new(false));
        let (_chat, visual) = cx.add_window_view(|window, cx| {
            crate::chat::ChatView::new(store.clone(), window_state, window, cx)
        });
        visual.simulate_resize(gpui::size(gpui::px(1024.), gpui::px(700.)));
        visual.update(|window, cx| {
            let _ = window.draw(cx);
        });
        assert!(visual.debug_bounds("retry-delivery").is_some());
        assert!(visual.debug_bounds("discard-delivery").is_some());
        store.update(cx, |store, _| store.retry_delivery(&key));
        let retry = tcode_protocol::decode_client_line(&requests.recv_blocking().unwrap()).unwrap();
        assert_ne!(retry.key, request.key);
        assert_eq!(retry.payload, request.payload);
        replies
            .send_blocking(
                tcode_protocol::encode_line(&tcode_protocol::HostMessage::Ack {
                    id: retry.id,
                    result: Ok(tcode_protocol::CommandResponse::Unit),
                })
                .unwrap(),
            )
            .unwrap();
        assert!(std::future::Future::poll(pump.as_mut(), &mut task_cx).is_pending());
        assert_eq!(
            store.read_with(cx, |store, _| store.delivery_messages()),
            vec![(retry.key.clone().unwrap(), "hello".into(), None, true)]
        );
        let accepted_key = retry.key.unwrap();
        let same_text_record = |id: String| {
            SessionEventRecord::from(AgentEvent::ItemCompleted(ThreadItem {
                id,
                parent_item_id: None,
                content: ItemContent::UserMessage {
                    text: "hello".into(),
                    context_len: None,
                    attachments: Vec::new(),
                },
            }))
        };
        store.update(cx, |store, cx| {
            store.apply_domain_event(
                &EventEnvelope {
                    request_id: None,
                    topic: Topic::SessionEvents {
                        session_id: "scripted".into(),
                    },
                    event: ServerEvent::SessionSnapshot {
                        from: 0,
                        end: 1,
                        total: 1,
                        total_turns: 1,
                        records: vec![same_text_record("unrelated".into())],
                        truncated: false,
                    },
                },
                cx,
            );
            assert_eq!(
                store.delivery_messages(),
                vec![(accepted_key.clone(), "hello".into(), None, true)]
            );
            store.selected_session_id = Some("another".into());
            // Adoption must survive navigation even when no history is retained.
            store.apply_domain_event(
                &EventEnvelope {
                    request_id: None,
                    topic: Topic::SessionEvents {
                        session_id: "scripted".into(),
                    },
                    event: ServerEvent::SessionEvent(same_text_record(format!(
                        "local-user-{accepted_key}"
                    ))),
                },
                cx,
            );
            store.threads.remove("scripted");
            store.selected_session_id = Some("scripted".into());
            assert!(store.delivery_messages().is_empty());
        });
        store.update(cx, |store, _| store.send_turn("hello".into(), Vec::new()));
        let early = tcode_protocol::decode_client_line(&requests.recv_blocking().unwrap()).unwrap();
        let early_key = early.key.unwrap();
        store.update(cx, |store, cx| {
            store.selected_session_id = Some("another".into());
            store.apply_domain_event(
                &EventEnvelope {
                    request_id: None,
                    topic: Topic::SessionEvents {
                        session_id: "scripted".into(),
                    },
                    event: ServerEvent::SessionEvent(same_text_record(format!(
                        "local-user-{early_key}"
                    ))),
                },
                cx,
            );
        });
        replies
            .send_blocking(
                tcode_protocol::encode_line(&tcode_protocol::HostMessage::Ack {
                    id: early.id,
                    result: Ok(tcode_protocol::CommandResponse::Unit),
                })
                .unwrap(),
            )
            .unwrap();
        assert!(std::future::Future::poll(pump.as_mut(), &mut task_cx).is_pending());
        store.update(cx, |store, _| {
            store.selected_session_id = Some("scripted".into());
        });
        assert!(
            store.read_with(cx, |store, _| store.delivery_messages().is_empty()),
            "a replica received before its Ack must stay adopted"
        );
        store.update(cx, |store, _| {
            store.send_turn("discard me".into(), Vec::new())
        });
        let request =
            tcode_protocol::decode_client_line(&requests.recv_blocking().unwrap()).unwrap();
        replies
            .send_blocking(
                tcode_protocol::encode_line(&tcode_protocol::HostMessage::Ack {
                    id: request.id,
                    result: Err(tcode_protocol::ProtocolError {
                        code: "unknown_session".into(),
                        message: "gone".into(),
                    }),
                })
                .unwrap(),
            )
            .unwrap();
        assert!(std::future::Future::poll(pump.as_mut(), &mut task_cx).is_pending());
        store.update(cx, |store, _| {
            store.discard_delivery(request.key.as_ref().unwrap())
        });
        assert!(link.failed_commands().is_empty());
        assert!(link.pending_commands().is_empty());
        link.close();
    }

    #[gpui::test]
    fn archived_list_coalesces_revision_changes_and_reloads_a_stale_reply(cx: &mut TestAppContext) {
        let (to_host, requests) = async_channel::unbounded();
        let (replies, from_host) = async_channel::unbounded();
        let deferred = std::iter::from_fn(|| from_host.try_recv().ok()).collect();
        let link = tcode_client::HostLink::new(to_host, from_host);
        let store = cx.new(|cx| {
            WorkspaceStore::new_attached(
                link.clone(),
                WorkspaceAttachment::Local,
                None,
                None,
                false,
                cx,
            )
        });
        crate::store::tests::seed_full_scope(&store, &replies, deferred, cx);
        let mut pump = std::pin::pin!(link.pump());
        let mut task_cx = std::task::Context::from_waker(std::task::Waker::noop());
        let queries = || {
            std::iter::from_fn(|| requests.try_recv().ok())
                .filter_map(|line| {
                    let message = tcode_protocol::decode_client_line(&line).unwrap();
                    matches!(
                        message.payload,
                        tcode_protocol::ClientPayload::Query(
                            tcode_protocol::Query::ArchivedSessions
                        )
                    )
                    .then_some(message.id)
                })
                .collect::<Vec<_>>()
        };
        let revision_event = |revision| EventEnvelope {
            request_id: None,
            topic: Topic::Index,
            event: ServerEvent::IndexSummaryReplaced(tcode_protocol::IndexSummary {
                archived_revision: revision,
                ..Default::default()
            }),
        };
        store.update(cx, |store, cx| {
            store.apply_domain_event(&revision_event(10), cx);
            store.hold_archived_sessions(cx);
        });
        cx.run_until_parked();
        let first = queries();
        assert_eq!(first.len(), 1);
        store.update(cx, |store, cx| {
            store.apply_domain_event(&revision_event(11), cx);
            store.apply_domain_event(&revision_event(12), cx);
        });
        cx.run_until_parked();
        assert!(
            queries().is_empty(),
            "one reload may be in flight at a time"
        );
        let response = |id, revision, title: &str| {
            let mut meta = SessionMeta::new(
                ProviderKind::Codex,
                std::path::PathBuf::from("/archive"),
                None,
            );
            meta.id = "archived".into();
            meta.title = title.into();
            meta.archived_at = Some(1);
            replies
                .try_send(
                    tcode_protocol::encode_line(&tcode_protocol::HostMessage::QueryResult {
                        id,
                        result: Ok(tcode_protocol::QueryResponse::ArchivedSessions(
                            tcode_protocol::ArchivedSessions {
                                sessions: vec![meta],
                                worktree_shared: Default::default(),
                                revision,
                            },
                        )),
                    })
                    .unwrap(),
                )
                .unwrap();
        };
        response(first[0], 10, "Stale title");
        assert!(std::future::Future::poll(pump.as_mut(), &mut task_cx).is_pending());
        cx.run_until_parked();
        let second = queries();
        assert_eq!(second.len(), 1);
        assert!(store.read_with(cx, |store, _| store.archived_loading()));
        response(second[0], 12, "Current title");
        assert!(std::future::Future::poll(pump.as_mut(), &mut task_cx).is_pending());
        cx.run_until_parked();
        store.read_with(cx, |store, _| {
            let archived = store.archived_replica.as_ref().unwrap();
            assert_eq!(archived.revision, 12);
            assert_eq!(archived.sessions[0].title, "Current title");
        });
        store.update(cx, |store, cx| {
            store.apply_domain_event(&revision_event(13), cx)
        });
        cx.run_until_parked();
        let third = queries();
        assert_eq!(third.len(), 1, "an archived rename invalidates a held list");
        response(third[0], 13, "Renamed again");
        assert!(std::future::Future::poll(pump.as_mut(), &mut task_cx).is_pending());
        cx.run_until_parked();
        store.update(cx, |store, cx| {
            let state = tcode_client::ConnectionState::Reconnecting {
                attempt: 1,
                reason: None,
            };
            store.host.set_connection_state(state.clone());
            store.apply_connection_state(state);
            let state = tcode_client::ConnectionState::Syncing { path: None };
            store.host.set_connection_state(state.clone());
            store.apply_connection_state(state);
            store.apply_domain_event(
                &EventEnvelope {
                    request_id: None,
                    topic: Topic::Index,
                    event: ServerEvent::IndexSnapshot(tcode_protocol::IndexSnapshot {
                        summary: tcode_protocol::IndexSummary {
                            archived_revision: 1,
                            ..Default::default()
                        },
                        sessions: Vec::new(),
                        projects: Vec::new(),
                    }),
                },
                cx,
            );
        });
        cx.run_until_parked();
        let reconnect = queries();
        assert_eq!(reconnect.len(), 1);
        response(reconnect[0], 1, "New host baseline");
        assert!(std::future::Future::poll(pump.as_mut(), &mut task_cx).is_pending());
        cx.run_until_parked();
        store.update(cx, |store, cx| {
            assert_eq!(
                store.archived_replica.as_ref().unwrap().sessions[0].title,
                "New host baseline"
            );
            // Settings and the Agents view hold the list independently.
            store.hold_archived_sessions(cx);
            store.release_archived_sessions();
            assert!(
                !store.archived_loading(),
                "one view closing keeps another's list"
            );
            store.release_archived_sessions();
            assert!(store.archived_loading());
        });
        link.close();
    }

    #[gpui::test]
    fn threads_wait_for_baseline_before_rendering_empty(cx: &mut TestAppContext) {
        use gpui::{px, size};
        cx.update(crate::theme::init);
        let (to_host, _outgoing) = async_channel::unbounded();
        let (_incoming, from_host) = async_channel::unbounded();
        let store = cx.new(|cx| {
            WorkspaceStore::new_attached(
                tcode_client::HostLink::new(to_host, from_host),
                super::WorkspaceAttachment::Remote {
                    host_id: "test".into(),
                    host_name: "Test".into(),
                },
                None,
                None,
                false,
                cx,
            )
        });
        seed_full_scope(&store, &_incoming, Vec::new(), cx);
        let window_state =
            cx.new(|_| crate::window_state::WindowState::new(false).with_compact(true));
        let (_sidebar, cx) = cx.add_window_view(|_, cx| {
            crate::sidebar::SessionsSidebar::new(store.clone(), window_state, cx)
        });
        cx.simulate_resize(size(px(393.), px(852.)));
        cx.update(|window, cx| {
            let _ = window.draw(cx);
        });
        assert!(
            cx.debug_bounds("baseline-loading").is_some(),
            "Threads must render a skeleton before Index arrives"
        );
        assert!(cx.debug_bounds("threads-empty").is_none());
        store.update(cx, |store, cx| {
            store.apply_domain_event(
                &EventEnvelope {
                    request_id: None,
                    topic: Topic::Settings,
                    event: ServerEvent::SettingsSnapshot(Settings::default()),
                },
                cx,
            );
            store.apply_domain_event(
                &EventEnvelope {
                    request_id: None,
                    topic: Topic::Index,
                    event: ServerEvent::IndexSnapshot(tcode_protocol::IndexSnapshot {
                        summary: Default::default(),
                        sessions: vec![],
                        projects: vec![],
                    }),
                },
                cx,
            );
            cx.emit(super::StoreChange {
                topic: super::TopicKind::Index,
            });
            cx.notify();
        });
        cx.run_until_parked();
        cx.update(|window, cx| {
            let _ = window.draw(cx);
        });
        assert!(cx.debug_bounds("baseline-loading").is_none());
        assert!(
            cx.debug_bounds("threads-empty").is_some(),
            "An applied empty baseline must render the empty message"
        );
    }

    #[gpui::test]
    fn deleted_thread_keeps_its_pending_and_rejected_send_visible(cx: &mut TestAppContext) {
        let root = scratch_root("tcode-deleted-pending");
        let disk = SessionStore::open_at(root.clone()).unwrap();
        disk.upsert_project(&project_at("p", &root)).unwrap();
        disk.upsert_meta(&thread(&root, "deleted", "p", None))
            .unwrap();
        let host = test_host(disk);
        let mut driver = LocalHostDriver::new(&host);
        let link = driver.link.clone();
        let workspace = driver.workspace(cx);
        workspace.update(cx, |store, _| store.select_session("deleted".into()));
        driver.wait_until(cx, &workspace, "selected thread", |cx| {
            selected_status(cx, &workspace, "deleted")
        });
        link.set_connection_state(tcode_client::ConnectionState::Reconnecting {
            attempt: 1,
            reason: None,
        });
        workspace.update(cx, |store, _| {
            store.send_turn("keep my failed send".into(), Vec::new())
        });
        smol::block_on(
            host.update_state_for_test(|state, cx| state.delete_session("deleted", false, cx)),
        )
        .unwrap();
        workspace.update(cx, |store, cx| {
            store.apply_domain_event(
                &EventEnvelope {
                    request_id: None,
                    topic: Topic::Index,
                    event: ServerEvent::IndexRemoveSession {
                        session_id: "deleted".into(),
                    },
                },
                cx,
            )
        });
        assert_eq!(
            workspace.read_with(cx, |store, _| store.active_session_id()),
            Some("deleted".into())
        );
        link.set_connection_state(tcode_client::ConnectionState::Connected { path: None });
        driver.wait_until(cx, &workspace, "rejected send", |cx| {
            workspace.read_with(cx, |store, _| {
                store
                    .delivery_messages()
                    .iter()
                    .any(|(_, _, error, _)| error.is_some())
            })
        });
        workspace.read_with(cx, |store, _| {
            assert_eq!(store.active_session_id().as_deref(), Some("deleted"));
            assert_eq!(store.delivery_messages()[0].1, "keep my failed send");
            assert!(!store.chat_loading());
        });
        let key = link.failed_commands()[0].0.key.clone();
        workspace.update(cx, |store, _| store.discard_delivery(&key));
        assert!(link.pending_commands().is_empty() && link.failed_commands().is_empty());
        driver.shutdown(cx, &workspace, &host);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn client_preferences_persist_and_override_host_settings_only_when_set() {
        use tcode_client::host::{ClientHost as _, ClientPreferences};

        let root = scratch_root("tcode-desktop-preferences");
        let client = tcode_traverse::NativeClientHost::new(root.clone(), "fallback device");
        let host = Settings {
            theme_mode: ThemeMode::Dark,
            language: Some(crate::LANGUAGE_SIMPLIFIED_CHINESE.into()),
            ..Settings::default()
        };

        assert_eq!(
            effective_client_settings(&host, &client.load_preferences()),
            host
        );

        client.save_preferences(&ClientPreferences {
            appearance: Some("light".into()),
            language: Some("system".into()),
            device_name: Some("Desk client".into()),
            ..Default::default()
        });
        let reloaded = tcode_traverse::NativeClientHost::new(root.clone(), "different fallback");
        let preferences = reloaded.load_preferences();
        let effective = effective_client_settings(&host, &preferences);
        assert_eq!(effective.theme_mode, ThemeMode::Light);
        assert_eq!(effective.language, None);
        assert_eq!(reloaded.device_name(), "Desk client");

        reloaded.save_preferences(&ClientPreferences::default());
        assert_eq!(
            effective_client_settings(&host, &reloaded.load_preferences()),
            host,
            "clearing the client override must reveal the replicated host fallback"
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[gpui::test]
    fn rapid_selection_is_immediate_idempotent_and_retires_old_loads(cx: &mut TestAppContext) {
        let (to_host, outgoing) = async_channel::unbounded();
        let (_incoming, from_host) = async_channel::unbounded();
        let host = tcode_client::HostLink::new(to_host, from_host);
        let workspace = cx.new(|cx| {
            WorkspaceStore::new_attached(
                host.clone(),
                WorkspaceAttachment::Local,
                None,
                None,
                false,
                cx,
            )
        });
        seed_full_scope(&workspace, &_incoming, Vec::new(), cx);
        workspace.update(cx, |store, cx| {
            let task_count = store.attachment_tasks.len();
            for index in 0..20 {
                let id = if index % 2 == 0 { "one" } else { "two" };
                store.select_session(id.into());
                assert_eq!(store.active_session_id().as_deref(), Some(id));
                assert!(store.session_loading());
                let messages = outgoing.len();
                store.select_session(id.into());
                assert_eq!(outgoing.len(), messages, "second tap must send nothing");
                assert_eq!(store.attachment_tasks.len(), task_count);
                assert_eq!(
                    host.subscriptions()
                        .iter()
                        .filter(|sub| matches!(sub.topic, Topic::SessionEvents { .. }))
                        .count(),
                    1
                );
                assert_eq!(
                    host.subscriptions()
                        .iter()
                        .filter(|sub| matches!(sub.topic, Topic::SessionStatus { .. }))
                        .count(),
                    1
                );
            }
            store.apply_domain_event(
                &EventEnvelope {
                    request_id: None,
                    topic: Topic::SessionEvents {
                        session_id: "one".into(),
                    },
                    event: ServerEvent::SessionSnapshot {
                        total: 0,
                        total_turns: 0,
                        truncated: false,
                        from: 0,
                        end: 0,
                        records: vec![],
                    },
                },
                cx,
            );
            assert!(
                store.session_loading(),
                "retired load must not replace the skeleton"
            );
            assert!(store.session_replica.is_none());
        });
    }

    #[gpui::test]
    fn rapid_thread_revisit_keeps_cached_history_when_old_replies_arrive(cx: &mut TestAppContext) {
        for old_first in [false, true] {
            for empty in [false, true] {
                let (to_host, _outgoing) = async_channel::unbounded();
                let (incoming, from_host) = async_channel::unbounded();
                let host = tcode_client::HostLink::new(to_host, from_host);
                let workspace = cx.new(|cx| {
                    WorkspaceStore::new_attached(
                        host.clone(),
                        WorkspaceAttachment::Local,
                        None,
                        None,
                        false,
                        cx,
                    )
                });
                seed_full_scope(&workspace, &incoming, Vec::new(), cx);
                workspace.update(cx, |store, cx| {
                    let topic = Topic::SessionEvents { session_id: "a".into() };
                    store.select_session("a".into());
                    let record = |text: &str| SessionEventRecord::from(AgentEvent::Delta {
                        item_id: "answer".into(), kind: agent::DeltaKind::AssistantText, text: text.into(),
                    });
                    let baseline_id = host.subscription_request_id(&topic).unwrap();
                    store.apply_domain_event(&EventEnvelope {
                        request_id: Some(baseline_id), topic: topic.clone(),
                        event: ServerEvent::SessionSnapshot { from: 10, end: 12, records: vec![record("cached")], total: 12, total_turns: 1, truncated: false },
                    }, cx);
                    host.subscribe(Subscription { topic: topic.clone(), after: Some(12), known_index: None }).unwrap();
                    let old_id = host.subscription_request_id(&topic).unwrap();
                    store.select_session("b".into());
                    store.select_session("a".into());
                    let current_id = host.subscription_request_id(&topic).unwrap();
                    assert_ne!(old_id, current_id);
                    let reply = |id| EventEnvelope {
                        request_id: Some(id), topic: topic.clone(),
                        event: ServerEvent::SessionSnapshot {
                            from: 12, end: if empty { 12 } else { 14 },
                            records: if empty { vec![] } else { vec![record(" continued")] },
                            total: if empty { 12 } else { 14 }, total_turns: 1, truncated: false,
                        },
                    };
                    for id in if old_first { [old_id, current_id] } else { [current_id, old_id] } {
                        store.apply_domain_event(&reply(id), cx);
                    }
                    store.apply_domain_event(&reply(current_id), cx);
                    let held = store.threads["a"].history.as_ref().unwrap();
                    assert_eq!((held.from, held.end), (10, if empty { 12 } else { 14 }));
                    let timeline = &store.session_replica.as_ref().unwrap().1;
                    assert_eq!(timeline.entries.len(), 1);
                    assert!(matches!(&timeline.entries[0].content, EntryContent::Item(ItemContent::AssistantMessage { text }) if text == if empty { "cached" } else { "cached continued" }));
                    let end = if empty { 12 } else { 14 };
                    host.subscribe(Subscription { topic: topic.clone(), after: None, known_index: None }).unwrap();
                    let replacement_id = host.subscription_request_id(&topic).unwrap();
                    let replacement = EventEnvelope { request_id: Some(replacement_id), topic: topic.clone(), event: ServerEvent::SessionSnapshot { from: end, end: end + 2, records: vec![record("replacement")], total: end + 2, total_turns: 1, truncated: false } };
                    store.apply_domain_event(&replacement, cx);
                    store.apply_domain_event(&reply(current_id), cx);
                    store.apply_domain_event(&replacement, cx);
                    let held = store.threads["a"].history.as_ref().unwrap();
                    assert_eq!((held.from, held.end), (end, end + 2));
                    let timeline = &store.session_replica.as_ref().unwrap().1;
                    assert_eq!(timeline.entries.len(), 1);
                    assert!(matches!(&timeline.entries[0].content, EntryContent::Item(ItemContent::AssistantMessage { text }) if text == "replacement"));
                });
            }
        }
    }

    #[gpui::test]
    fn paged_history_keeps_event_and_turn_cursors_absolute(cx: &mut TestAppContext) {
        let (to_host, outgoing) = async_channel::unbounded();
        let (_incoming, from_host) = async_channel::unbounded();
        let host = tcode_client::HostLink::new(to_host, from_host);
        let workspace = cx.new(|cx| {
            WorkspaceStore::new_attached(
                host.clone(),
                WorkspaceAttachment::Local,
                None,
                None,
                false,
                cx,
            )
        });
        seed_full_scope(&workspace, &_incoming, Vec::new(), cx);
        workspace.update(cx, |store, cx| {
            store.select_session("large".into());
            let records = (450..500)
                .flat_map(|index| {
                    [
                        AgentEvent::TurnStarted {
                            turn_id: index.to_string(),
                        }
                        .into(),
                        AgentEvent::ItemCompleted(ThreadItem {
                            id: format!("user-{index}"),
                            parent_item_id: None,
                            content: ItemContent::UserMessage {
                                text: format!("Message {index}"),
                                attachments: vec![],
                                context_len: None,
                            },
                        })
                        .into(),
                        AgentEvent::ItemCompleted(ThreadItem {
                            id: format!("assistant-{index}"),
                            parent_item_id: None,
                            content: ItemContent::AssistantMessage {
                                text: format!("Response {index}"),
                            },
                        })
                        .into(),
                        AgentEvent::TurnCompleted {
                            turn_id: index.to_string(),
                            status: TurnStatus::Completed,
                            usage: None,
                        }
                        .into(),
                    ]
                })
                .collect();
            store.apply_domain_event(
                &EventEnvelope {
                    request_id: None,
                    topic: Topic::SessionEvents {
                        session_id: "large".into(),
                    },
                    event: ServerEvent::SessionSnapshot {
                        from: 1800,
                        end: 2000,
                        records,
                        total: 2000,
                        total_turns: 500,
                        truncated: false,
                    },
                },
                cx,
            );
            assert_eq!(store.session_turn_offset, 450);
            assert_eq!(
                host.subscriptions()
                    .iter()
                    .find(|sub| matches!(sub.topic, Topic::SessionEvents { .. }))
                    .unwrap()
                    .after,
                Some(2000)
            );
            while outgoing.try_recv().is_ok() {}
            store.rewind_turn(2, agent::RewindMode::Conversation);
            let command =
                tcode_protocol::decode_client_line(&outgoing.try_recv().unwrap()).unwrap();
            assert!(matches!(
                command.payload,
                tcode_protocol::ClientPayload::Command(Command::RewindTurn { turn: 452, .. })
            ));
            let topic = Topic::SessionEvents {
                session_id: "large".into(),
            };
            store.baseline_topics.remove(&topic);
            host.subscribe(Subscription {
                topic: topic.clone(),
                after: Some(2000),
                known_index: None,
            })
            .unwrap();
            let reconnect_id = host.subscription_request_id(&topic).unwrap();
            let retained_entry = store.session_replica.as_ref().unwrap().1.entries[0].clone();
            store.apply_domain_event(
                &EventEnvelope {
                    request_id: Some(reconnect_id),
                    topic: topic.clone(),
                    event: ServerEvent::SessionSnapshot {
                        from: 2000,
                        end: 2000,
                        records: vec![],
                        total: 2000,
                        total_turns: 500,
                        truncated: false,
                    },
                },
                cx,
            );
            assert!(
                store.baseline_topics.contains(&topic),
                "an empty reconnect tail still establishes readiness"
            );
            assert!(
                std::sync::Arc::ptr_eq(
                    &retained_entry,
                    &store.session_replica.as_ref().unwrap().1.entries[0]
                ),
                "empty replay must retain the existing timeline"
            );
        });
    }

    /// The host merges records, so the cursors a client resumes from are the
    /// window it was sent, not the records it holds; visit times and index
    /// facts arrive as changes rather than replacements.
    #[gpui::test]
    fn merged_windows_visits_and_summaries_apply_as_changes(cx: &mut TestAppContext) {
        use std::collections::{HashMap, HashSet};
        use tcode_core::session::StoredEvent;
        use tcode_protocol::IndexSummary;
        let (to_host, _outgoing) = async_channel::unbounded();
        let (_incoming, from_host) = async_channel::unbounded();
        let deferred = std::iter::from_fn(|| from_host.try_recv().ok()).collect();
        let link = tcode_client::HostLink::new(to_host, from_host);
        let workspace = cx.new(|cx| {
            WorkspaceStore::new_attached(link, WorkspaceAttachment::Local, None, None, false, cx)
        });
        crate::store::tests::seed_full_scope(&workspace, &_incoming, deferred, cx);
        let topic = Topic::SessionEvents {
            session_id: "merged".into(),
        };
        let tool = agent::AgentEvent::ItemCompleted(agent::ThreadItem {
            id: "shot".into(),
            parent_item_id: None,
            content: ItemContent::ToolCall {
                presentation: None,
                image_reads: Vec::new(),
                name: "screenshot".into(),
                input: serde_json::json!({}),
                output: Some("preview".into()),
                status: agent::ItemStatus::Completed,
            },
        });
        workspace.update(cx, |store, cx| {
            store.selected_session_id = Some("merged".into());
            store.settings_replica.last_visited = HashMap::from([("old".into(), 1)]);
            let event = |event| EventEnvelope {
                request_id: None,
                topic: topic.clone(),
                event,
            };
            store.apply_domain_event(
                &event(ServerEvent::SessionSnapshot {
                    from: 10,
                    end: 20,
                    records: vec![StoredEvent {
                        origin: None,
                        author: None,
                        ts: Some(1),
                        event: tool.clone(),
                        elided: Some(700_000),
                    }],
                    total: 20,
                    total_turns: 1,
                    truncated: false,
                }),
                cx,
            );
            assert_eq!(
                (
                    held_history(store, "merged").from,
                    held_history(store, "merged").end
                ),
                (10, 20)
            );
            assert_eq!(
                store.with_active_timeline(|timeline| timeline.elided_outputs.get("shot").copied()),
                Some(Some(700_000))
            );
            store.apply_domain_event(
                &event(ServerEvent::SessionEvent(
                    agent::AgentEvent::TurnStarted {
                        turn_id: "next".into(),
                    }
                    .into(),
                )),
                cx,
            );
            assert_eq!(held_history(store, "merged").end, 21);
            store.apply_domain_event(
                &EventEnvelope {
                    request_id: None,
                    topic: Topic::Settings,
                    event: ServerEvent::LastVisitedChanged(HashMap::from([("new".into(), 2)])),
                },
                cx,
            );
            assert_eq!(
                store.settings_replica.last_visited,
                HashMap::from([("old".into(), 1), ("new".into(), 2)])
            );
            store.apply_domain_event(
                &EventEnvelope {
                    request_id: None,
                    topic: Topic::Index,
                    event: ServerEvent::IndexSummaryReplaced(IndexSummary {
                        title_generating: HashSet::from(["named".into()]),
                        ..IndexSummary::default()
                    }),
                },
                cx,
            );
            assert!(store.title_generating("named"));
        });
    }

    /// A device with full access to a remote machine is the owner's own: the
    /// owner's unattributed messages carry no byline there, while a space
    /// member's message is still attributed.
    #[gpui::test]
    fn a_full_remote_device_attributes_only_space_members(cx: &mut TestAppContext) {
        use tcode_core::session::{Author, StoredEvent};
        let (to_host, _outgoing) = async_channel::unbounded();
        let (incoming, from_host) = async_channel::unbounded();
        let link = tcode_client::HostLink::new(to_host, from_host);
        let workspace = cx.new(|cx| {
            WorkspaceStore::new_attached(
                link,
                WorkspaceAttachment::Remote {
                    host_id: "machine".into(),
                    host_name: "Machine".into(),
                },
                None,
                None,
                false,
                cx,
            )
        });
        crate::store::tests::seed_full_scope(&workspace, &incoming, Vec::new(), cx);
        let message = |id: &str, author: Option<Author>| StoredEvent {
            origin: None,
            author,
            ts: Some(1),
            event: agent::AgentEvent::ItemCompleted(agent::ThreadItem {
                id: id.into(),
                parent_item_id: None,
                content: ItemContent::UserMessage {
                    text: id.into(),
                    context_len: None,
                    attachments: Vec::new(),
                },
            }),
            elided: None,
        };
        let snapshot = |records: Vec<StoredEvent>| EventEnvelope {
            request_id: None,
            topic: Topic::SessionEvents {
                session_id: "shared".into(),
            },
            event: ServerEvent::SessionSnapshot {
                from: 0,
                end: records.len() as u64,
                total: records.len() as u64,
                records,
                total_turns: 1,
                truncated: false,
            },
        };
        workspace.update(cx, |store, cx| {
            store.selected_session_id = Some("shared".into());
            store.apply_domain_event(&snapshot(vec![message("owner", None)]), cx);
            assert_eq!(store.message_byline("owner"), None);

            let member = Author {
                device_id: "member-device".into(),
                name: "Alex's laptop".into(),
            };
            store.apply_domain_event(&snapshot(vec![message("member", Some(member))]), cx);
            assert_eq!(
                store.message_byline("member").as_deref(),
                Some("Alex's laptop")
            );
            store.apply_domain_event(
                &EventEnvelope {
                    request_id: None,
                    topic: Topic::SessionEvents {
                        session_id: "shared".into(),
                    },
                    event: ServerEvent::SessionEvent(message("owner", None)),
                },
                cx,
            );
            assert_eq!(
                store.message_byline("owner").as_deref(),
                Some(crate::tr!("member.owner").as_ref())
            );
            assert_eq!(
                store.message_byline("member").as_deref(),
                Some("Alex's laptop")
            );

            // An orchestrator's message to its child is no member's, the owner's included.
            let mut lead = message("lead", None);
            lead.origin = Some(tcode_core::session::MessageOrigin::Agent);
            store.apply_domain_event(
                &EventEnvelope {
                    request_id: None,
                    topic: Topic::SessionEvents {
                        session_id: "shared".into(),
                    },
                    event: ServerEvent::SessionEvent(lead),
                },
                cx,
            );
            assert_eq!(
                store.message_byline("lead").as_deref(),
                Some(crate::tr!("chat.from_orchestrator").as_ref())
            );
        });
    }

    /// A thread is read only once its conversation has loaded and is on
    /// screen, and stays read through updates that land while it is shown. A
    /// view left before its conversation arrived reports nothing.
    #[gpui::test]
    fn a_thread_is_reported_read_once_its_conversation_loads(cx: &mut TestAppContext) {
        let (to_host, outgoing) = async_channel::unbounded();
        let (_incoming, from_host) = async_channel::unbounded();
        let deferred = std::iter::from_fn(|| from_host.try_recv().ok()).collect();
        let link = tcode_client::HostLink::new(to_host, from_host);
        let workspace = cx.new(|cx| {
            WorkspaceStore::new_attached(link, WorkspaceAttachment::Local, None, None, false, cx)
        });
        crate::store::tests::seed_full_scope(&workspace, &_incoming, deferred, cx);
        let reads = || {
            std::iter::from_fn(|| outgoing.try_recv().ok())
                .filter_map(|line| {
                    match tcode_protocol::decode_client_line(&line).unwrap().payload {
                        tcode_protocol::ClientPayload::Command(Command::MarkSessionRead {
                            session_id,
                            through,
                        }) => Some((session_id, through)),
                        _ => None,
                    }
                })
                .collect::<Vec<_>>()
        };
        let upsert = |id: &str, updated_at| {
            let mut meta =
                SessionMeta::new(ProviderKind::Codex, std::path::PathBuf::from("/tmp"), None);
            meta.id = id.into();
            meta.updated_at = updated_at;
            EventEnvelope {
                request_id: None,
                topic: Topic::Index,
                event: ServerEvent::IndexUpsertSession(meta),
            }
        };
        workspace.update(cx, |store, cx| {
            store.apply_domain_event(&upsert("left", 100), cx);
            store.apply_domain_event(&upsert("shown", 100), cx);
            store.settings_replica.last_visited =
                std::collections::HashMap::from([("left".into(), 50), ("shown".into(), 50)]);

            store.set_conversation_on_screen(true, cx);
            store.select_session("left".into());
            store.leave_session();
            store.apply_domain_event(&session_snapshot("left", 0, vec![reply(1)]), cx);
            assert_eq!(reads(), [], "left before the conversation loaded");

            store.select_session("shown".into());
            assert_eq!(reads(), [], "nothing has loaded yet");
            // A compact window went back to the thread list, which keeps the
            // thread selected, before the conversation arrived.
            store.set_conversation_on_screen(false, cx);
            store.apply_domain_event(&session_snapshot("shown", 0, vec![reply(1)]), cx);
            assert_eq!(reads(), [], "loaded behind the thread list");
            store.set_conversation_on_screen(true, cx);
            assert_eq!(reads(), [("shown".to_string(), 100)]);
            store.apply_domain_event(&upsert("left", 110), cx);
            assert_eq!(reads(), [], "unchanged for the thread on screen");

            store.apply_domain_event(&upsert("shown", 120), cx);
            assert_eq!(reads(), [("shown".to_string(), 120)]);
        });
    }

    #[gpui::test]
    fn history_prefetch_keeps_one_bounded_page_in_flight(cx: &mut TestAppContext) {
        let root = scratch_root("tcode-prefetch-test");
        let host = test_host(SessionStore::open_at(root.clone()).unwrap());
        let status = smol::block_on(host.update_state_for_test(|state, cx| {
            let id = state.start_draft("history".into(), std::env::temp_dir(), cx);
            state.session_status_snapshot(&id).unwrap()
        }))
        .unwrap();
        host.shutdown_blocking()
            .expect("stop unsubscribed fixture host");
        std::fs::remove_dir_all(root).unwrap();

        let (to_host, outgoing) = async_channel::unbounded();
        let (incoming, from_host) = async_channel::unbounded();
        let deferred = std::iter::from_fn(|| from_host.try_recv().ok()).collect();
        let link = tcode_client::HostLink::new(to_host, from_host);
        let pump_link = link.clone();
        let executor = cx.background_executor.clone();
        let _pump = cx.background_executor.spawn(async move {
            pump_link
                .pump_with_timer(|| executor.timer(std::time::Duration::from_millis(25)))
                .await;
        });
        let workspace = cx.new(|cx| {
            WorkspaceStore::new_attached(link, WorkspaceAttachment::Local, None, None, false, cx)
        });
        crate::store::tests::seed_full_scope(&workspace, &incoming, deferred, cx);
        workspace.update(cx, |store, cx| {
            store.selected_session_id = Some("large".into());
            store.session_status_replica = Some(status);
            store.apply_domain_event(
                &EventEnvelope {
                    request_id: None,
                    topic: Topic::SessionEvents {
                        session_id: "large".into(),
                    },
                    event: ServerEvent::SessionSnapshot {
                        from: 1800,
                        end: 2000,
                        records: (0..200)
                            .map(|_| {
                                agent::AgentEvent::Warning {
                                    message: "short".into(),
                                }
                                .into()
                            })
                            .collect(),
                        total: 2000,
                        total_turns: 1,
                        truncated: false,
                    },
                },
                cx,
            );
        });
        while outgoing.try_recv().is_ok() {}
        workspace.update(cx, |store, cx| store.update_history_window(1., true, cx));
        cx.run_until_parked();
        let mut request =
            tcode_protocol::decode_client_line(&outgoing.try_recv().unwrap()).unwrap();
        assert!(matches!(
            request.payload,
            tcode_protocol::ClientPayload::Query(tcode_protocol::Query::SessionHistoryPage {
                before: 1800,
                limit: 200,
                ..
            })
        ));
        workspace.update(cx, |store, cx| {
            assert!(store.history_loading());
            store.load_earlier_messages(cx);
        });
        cx.run_until_parked();
        assert!(
            outgoing.try_recv().is_err(),
            "scrolling while loading must not queue another page"
        );

        incoming
            .try_send(
                tcode_protocol::encode_line(&tcode_protocol::HostMessage::QueryResult {
                    id: request.id,
                    result: Err(tcode_protocol::ProtocolError::decode("offline")),
                })
                .unwrap(),
            )
            .unwrap();
        cx.run_until_parked();
        workspace.update(cx, |store, cx| {
            assert!(!store.history_loading(), "failed pages hide activity");
            store.load_earlier_messages(cx);
        });
        cx.executor()
            .advance_clock(std::time::Duration::from_secs(4));
        workspace.update(cx, |store, cx| store.load_earlier_messages(cx));
        cx.run_until_parked();
        assert!(
            outgoing.try_recv().is_err(),
            "retry is throttled for five seconds"
        );
        cx.executor()
            .advance_clock(std::time::Duration::from_secs(1));
        cx.run_until_parked();
        workspace.update(cx, |store, cx| store.load_earlier_messages(cx));
        cx.run_until_parked();
        let retry = tcode_protocol::decode_client_line(&outgoing.try_recv().unwrap()).unwrap();
        assert_eq!(
            retry.payload, request.payload,
            "retry requests the same bounded page"
        );
        request = retry;
        workspace.update(cx, |store, cx| {
            assert!(store.history_loading());
            store.load_earlier_messages(cx);
        });
        cx.run_until_parked();
        assert!(
            outgoing.try_recv().is_err(),
            "retry also permits one page in flight"
        );

        for page in 0..4 {
            let before = 1800 - page * 200;
            assert!(matches!(
                request.payload,
                tcode_protocol::ClientPayload::Query(tcode_protocol::Query::SessionHistoryPage {
                    before: requested_before,
                    limit: 200,
                    ..
                }) if requested_before == before
            ));
            let records = (0..100)
                .flat_map(|turn| {
                    let turn_id = format!("{page}-{turn}");
                    [
                        agent::AgentEvent::TurnStarted {
                            turn_id: turn_id.clone(),
                        }
                        .into(),
                        agent::AgentEvent::TurnCompleted {
                            turn_id,
                            status: agent::TurnStatus::Completed,
                            usage: None,
                        }
                        .into(),
                    ]
                })
                .collect();
            incoming
                .try_send(
                    tcode_protocol::encode_line(&tcode_protocol::HostMessage::QueryResult {
                        id: request.id,
                        result: Ok(tcode_protocol::QueryResponse::SessionHistoryPage {
                            records,
                            from: before - 200,
                            end: before,
                            truncated: false,
                        }),
                    })
                    .unwrap(),
                )
                .unwrap();
            wait_until(cx, &workspace, "prefetched page applied", |cx| {
                workspace.read_with(cx, |store, _| {
                    held_window(store, "large").is_some_and(|held| held.from == before - 200)
                })
            });
            assert!(
                outgoing.try_recv().is_err(),
                "yield between automatic pages"
            );
            cx.executor()
                .advance_clock(std::time::Duration::from_millis(250));
            cx.run_until_parked();
            workspace.update(cx, |store, cx| {
                store.update_history_window(if page < 3 { 2. + page as f32 } else { 6. }, true, cx);
            });
            cx.run_until_parked();
            if page < 3 {
                wait_until(cx, &workspace, "next prefetch request", |_| {
                    !outgoing.is_empty()
                });
                request =
                    tcode_protocol::decode_client_line(&outgoing.try_recv().unwrap()).unwrap();
            }
        }
        assert!(
            outgoing.try_recv().is_err(),
            "stop when six screens are covered, without a scroll event"
        );
        workspace.read_with(cx, |store, _| {
            assert_eq!(held_history(store, "large").from, 1000);
            assert_eq!(held_history(store, "large").records.len(), 1000);
            assert!(!store.history_loading());
        });
    }

    fn draft_status() -> tcode_protocol::SessionStatus {
        let root = scratch_root("tcode-draft-status-test");
        let host = test_host(SessionStore::open_at(root.clone()).unwrap());
        let status = smol::block_on(host.update_state_for_test(|state, cx| {
            let id = state.start_draft("running".into(), std::env::temp_dir(), cx);
            state.session_status_snapshot(&id).unwrap()
        }))
        .unwrap();
        host.shutdown_blocking()
            .expect("stop unsubscribed fixture host");
        std::fs::remove_dir_all(root).unwrap();
        status
    }

    fn with_running_turn(
        status: &tcode_protocol::SessionStatus,
        running: Option<tcode_core::session::RunningTurn>,
        question: Option<tcode_core::session::PendingUserInput>,
    ) -> tcode_protocol::SessionStatus {
        let mut status = status.clone();
        status.activity.turn_running = running.is_some();
        status.activity.working = running.is_some();
        status.running_turn = running;
        status.pending_user_input = question;
        status
    }

    fn question() -> tcode_core::session::PendingUserInput {
        tcode_core::session::PendingUserInput {
            request_id: "ask".into(),
            questions: vec![agent::UserInputQuestion {
                id: "which".into(),
                header: "Next".into(),
                question: "Which way?".into(),
                options: Vec::new(),
                multi_select: false,
                prefill: None,
            }],
            delivery: agent::UserInputDelivery::Blocking,
        }
    }

    fn recorded(ts: u64, event: AgentEvent) -> SessionEventRecord {
        SessionEventRecord {
            origin: None,
            author: None,
            ts: Some(ts),
            ..event.into()
        }
    }

    fn reply(ts: u64) -> SessionEventRecord {
        recorded(
            ts,
            AgentEvent::ItemCompleted(ThreadItem {
                id: format!("reply-{ts}"),
                parent_item_id: None,
                content: ItemContent::AssistantMessage {
                    text: "working".into(),
                },
            }),
        )
    }

    fn session_snapshot(
        session_id: &str,
        from: u64,
        records: Vec<SessionEventRecord>,
    ) -> EventEnvelope {
        let end = from + records.len() as u64;
        EventEnvelope {
            request_id: None,
            topic: Topic::SessionEvents {
                session_id: session_id.into(),
            },
            event: ServerEvent::SessionSnapshot {
                from,
                end,
                records,
                total: end,
                total_turns: 1,
                truncated: from > 0,
            },
        }
    }

    fn live_turn(workspace: &gpui::Entity<WorkspaceStore>, cx: &TestAppContext) -> Option<u64> {
        workspace.read_with(cx, |store, _| {
            store
                .with_active_timeline(|timeline| {
                    timeline
                        .turns
                        .last()
                        .filter(|turn| turn.running)
                        .and_then(|turn| turn.start_ts)
                })
                .flatten()
        })
    }

    #[gpui::test]
    fn the_status_settles_the_running_turn_whichever_topic_arrives_first(cx: &mut TestAppContext) {
        let status = draft_status();
        let id = status.session_id.clone();
        let status_event = |running: Option<u64>, question| EventEnvelope {
            request_id: None,
            topic: Topic::SessionStatus {
                session_id: id.clone(),
            },
            event: ServerEvent::SessionStatusReplaced(Box::new(with_running_turn(
                &status,
                running.map(|turn| tcode_core::session::RunningTurn {
                    turn,
                    started_at: Some(1_000),
                }),
                question,
            ))),
        };
        let snapshot = session_snapshot(
            &id,
            0,
            vec![
                recorded(
                    1_000,
                    AgentEvent::TurnStarted {
                        turn_id: "turn".into(),
                    },
                ),
                reply(2_000),
                recorded(
                    3_000,
                    AgentEvent::UserInputRequested {
                        request_id: "ask".into(),
                        questions: question().questions,
                        delivery: agent::UserInputDelivery::Blocking,
                    },
                ),
            ],
        );
        for status_first in [false, true] {
            let (to_host, _outgoing) = async_channel::unbounded();
            let (_incoming, from_host) = async_channel::unbounded();
            let deferred = std::iter::from_fn(|| from_host.try_recv().ok()).collect();
            let link = tcode_client::HostLink::new(to_host, from_host);
            let workspace = cx.new(|cx| {
                WorkspaceStore::new_attached(
                    link,
                    WorkspaceAttachment::Local,
                    None,
                    None,
                    false,
                    cx,
                )
            });
            crate::store::tests::seed_full_scope(&workspace, &_incoming, deferred, cx);
            let open_question = |cx: &TestAppContext| {
                workspace.read_with(cx, |store, _| {
                    store
                        .composer_state()
                        .pending_user_input
                        .map(|pending| pending.request_id)
                })
            };
            workspace.update(cx, |store, cx| {
                store.selected_session_id = Some(id.clone());
                // The status cached from the last visit, before this turn began.
                store.session_status_replica = Some(status.clone());
                if status_first {
                    store.apply_domain_event(&status_event(Some(0), Some(question())), cx);
                    store.apply_domain_event(&snapshot, cx);
                } else {
                    store.apply_domain_event(&snapshot, cx);
                    assert_eq!(store.with_active_timeline(|t| t.turn_running), Some(false));
                    store.apply_domain_event(&status_event(Some(0), Some(question())), cx);
                }
            });
            assert_eq!(live_turn(&workspace, cx), Some(1_000));
            assert_eq!(open_question(cx).as_deref(), Some("ask"));

            // The next turn runs before its opening record arrives.
            workspace.update(cx, |store, cx| {
                store.apply_domain_event(&status_event(Some(1), None), cx)
            });
            assert_eq!(live_turn(&workspace, cx), None);
            assert_eq!(
                workspace.read_with(cx, |store, _| store
                    .with_active_timeline(|timeline| timeline.turn_running)),
                Some(true)
            );

            workspace.update(cx, |store, cx| {
                store.apply_domain_event(&status_event(None, None), cx)
            });
            assert_eq!(live_turn(&workspace, cx), None);
            assert_eq!(open_question(cx), None);
        }
    }

    #[gpui::test]
    fn a_window_cut_inside_the_running_turn_is_timed_from_the_host(cx: &mut TestAppContext) {
        let status = with_running_turn(
            &draft_status(),
            Some(tcode_core::session::RunningTurn {
                turn: 0,
                started_at: Some(1_000),
            }),
            None,
        );
        let id = status.session_id.clone();
        let (to_host, outgoing) = async_channel::unbounded();
        let (_incoming, from_host) = async_channel::unbounded();
        let deferred = std::iter::from_fn(|| from_host.try_recv().ok()).collect();
        let link = tcode_client::HostLink::new(to_host, from_host);
        let workspace = cx.new(|cx| {
            WorkspaceStore::new_attached(link, WorkspaceAttachment::Local, None, None, false, cx)
        });
        crate::store::tests::seed_full_scope(&workspace, &_incoming, deferred, cx);
        workspace.update(cx, |store, cx| {
            store.selected_session_id = Some(id.clone());
            store.session_status_replica = Some(status);
            store.apply_domain_event(&session_snapshot(&id, 3, vec![reply(4_000)]), cx);
        });
        cx.run_until_parked();
        assert_eq!(live_turn(&workspace, cx), Some(1_000));
        assert!(
            !std::iter::from_fn(|| outgoing.try_recv().ok())
                .map(|line| tcode_protocol::decode_client_line(&line).unwrap())
                .any(|request| matches!(
                    request.payload,
                    tcode_protocol::ClientPayload::Query(
                        tcode_protocol::Query::SessionHistoryPage { .. }
                    )
                )),
            "the turn's start comes with the status, not from earlier pages"
        );
    }

    fn held_window<'a>(store: &'a WorkspaceStore, session_id: &str) -> Option<&'a HeldHistory> {
        store.threads.get(session_id)?.history.as_ref()
    }

    fn held_history<'a>(store: &'a WorkspaceStore, session_id: &str) -> &'a HeldHistory {
        held_window(store, session_id).expect("a held window")
    }

    fn test_host(store: SessionStore) -> SpawnedHost {
        spawn_host(store, HostServices::default()).expect("spawn test host")
    }

    macro_rules! update_host {
        ($host:expr, $update:expr) => {
            smol::block_on($host.update_state_for_test($update)).expect("update test host")
        };
    }

    pub(crate) struct LocalHostDriver {
        pub(crate) link: tcode_client::HostLink,
        pump: std::pin::Pin<Box<dyn std::future::Future<Output = ()>>>,
    }

    impl LocalHostDriver {
        pub(crate) fn new(host: &SpawnedHost) -> Self {
            let link = tcode_client::HostLink::new(host.to_host.clone(), host.from_host.clone());
            Self::from_link(link)
        }

        pub(crate) fn from_link(link: tcode_client::HostLink) -> Self {
            let pump_link = link.clone();
            Self {
                link,
                pump: Box::pin(async move { pump_link.pump().await }),
            }
        }

        fn workspace(&mut self, cx: &mut TestAppContext) -> gpui::Entity<WorkspaceStore> {
            let workspace = cx.new(|cx| {
                WorkspaceStore::new_attached(
                    self.link.clone(),
                    WorkspaceAttachment::Local,
                    None,
                    None,
                    false,
                    cx,
                )
            });
            self.wait_until(cx, &workspace, "workspace baseline", |cx| {
                workspace.read_with(cx, |store, _| store.baseline_ready())
            });
            workspace
        }

        pub(crate) fn await_with_events<T>(
            &mut self,
            cx: &mut TestAppContext,
            store: &gpui::Entity<WorkspaceStore>,
            future: impl std::future::Future<Output = T>,
        ) -> T {
            let mut future = std::pin::pin!(future);
            let mut answer = None;
            self.wait_until(cx, store, "host reply", |_| {
                let mut task_cx = std::task::Context::from_waker(std::task::Waker::noop());
                if let std::task::Poll::Ready(value) =
                    std::future::Future::poll(future.as_mut(), &mut task_cx)
                {
                    answer = Some(value);
                    true
                } else {
                    false
                }
            });
            answer.expect("reply observed")
        }

        pub(crate) fn step(
            &mut self,
            cx: &mut TestAppContext,
            store: &gpui::Entity<WorkspaceStore>,
        ) {
            // The real host sends from an OS thread. Route replies here so
            // asset and command futures wake on GPUI's test scheduler thread.
            let mut task_cx = std::task::Context::from_waker(std::task::Waker::noop());
            let _ = self.pump.as_mut().poll(&mut task_cx);
            store.update(cx, |store, cx| store.drain_host_events_for_test(cx));
            cx.run_until_parked();
        }

        pub(crate) fn wait_until(
            &mut self,
            cx: &mut TestAppContext,
            store: &gpui::Entity<WorkspaceStore>,
            description: &str,
            mut ready: impl FnMut(&TestAppContext) -> bool,
        ) {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            loop {
                self.step(cx, store);
                if ready(cx) {
                    return;
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "waiting for {description}"
                );
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
        }

        fn command(
            &mut self,
            cx: &mut TestAppContext,
            store: &gpui::Entity<WorkspaceStore>,
            command: Command,
        ) {
            let link = self.link.clone();
            self.await_with_events(cx, store, link.command(command))
                .expect("typed host command");
        }

        pub(crate) fn shutdown(
            &mut self,
            cx: &mut TestAppContext,
            store: &gpui::Entity<WorkspaceStore>,
            host: &SpawnedHost,
        ) {
            let link = self.link.clone();
            self.await_with_events(cx, store, link.shutdown())
                .expect("stop host");
            host.stopped.recv_blocking().expect("host stopped");
        }
    }

    pub(crate) fn await_with_events<T>(
        cx: &mut TestAppContext,
        workspace: &gpui::Entity<WorkspaceStore>,
        future: impl std::future::Future<Output = T>,
    ) -> T {
        let mut future = std::pin::pin!(future);
        let mut task_cx = std::task::Context::from_waker(std::task::Waker::noop());
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            workspace.update(cx, |store, cx| store.drain_host_events_for_test(cx));
            if let std::task::Poll::Ready(result) =
                std::future::Future::poll(future.as_mut(), &mut task_cx)
            {
                workspace.update(cx, |store, cx| store.drain_host_events_for_test(cx));
                return result;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "host request did not finish while consuming events"
            );
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    }

    pub(crate) fn shutdown_test_host(
        cx: &mut TestAppContext,
        workspace: &gpui::Entity<WorkspaceStore>,
        host: &SpawnedHost,
    ) {
        let link = host.link();
        await_with_events(cx, workspace, link.shutdown()).expect("drain test store and stop host");
        host.stopped.recv_blocking().expect("host stopped");
    }

    /// Drain the host's events into `workspace` until `ready`. A host
    /// delivers from its own threads, so a test waits on the condition, never
    /// unbounded: past the deadline it fails naming what it waited for.
    pub(crate) fn wait_until(
        cx: &mut TestAppContext,
        workspace: &gpui::Entity<WorkspaceStore>,
        description: &str,
        ready: impl Fn(&TestAppContext) -> bool,
    ) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while std::time::Instant::now() < deadline {
            workspace.update(cx, |store, cx| store.drain_host_events_for_test(cx));
            cx.run_until_parked();
            if ready(cx) {
                return;
            }
            smol::block_on(smol::Timer::after(std::time::Duration::from_millis(1)));
        }
        panic!("timed out waiting for {description}");
    }

    #[gpui::test]
    fn reconnect_on_a_delayed_link_restores_workspace_readiness(cx: &mut TestAppContext) {
        use tcode_client::{ConnectionFailure, ConnectionState};
        let root = scratch_root("delayed-reconnect");
        let disk = SessionStore::open_at(root.clone()).unwrap();
        disk.upsert_project(&project_at("p", &root)).unwrap();
        disk.upsert_meta(&thread(&root, "one", "p", None)).unwrap();
        let host = test_host(disk);
        let mux =
            tcode_traverse::mux::HostMux::new(host.attachments.clone(), host.from_host.clone());
        let connection = mux.attach(
            tcode_protocol::Principal::Full,
            tcode_client::attachment::AttachmentPolicy::network(),
        );
        let (incoming, from_host) = async_channel::unbounded();
        let (outgoing, requests) = tcode_client::outgoing::channel();
        let link = tcode_client::HostLink::new(outgoing, from_host);
        let state_link = link.clone();
        let replies = async move {
            while let Ok(line) = connection.from_host.recv().await {
                smol::Timer::after(std::time::Duration::from_millis(30)).await;
                if matches!(
                    state_link.connection_state(),
                    ConnectionState::Syncing { .. }
                ) {
                    state_link.set_connection_state(ConnectionState::Connected { path: None });
                }
                if incoming.send(line).await.is_err() {
                    break;
                }
            }
        };
        let mut driver = LocalHostDriver::from_link(link);
        let pump = driver.pump;
        // A busy transport may not read until the whole recovery flight is queued.
        let forwards = async move {
            while let Ok(line) = requests.recv().await {
                if tcode_client::outgoing::subscription_key(&line).is_none() {
                    requests.sent(&line);
                }
                if connection.to_host.send(line).await.is_err() {
                    break;
                }
            }
        };
        driver.pump = Box::pin(smol::future::race(
            pump,
            smol::future::race(forwards, replies),
        ));
        let workspace = cx.new(|cx| {
            WorkspaceStore::new_attached(
                driver.link.clone(),
                WorkspaceAttachment::Remote {
                    host_id: "test".into(),
                    host_name: "test".into(),
                },
                None,
                None,
                false,
                cx,
            )
        });
        driver.wait_until(cx, &workspace, "initial workspace baseline", |cx| {
            workspace.read_with(cx, |store, _| store.baseline_ready())
        });
        workspace.update(cx, |store, _| store.select_session("one".into()));
        driver.wait_until(cx, &workspace, "initial selected thread baseline", |cx| {
            workspace.read_with(cx, |store, _| store.baseline_ready())
        });
        driver
            .link
            .set_connection_state(ConnectionState::Reconnecting {
                attempt: 2,
                reason: Some(ConnectionFailure::Timeout),
            });
        cx.run_until_parked();
        assert!(!workspace.read_with(cx, |store, _| store.baseline_ready()));
        driver
            .link
            .set_connection_state(ConnectionState::Syncing { path: None });
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            driver.step(cx, &workspace);
            if workspace.read_with(cx, |store, _| store.baseline_ready()) {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "reconnect baseline stalled: {:?}",
                workspace.read_with(cx, |store, _| store.baseline_topics.clone())
            );
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        let link = driver.link.clone();
        driver
            .await_with_events(cx, &workspace, link.command(Command::ShutdownAllAndFlush))
            .unwrap();
        driver.link.close();
        drop(driver);
        // A remote client closes its attachment; the local owner closes the host.
        host.to_host.close();
        host.from_host.close();
        host.stopped.recv_blocking().unwrap();
        let _ = std::fs::remove_dir_all(root);
    }

    #[gpui::test]
    fn reconnect_replays_current_index_fingerprint_and_retains_replicas_until_baselines_arrive(
        cx: &mut TestAppContext,
    ) {
        use tcode_client::ConnectionState;
        let root = scratch_root("baseline-replay");
        let disk = SessionStore::open_at(root.clone()).unwrap();
        disk.upsert_project(&project_at("p", &root)).unwrap();
        disk.upsert_meta(&thread(&root, "one", "p", None)).unwrap();
        let host = test_host(disk);
        let (to_host, requests) = async_channel::unbounded::<String>();
        let replayed = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let recorded = replayed.clone();
        let host_sender = host.to_host.clone();
        let forwarder = std::thread::spawn(move || {
            while let Ok(line) = requests.recv_blocking() {
                recorded
                    .lock()
                    .unwrap()
                    .push(tcode_protocol::decode_client_line(&line).unwrap());
                if host_sender.send_blocking(line).is_err() {
                    break;
                }
            }
            host_sender.close();
        });
        let link = tcode_client::HostLink::new(to_host, host.from_host.clone());
        let mut driver = LocalHostDriver::from_link(link);
        let workspace = driver.workspace(cx);
        workspace.update(cx, |store, _| store.select_session("one".into()));
        driver.wait_until(cx, &workspace, "selected thread baseline", |cx| {
            workspace.read_with(cx, |store, _| store.baseline_ready())
        });
        let link = driver.link.clone();
        driver
            .await_with_events(
                cx,
                &workspace,
                link.command(Command::RenameSession {
                    session_id: "one".into(),
                    title: "Updated live".into(),
                }),
            )
            .unwrap();
        driver.wait_until(cx, &workspace, "live index upsert", |cx| {
            workspace.read_with(cx, |store, _| {
                store.index_replica.0[0].title == "Updated live"
            })
        });
        let fingerprint =
            workspace.read_with(cx, |store, _| store.cached_index_fingerprint().unwrap());
        replayed.lock().unwrap().clear();
        workspace.update(cx, |store, cx| {
            let state = ConnectionState::Reconnecting {
                attempt: 2,
                reason: None,
            };
            store.host.set_connection_state(state.clone());
            store.apply_connection_state(state);
            store.apply_domain_event(
                &EventEnvelope {
                    request_id: None,
                    topic: Topic::Scope,
                    event: ServerEvent::ScopeSnapshot(tcode_protocol::Scope::Full),
                },
                cx,
            );
            assert!(!store.baseline_ready());
            let state = ConnectionState::Syncing { path: None };
            store.host.set_connection_state(state.clone());
            store.apply_connection_state(state);
            assert!(!store.threads_loading(), "cached list remains visible");
            assert!(!store.chat_loading(), "cached thread remains visible");
            assert_eq!(store.active_session_id().as_deref(), Some("one"));
        });
        driver.wait_until(cx, &workspace, "correlated reconnect baselines", |cx| {
            workspace.read_with(cx, |store, _| store.connection_state().is_connected())
        });
        let subscriptions = replayed.lock().unwrap();
        let replay = subscriptions
            .iter()
            .find_map(|request| match &request.payload {
                tcode_protocol::ClientPayload::Subscribe(subscription)
                    if subscription.topic == Topic::Index =>
                {
                    Some(subscription)
                }
                _ => None,
            })
            .expect("replayed index subscription");
        assert_eq!(replay.known_index.as_ref(), Some(&fingerprint));
        drop(subscriptions);
        let reconnecting = ConnectionState::Reconnecting {
            attempt: 3,
            reason: None,
        };
        let syncing = ConnectionState::Syncing { path: None };
        driver.link.set_connection_state(reconnecting.clone());
        driver.link.set_connection_state(syncing.clone());
        let events = driver.link.events();
        let mut deferred_events = Vec::new();
        let scope = smol::block_on(smol::future::race(
            async {
                let mut scope = None;
                let mut unchanged = false;
                loop {
                    if unchanged && let Some(scope) = scope.take() {
                        break scope;
                    }
                    let mut next = std::pin::pin!(events.recv());
                    let event = std::future::poll_fn(|task_cx| {
                        let _ = driver.pump.as_mut().poll(task_cx);
                        next.as_mut().poll(task_cx)
                    })
                    .await
                    .unwrap();
                    if matches!(event.event, ServerEvent::ScopeSnapshot(_)) {
                        scope = Some(event);
                    } else {
                        if let ServerEvent::IndexUnchanged {
                            fingerprint: received,
                            ..
                        } = &event.event
                        {
                            assert_eq!(received, &fingerprint);
                            unchanged = true;
                        }
                        deferred_events.push(event);
                    }
                }
            },
            async {
                smol::Timer::after(std::time::Duration::from_secs(5)).await;
                panic!("recovery Scope and IndexUnchanged were not delivered");
            },
        ));
        assert!(matches!(scope.event, ServerEvent::ScopeSnapshot(_)));
        workspace.update(cx, |store, cx| {
            store.apply_domain_event(&scope, cx);
            assert!(
                !store.baseline_ready(),
                "Scope alone cannot make old baselines current"
            );
            store.apply_connection_state(reconnecting);
            store.apply_connection_state(syncing);
            assert!(
                !store.baseline_ready(),
                "other current baselines are still pending"
            );
            assert!(!store.chat_loading(), "cached thread remains visible");
            for event in deferred_events {
                store.apply_domain_event(&event, cx);
            }
        });
        driver.wait_until(
            cx,
            &workspace,
            "baselines after delayed state notifications",
            |cx| workspace.read_with(cx, |store, _| store.connection_state().is_connected()),
        );
        workspace.read_with(cx, |store, _| {
            assert_eq!(
                store.cached_index_fingerprint().as_ref(),
                Some(&fingerprint)
            );
            assert_eq!(store.index_replica.0[0].title, "Updated live");
        });
        driver.shutdown(cx, &workspace, &host);
        driver.link.close();
        forwarder.join().unwrap();
        let _ = std::fs::remove_dir_all(root);
    }

    /// A client that visits many threads keeps the replicas of the selected
    /// one and of the few it left last. Re-selecting a released thread asks
    /// for a baseline and holds exactly what a client opening it fresh holds;
    /// re-selecting a kept one sends its cursor and receives only what it
    /// missed. Records appended while a thread was away and live afterwards
    /// are each held once, and a thread deleted from the index is released at
    /// once.
    #[gpui::test]
    fn visiting_many_threads_keeps_the_replicas_of_the_last_few(cx: &mut TestAppContext) {
        let root = scratch_root("visited-threads");
        let disk = SessionStore::open_at(root.clone()).unwrap();
        let ids: Vec<String> = (0..50).map(|index| format!("thread-{index:02}")).collect();
        let answer = |id: &str, turn: usize| {
            AgentEvent::ItemCompleted(ThreadItem {
                id: format!("{id}-answer-{turn}"),
                parent_item_id: None,
                content: ItemContent::AssistantMessage {
                    text: format!("answer {turn} in {id}"),
                },
            })
        };
        let turn = |id: &str, turn: usize| {
            [
                AgentEvent::TurnStarted {
                    turn_id: turn.to_string(),
                },
                answer(id, turn),
                AgentEvent::TurnCompleted {
                    turn_id: turn.to_string(),
                    status: TurnStatus::Completed,
                    usage: None,
                },
            ]
        };
        disk.upsert_project(&project_at("p", &root)).unwrap();
        for id in &ids {
            disk.upsert_meta(&thread(&root, id, "p", None)).unwrap();
            let appends: Vec<_> = (0..3)
                .flat_map(|index| turn(id, index))
                .enumerate()
                .map(|(ts, event)| {
                    tcode_services::store::Mutation::append_event(id, ts as u64 + 1, &event)
                        .unwrap()
                })
                .collect();
            disk.apply(&appends).unwrap();
        }
        let host = test_host(disk);
        let mut driver = LocalHostDriver::new(&host);
        let workspace = driver.workspace(cx);
        let open = |driver: &mut LocalHostDriver,
                    cx: &mut TestAppContext,
                    workspace: &gpui::Entity<WorkspaceStore>,
                    id: &str| {
            workspace.update(cx, |store, _| store.select_session(id.into()));
            driver.wait_until(cx, workspace, id, |cx| {
                workspace.read_with(cx, |store, _| {
                    store.baseline_ready() && !store.session_loading() && !store.session_catching_up
                })
            });
        };
        let events_cursor = |store: &WorkspaceStore| {
            store
                .host
                .subscriptions()
                .into_iter()
                .find(|subscription| matches!(subscription.topic, Topic::SessionEvents { .. }))
                .map(|subscription| subscription.after)
        };
        let held_count = |store: &WorkspaceStore, id: &str, event: &AgentEvent| {
            held_history(store, id)
                .records
                .iter()
                .filter(|record| record.event == *event)
                .count()
        };
        for id in &ids {
            open(&mut driver, cx, &workspace, id);
        }
        workspace.read_with(cx, |store, _| {
            let mut held: Vec<&String> = store.threads.keys().collect();
            held.sort();
            assert_eq!(held, ids[45..].iter().collect::<Vec<_>>());
        });

        // A kept thread continues from its cursor even past what a baseline
        // would carry: the baseline would begin hundreds of records later.
        let kept = &ids[46];
        let end = workspace.read_with(cx, |store, _| held_history(store, kept).end);
        let missed: Vec<AgentEvent> = (3..153).flat_map(|index| turn(kept, index)).collect();
        update_host!(&host, {
            let (kept, missed) = (kept.clone(), missed.clone());
            move |state, cx| {
                for (offset, event) in missed.iter().enumerate() {
                    state.record_event_for_replica_test(&kept, 100 + offset as u64, event, cx);
                }
            }
        });
        workspace.update(cx, |store, _| {
            store.select_session(kept.clone());
            assert_eq!(events_cursor(store), Some(Some(end)));
        });
        let total = end + missed.len() as u64;
        driver.wait_until(cx, &workspace, "the kept thread's continuation", |cx| {
            workspace.read_with(cx, |store, _| {
                held_window(store, kept).is_some_and(|held| held.end == total)
                    && store.session_replica.is_some()
            })
        });
        let after_reselect = answer(kept, 153);
        update_host!(&host, {
            let (kept, after_reselect) = (kept.clone(), after_reselect.clone());
            move |state, cx| state.record_event_for_replica_test(&kept, 1000, &after_reselect, cx)
        });
        driver.wait_until(cx, &workspace, "the kept thread's live record", |cx| {
            workspace.read_with(cx, |store, _| {
                held_window(store, kept).is_some_and(|held| held.end == total + 1)
            })
        });
        workspace.read_with(cx, |store, _| {
            let held = held_history(store, kept);
            assert_eq!(held.from, 0, "a continuation, not a baseline");
            for event in [answer(kept, 0), answer(kept, 152), after_reselect.clone()] {
                assert_eq!(held_count(store, kept, &event), 1);
            }
        });

        // A released thread opens as on a fresh client, with what was
        // appended while it was away and what arrives live once it is open.
        let released = &ids[3];
        let away = answer(released, 3);
        let live = answer(released, 4);
        update_host!(&host, {
            let (released, away) = (released.clone(), away.clone());
            move |state, cx| state.record_event_for_replica_test(&released, 100, &away, cx)
        });
        workspace.update(cx, |store, _| {
            store.select_session(released.clone());
            assert_eq!(events_cursor(store), Some(None));
        });
        driver.wait_until(cx, &workspace, "the released thread's baseline", |cx| {
            workspace.read_with(cx, |store, _| {
                store.baseline_ready() && !store.session_loading()
            })
        });
        update_host!(&host, {
            let (released, live) = (released.clone(), live.clone());
            move |state, cx| state.record_event_for_replica_test(&released, 101, &live, cx)
        });
        driver.wait_until(cx, &workspace, "the live record", |cx| {
            workspace.read_with(cx, |store, _| {
                held_window(store, released).is_some_and(|held| held.end == 11)
            })
        });
        let fresh = driver.workspace(cx);
        open(&mut driver, cx, &fresh, released);
        let fresh = fresh.read_with(cx, |store, _| {
            (store.session_replica.clone(), store.session_turn_offset)
        });
        workspace.read_with(cx, |store, _| {
            assert_eq!(
                (store.session_replica.clone(), store.session_turn_offset),
                fresh
            );
            for event in [&away, &live] {
                assert_eq!(held_count(store, released, event), 1);
            }
        });

        let deleted = ids[47].clone();
        assert!(workspace.read_with(cx, |store, _| store.threads.contains_key(&deleted)));
        driver.command(
            cx,
            &workspace,
            Command::DeleteSession {
                session_id: deleted.clone(),
                remove_worktree: false,
            },
        );
        driver.wait_until(cx, &workspace, "the deletion", |cx| {
            workspace.read_with(cx, |store, _| {
                !store.index_replica.0.iter().any(|meta| meta.id == deleted)
            })
        });
        assert!(!workspace.read_with(cx, |store, _| store.threads.contains_key(&deleted)));

        driver.shutdown(cx, &workspace, &host);
        let _ = std::fs::remove_dir_all(&root);
    }

    pub(crate) fn scratch_root(label: &str) -> std::path::PathBuf {
        static NEXT_ROOT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        std::env::temp_dir().join(format!(
            "tcode-{label}-{}-{}-{}",
            std::process::id(),
            tcode_services::store::now_millis(),
            NEXT_ROOT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ))
    }

    fn project_at(id: &str, root: &std::path::Path) -> Project {
        Project {
            id: id.into(),
            name: id.into(),
            root: root.to_path_buf(),
            quick_chat: false,
            icon_path: None,
            permission_defaults: Default::default(),
            created_at: 0,
        }
    }

    fn thread(
        root: &std::path::Path,
        id: &str,
        project: &str,
        parent: Option<&str>,
    ) -> SessionMeta {
        let mut meta = SessionMeta::new(ProviderKind::Codex, root.to_path_buf(), None);
        meta.id = id.into();
        meta.project_id = Some(project.into());
        meta.parent_session_id = parent.map(str::to_string);
        meta
    }

    fn selected_status(
        cx: &TestAppContext,
        workspace: &gpui::Entity<WorkspaceStore>,
        session_id: &str,
    ) -> bool {
        workspace.read_with(cx, |store, _| {
            store
                .session_status_replica
                .as_ref()
                .is_some_and(|status| status.session_id == session_id)
        })
    }

    /// Leave user state in whatever project draft is on screen, so a later
    /// assertion can tell a cleared entry from a freshly defaulted one.
    fn mark_draft_state(cx: &mut TestAppContext, workspace: &gpui::Entity<WorkspaceStore>) {
        workspace.update(cx, |store, _| {
            let draft = store
                .conversation_ui
                .keys()
                .find(|destination| matches!(destination, ConversationDestination::ProjectDraft(_)))
                .cloned()
                .expect("draft conversation state");
            store
                .conversation_ui
                .get_mut(&draft)
                .expect("draft conversation state")
                .right_panel_open = true;
        });
    }

    fn assert_no_draft_state(cx: &mut TestAppContext, workspace: &gpui::Entity<WorkspaceStore>) {
        workspace.read_with(cx, |store, _| {
            let stranded: Vec<_> = store
                .conversation_ui
                .iter()
                .filter(|(destination, ui)| {
                    matches!(destination, ConversationDestination::ProjectDraft(_))
                        && ui.right_panel_open
                })
                .map(|(destination, _)| destination)
                .collect();
            assert!(
                stranded.is_empty(),
                "the deleted project's draft state outlived it: {stranded:?}"
            );
        });
    }

    /// Archived threads leave the index; the host still has them.
    fn archived(cx: &TestAppContext, workspace: &gpui::Entity<WorkspaceStore>, id: &str) -> bool {
        workspace.read_with(cx, |store, _| {
            !store.index_replica.0.iter().any(|meta| meta.id == id)
                && store.index_summary.archived_counts.values().sum::<usize>() > 0
        })
    }

    /// Deleting a thread deletes every thread under it, archived ones too,
    /// though the index carries no archived thread: the confirmation counts
    /// them all.
    #[gpui::test]
    fn delete_confirmation_counts_archived_descendants(cx: &mut TestAppContext) {
        let (to_host, requests) = async_channel::unbounded();
        let (replies, from_host) = async_channel::unbounded();
        let deferred = std::iter::from_fn(|| from_host.try_recv().ok()).collect();
        let link = tcode_client::HostLink::new(to_host, from_host);
        let store = cx.new(|cx| {
            WorkspaceStore::new_attached(
                link.clone(),
                WorkspaceAttachment::Local,
                None,
                None,
                false,
                cx,
            )
        });
        crate::store::tests::seed_full_scope(&store, &replies, deferred, cx);
        let mut pump = std::pin::pin!(link.pump());
        let mut task_cx = std::task::Context::from_waker(std::task::Waker::noop());
        let root = std::path::Path::new("/project");
        let archived = |id: &str, parent: &str| {
            let mut meta = thread(root, id, "p", Some(parent));
            meta.archived_at = Some(1);
            meta
        };
        store.update(cx, |store, cx| {
            store.apply_domain_event(
                &EventEnvelope {
                    request_id: None,
                    topic: Topic::Index,
                    event: ServerEvent::IndexSnapshot(tcode_protocol::IndexSnapshot {
                        summary: Default::default(),
                        sessions: vec![
                            thread(root, "parent", "p", None),
                            thread(root, "other", "p", None),
                        ],
                        projects: Vec::new(),
                    }),
                },
                cx,
            );
        });

        let count = std::rc::Rc::new(std::cell::Cell::new(None));
        let result = count.clone();
        store.update(cx, |store, cx| {
            let task = store.fetch_deletion_count("parent", cx);
            cx.spawn(async move |_, _| result.set(Some(task.await)))
                .detach();
        });
        cx.run_until_parked();
        let query = std::iter::from_fn(|| requests.try_recv().ok())
            .map(|line| tcode_protocol::decode_client_line(&line).unwrap())
            .find(|message| {
                message.payload
                    == tcode_protocol::ClientPayload::Query(tcode_protocol::Query::ArchivedSessions)
            })
            .expect("the count fetches the archived threads");
        replies
            .try_send(
                tcode_protocol::encode_line(&tcode_protocol::HostMessage::QueryResult {
                    id: query.id,
                    result: Ok(tcode_protocol::QueryResponse::ArchivedSessions(
                        tcode_protocol::ArchivedSessions {
                            sessions: vec![
                                archived("child", "parent"),
                                archived("grandchild", "child"),
                            ],
                            worktree_shared: Default::default(),
                            revision: 0,
                        },
                    )),
                })
                .unwrap(),
            )
            .unwrap();
        assert!(std::future::Future::poll(pump.as_mut(), &mut task_cx).is_pending());
        cx.run_until_parked();

        assert_eq!(count.get(), Some(3));
        link.close();
    }

    /// Delete all deletes each archived thread's tree: the confirmation counts
    /// the unarchived threads under them, and a thread under another archived
    /// thread is neither counted nor deleted twice.
    #[gpui::test]
    fn delete_all_archived_counts_unarchived_descendants_once(cx: &mut TestAppContext) {
        let root = std::path::Path::new("/project");
        let archived = |id: &str, parent: Option<&str>| {
            let mut meta = thread(root, id, "p", parent);
            meta.archived_at = Some(1);
            meta
        };
        let delete_all = |cx: &mut TestAppContext, index, archived| {
            let (to_host, requests) = async_channel::unbounded();
            let (replies, from_host) = async_channel::unbounded();
            let deferred = std::iter::from_fn(|| from_host.try_recv().ok()).collect();
            let link = tcode_client::HostLink::new(to_host, from_host);
            let store = cx.new(|cx| {
                WorkspaceStore::new_attached(
                    link.clone(),
                    WorkspaceAttachment::Local,
                    None,
                    None,
                    false,
                    cx,
                )
            });
            crate::store::tests::seed_full_scope(&store, &replies, deferred, cx);
            let mut pump = std::pin::pin!(link.pump());
            let mut task_cx = std::task::Context::from_waker(std::task::Waker::noop());
            // A thread on screen, so the empty-workspace draft does not open.
            store.update(cx, |store, cx| {
                store.select_session("other".into());
                store.apply_domain_event(
                    &EventEnvelope {
                        request_id: None,
                        topic: Topic::Index,
                        event: ServerEvent::IndexSnapshot(tcode_protocol::IndexSnapshot {
                            summary: Default::default(),
                            sessions: index,
                            projects: vec![project_at("p", root)],
                        }),
                    },
                    cx,
                );
                store.hold_archived_sessions(cx);
            });
            cx.run_until_parked();
            let query = std::iter::from_fn(|| requests.try_recv().ok())
                .map(|line| tcode_protocol::decode_client_line(&line).unwrap())
                .find(|message| {
                    message.payload
                        == tcode_protocol::ClientPayload::Query(
                            tcode_protocol::Query::ArchivedSessions,
                        )
                })
                .expect("the Archived page fetches the archived threads");
            replies
                .try_send(
                    tcode_protocol::encode_line(&tcode_protocol::HostMessage::QueryResult {
                        id: query.id,
                        result: Ok(tcode_protocol::QueryResponse::ArchivedSessions(
                            tcode_protocol::ArchivedSessions {
                                sessions: archived,
                                worktree_shared: Default::default(),
                                revision: 0,
                            },
                        )),
                    })
                    .unwrap(),
                )
                .unwrap();
            assert!(std::future::Future::poll(pump.as_mut(), &mut task_cx).is_pending());
            cx.run_until_parked();
            let deletion = store
                .read_with(cx, |store, _| store.archived_deletion())
                .expect("the archived threads are held");
            let counts = (deletion.archived, deletion.unarchived);
            store.update(cx, |store, _| store.delete_archived(deletion));
            cx.run_until_parked();
            let deleted: Vec<String> = std::iter::from_fn(|| requests.try_recv().ok())
                .filter_map(|line| {
                    match tcode_protocol::decode_client_line(&line).unwrap().payload {
                        tcode_protocol::ClientPayload::Command(Command::DeleteSession {
                            session_id,
                            ..
                        }) => Some(session_id),
                        _ => None,
                    }
                })
                .collect();
            link.close();
            (counts, deleted)
        };

        let (counts, deleted) = delete_all(
            cx,
            vec![
                thread(root, "child", "p", Some("parent")),
                thread(root, "other", "p", None),
            ],
            vec![archived("parent", None)],
        );
        assert_eq!((counts, deleted), ((1, 1), vec!["parent".to_string()]));
        let (counts, deleted) = delete_all(
            cx,
            vec![thread(root, "other", "p", None)],
            vec![archived("child", Some("parent")), archived("parent", None)],
        );
        assert_eq!((counts, deleted), ((2, 0), vec!["parent".to_string()]));
    }

    /// An Orchestrate child archived while on screen hands the workspace to
    /// its parent, keeping the parent's client-side records; archiving a thread
    /// the user is not viewing must not move them at all.
    #[gpui::test]
    fn archiving_the_viewed_child_returns_to_its_parent(cx: &mut TestAppContext) {
        let root = scratch_root("archive-to-parent");
        let disk = SessionStore::open_at(root.clone()).expect("open test store");
        disk.upsert_project(&project_at("p", &root))
            .expect("persist project");
        for meta in [
            thread(&root, "parent", "p", None),
            thread(&root, "child", "p", Some("parent")),
            thread(&root, "sibling", "p", None),
        ] {
            disk.upsert_meta(&meta).expect("persist session");
        }
        let host = test_host(disk);
        let mut driver = LocalHostDriver::new(&host);
        let workspace = cx.new(|cx| {
            WorkspaceStore::new_attached(
                driver.link.clone(),
                WorkspaceAttachment::Local,
                None,
                None,
                false,
                cx,
            )
        });

        workspace.update(cx, |store, _| store.select_session("parent".into()));
        driver.wait_until(cx, &workspace, "parent on screen", |cx| {
            selected_status(cx, &workspace, "parent")
                && workspace.read_with(cx, |store, _| held_window(store, "parent").is_some())
        });
        let parent_cursor =
            workspace.read_with(cx, |store, _| Some(held_history(store, "parent").end));
        workspace.update(cx, |store, _| store.select_session("child".into()));
        driver.wait_until(cx, &workspace, "child selected", |cx| {
            selected_status(cx, &workspace, "child")
        });

        driver.command(
            cx,
            &workspace,
            Command::ArchiveSession {
                session_id: "child".into(),
            },
        );
        driver.wait_until(cx, &workspace, "parent reopened", |cx| {
            selected_status(cx, &workspace, "parent")
        });
        workspace.read_with(cx, |store, _| {
            assert!(
                held_window(store, "parent").is_some(),
                "the parent's replicated records were dropped on the way back"
            );
            let events_cursor = store
                .host
                .subscriptions()
                .into_iter()
                .find(|subscription| matches!(subscription.topic, Topic::SessionEvents { .. }))
                .map(|subscription| subscription.after);
            assert_eq!(
                events_cursor,
                Some(parent_cursor),
                "the parent resumes from its cursor, not from nothing"
            );
        });
        // The index and its summary replicate on their own topics: the
        // return to the parent follows the index, the archived count the
        // summary, and nothing orders one before the other.
        driver.wait_until(cx, &workspace, "child archived", |cx| {
            archived(cx, &workspace, "child")
        });

        driver.command(
            cx,
            &workspace,
            Command::ArchiveSession {
                session_id: "sibling".into(),
            },
        );
        driver.wait_until(cx, &workspace, "sibling archived", |cx| {
            archived(cx, &workspace, "sibling")
        });
        assert!(
            selected_status(cx, &workspace, "parent"),
            "archiving a background thread moved the user"
        );

        driver.shutdown(cx, &workspace, &host);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Archiving a parent archives its children in one batch. Viewing one of
    /// those children leaves no visible parent to return to, so the workspace
    /// falls back to the standing draft of the last interacted project —
    /// the same draft session id, so its composer state survives.
    #[gpui::test]
    fn batch_archive_without_a_visible_parent_reopens_the_standing_draft(cx: &mut TestAppContext) {
        let root = scratch_root("archive-to-draft");
        let disk = SessionStore::open_at(root.clone()).expect("open test store");
        // "other" is listed first, so a draft for it proves nothing about the
        // remembered project; "p" is the one the user last worked in.
        disk.upsert_project(&project_at("other", &root.join("other")))
            .expect("persist project");
        disk.upsert_project(&project_at("p", &root))
            .expect("persist project");
        for meta in [
            thread(&root, "parent", "p", None),
            thread(&root, "child", "p", Some("parent")),
        ] {
            disk.upsert_meta(&meta).expect("persist session");
        }
        let host = test_host(disk);
        let mut driver = LocalHostDriver::new(&host);
        let workspace = cx.new(|cx| {
            WorkspaceStore::new_attached(
                driver.link.clone(),
                WorkspaceAttachment::Local,
                None,
                None,
                false,
                cx,
            )
        });

        // Let the launch fallback settle on the first project before the user
        // navigates into "p" themselves.
        driver.wait_until(cx, &workspace, "launch draft", |cx| {
            workspace.read_with(cx, |store, _| store.selected_session_id.is_some())
        });
        workspace.update(cx, |store, cx| {
            store.start_draft("p".into(), root.clone(), cx)
        });
        driver.wait_until(cx, &workspace, "draft for the last project", |cx| {
            workspace.read_with(cx, |store, _| {
                store
                    .session_status_replica
                    .as_ref()
                    .is_some_and(|status| status.draft && status.project_id.as_deref() == Some("p"))
            })
        });
        let draft_id = workspace
            .read_with(cx, |store, _| store.selected_session_id.clone())
            .expect("draft selected");
        workspace.update(cx, |store, _| {
            store
                .conversation_ui
                .get_mut(&ConversationDestination::ProjectDraft("p".into()))
                .expect("draft conversation state")
                .right_panel_open = true;
        });

        workspace.update(cx, |store, _| store.select_session("child".into()));
        driver.wait_until(cx, &workspace, "child selected", |cx| {
            selected_status(cx, &workspace, "child")
        });

        driver.command(
            cx,
            &workspace,
            Command::ArchiveSession {
                session_id: "parent".into(),
            },
        );
        driver.wait_until(cx, &workspace, "the standing draft reopened", |cx| {
            workspace.read_with(cx, |store, _| {
                store.selected_session_id.as_deref() == Some(draft_id.as_str())
            })
        });
        workspace.read_with(cx, |store, _| {
            assert!(
                store
                    .conversation_ui
                    .get(&ConversationDestination::ProjectDraft("p".into()))
                    .is_some_and(|ui| ui.right_panel_open),
                "the reopened draft lost the state the user left in it"
            );
        });
        assert!(archived(cx, &workspace, "parent"));
        assert!(archived(cx, &workspace, "child"));

        driver.shutdown(cx, &workspace, &host);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Launching with nothing selected opens the remembered project's new
    /// thread page instead of a dead empty page; a workspace with no project
    /// at all keeps its add-project state.
    #[gpui::test]
    fn a_workspace_with_no_conversation_opens_the_remembered_projects_draft(
        cx: &mut TestAppContext,
    ) {
        let root = scratch_root("remembered-project");
        let disk = SessionStore::open_at(root.clone()).expect("open test store");
        disk.upsert_project(&project_at("first", &root.join("first")))
            .expect("persist project");
        disk.upsert_project(&project_at("remembered", &root))
            .expect("persist project");
        tcode_services::settings::SettingsStore::new(root.clone())
            .save(&tcode_core::settings::Settings {
                last_project_id: Some("remembered".into()),
                ..Default::default()
            })
            .expect("persist remembered project");
        let host = test_host(disk);
        let mut driver = LocalHostDriver::new(&host);
        let workspace = cx.new(|cx| {
            WorkspaceStore::new_attached(
                driver.link.clone(),
                WorkspaceAttachment::Local,
                None,
                None,
                false,
                cx,
            )
        });
        driver.wait_until(cx, &workspace, "remembered project draft", |cx| {
            workspace.read_with(cx, |store, _| {
                store.session_status_replica.as_ref().is_some_and(|status| {
                    status.draft && status.project_id.as_deref() == Some("remembered")
                })
            })
        });
        driver.shutdown(cx, &workspace, &host);
        let _ = std::fs::remove_dir_all(&root);

        let empty_root = scratch_root("no-projects");
        let empty_host = test_host(SessionStore::open_at(empty_root.clone()).expect("open store"));
        let mut empty_driver = LocalHostDriver::new(&empty_host);
        let empty = cx.new(|cx| {
            WorkspaceStore::new_attached(
                empty_driver.link.clone(),
                WorkspaceAttachment::Local,
                None,
                None,
                false,
                cx,
            )
        });
        empty_driver.wait_until(cx, &empty, "empty workspace baseline", |cx| {
            empty.read_with(cx, |store, _| store.baseline_ready())
        });
        empty.read_with(cx, |store, _| {
            assert!(store.projects().is_empty());
            assert_eq!(
                store.selected_session_id, None,
                "a workspace with no project must stay on its add-project state"
            );
        });
        empty_driver.shutdown(cx, &empty, &empty_host);
        let _ = std::fs::remove_dir_all(&empty_root);
    }

    #[gpui::test]
    fn reconnect_preserves_exactly_one_copy_of_each_record(cx: &mut TestAppContext) {
        let root = scratch_root("p4a-reconnect");
        let disk = SessionStore::open_at(root.clone()).unwrap();
        let mut meta = SessionMeta::new(ProviderKind::Codex, root.clone(), None);
        meta.id = "reconnect".into();
        disk.upsert_meta(&meta).unwrap();
        let host = test_host(disk);
        let mut driver = LocalHostDriver::new(&host);
        let workspace = driver.workspace(cx);
        workspace.update(cx, |store, _| store.select_session("reconnect".into()));
        driver.wait_until(cx, &workspace, "selected status", |cx| {
            workspace.read_with(cx, |store, _| store.session_status_replica.is_some())
        });
        update_host!(&host, |state, cx| {
            for (ts, text) in [(1, "one"), (2, "two"), (3, "three")] {
                state.record_event_for_replica_test(
                    "reconnect",
                    ts,
                    &AgentEvent::Warning {
                        message: text.into(),
                    },
                    cx,
                );
            }
        });
        driver.wait_until(cx, &workspace, "three records", |cx| {
            workspace.read_with(cx, |store, _| {
                held_window(store, "reconnect").is_some_and(|held| held.records.len() == 3)
            })
        });
        driver
            .link
            .set_connection_state(tcode_client::ConnectionState::Reconnecting {
                attempt: 1,
                reason: None,
            });
        driver
            .link
            .set_connection_state(tcode_client::ConnectionState::Connected { path: None });
        driver.command(cx, &workspace, Command::ClearRelaunchMarker);
        workspace.update(cx, |store, cx| {
            store.drain_host_events_for_test(cx);
            assert_eq!(held_history(store, "reconnect").records.len(), 3);
        });
        workspace.read_with(cx, |store, _| {
            assert_eq!(
                held_history(store, "reconnect")
                    .records
                    .iter()
                    .map(|record| record.ts)
                    .collect::<Vec<_>>(),
                vec![Some(1), Some(2), Some(3)]
            );
            let subscription = store
                .host
                .subscriptions()
                .into_iter()
                .find(|sub| matches!(sub.topic, Topic::SessionEvents { .. }))
                .unwrap();
            assert_eq!(subscription.after, Some(3));
        });
        driver.shutdown(cx, &workspace, &host);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[gpui::test]
    fn session_replica_matches_live_timeline_for_synthetic_turn(cx: &mut TestAppContext) {
        let root = scratch_root("tcode-session-replica-consistency-test");
        let session_store = SessionStore::open_at(root.clone()).expect("open test store");
        let meta = SessionMeta::new(ProviderKind::Codex, root.join("worktree"), None);
        let session_id = meta.id.clone();
        session_store.upsert_meta(&meta).expect("persist session");
        let events = [
            AgentEvent::ItemCompleted(ThreadItem {
                id: "user-1".into(),
                parent_item_id: None,
                content: ItemContent::UserMessage {
                    text: "replicate this turn".into(),
                    context_len: None,
                    attachments: Vec::new(),
                },
            }),
            AgentEvent::TurnStarted {
                turn_id: "turn-1".into(),
            },
            AgentEvent::ItemCompleted(ThreadItem {
                id: "assistant-1".into(),
                parent_item_id: None,
                content: ItemContent::AssistantMessage {
                    text: "replicated".into(),
                },
            }),
            AgentEvent::TurnCompleted {
                turn_id: "turn-1".into(),
                status: TurnStatus::Completed,
                usage: None,
            },
        ];
        for (offset, event) in events.iter().enumerate() {
            session_store
                .append_event(&session_id, 100 + offset as u64, event)
                .expect("persist synthetic event");
        }

        let host = test_host(session_store);
        let mut driver = LocalHostDriver::new(&host);
        let workspace = driver.workspace(cx);
        workspace.update(cx, |store, _| store.select_session(session_id.clone()));
        driver.wait_until(cx, &workspace, "initial session timeline replica", |cx| {
            workspace.read_with(cx, |store, _| {
                store
                    .session_replica
                    .as_ref()
                    .is_some_and(|(id, timeline)| id == &session_id && timeline.turns.len() == 1)
            })
        });

        // The replica above is fed by the SessionEvents baseline, which the host
        // reads from the store independently of its own resident timeline. That
        // timeline is hydrated by a detached load; on slow disks (Windows CI) it
        // can still be empty here, so live events would land on nothing and the
        // snapshot below would miss turn-1.
        driver.wait_until(cx, &workspace, "host resident timeline hydration", |_| {
            let target_id = session_id.clone();
            update_host!(&host, move |state, _| {
                state
                    .residents
                    .live
                    .get(&target_id)
                    .is_some_and(|session| session.timeline.turns.len() == 1)
            })
        });

        let live_events = [
            AgentEvent::ItemCompleted(ThreadItem {
                id: "user-2".into(),
                parent_item_id: None,
                content: ItemContent::UserMessage {
                    text: "apply incrementally".into(),
                    context_len: None,
                    attachments: Vec::new(),
                },
            }),
            AgentEvent::TurnStarted {
                turn_id: "turn-2".into(),
            },
            AgentEvent::ItemCompleted(ThreadItem {
                id: "assistant-2".into(),
                parent_item_id: None,
                content: ItemContent::AssistantMessage {
                    text: "incremental replica".into(),
                },
            }),
            AgentEvent::TurnCompleted {
                turn_id: "turn-2".into(),
                status: TurnStatus::Completed,
                usage: None,
            },
        ];
        for (offset, event) in live_events.into_iter().enumerate() {
            let event_session_id = session_id.clone();
            update_host!(&host, move |state, cx| {
                state.record_event_for_replica_test(
                    &event_session_id,
                    200 + offset as u64,
                    &event,
                    cx,
                );
            });
        }
        let target_id = session_id.clone();
        let live = update_host!(&host, move |state, _| {
            let timeline = &state
                .residents
                .live
                .get(&target_id)
                .expect("selected session")
                .timeline;
            (
                timeline
                    .entries
                    .iter()
                    .map(|entry| (entry.id.clone(), entry.turn, format!("{:?}", entry.content)))
                    .collect::<Vec<_>>(),
                timeline.turns.len(),
            )
        });
        // Both timelines can have the same entry and turn counts before the
        // new events arrive. Wait for the final event before comparing contents.
        driver.wait_until(
            cx,
            &workspace,
            "incremental session timeline replica",
            |cx| {
                workspace.read_with(cx, |store, _| {
                    held_window(store, &session_id)
                        .and_then(|held| held.records.last())
                        .is_some_and(|record| {
                            matches!(
                                &record.event,
                                AgentEvent::TurnCompleted { turn_id, .. } if turn_id == "turn-2"
                            )
                        })
                })
            },
        );
        let replica = workspace.read_with(cx, |store, _| {
            let (id, timeline) = store.session_replica.as_ref().expect("session replica");
            assert_eq!(id, &session_id);
            (
                timeline
                    .entries
                    .iter()
                    .map(|entry| (entry.id.clone(), entry.turn, format!("{:?}", entry.content)))
                    .collect::<Vec<_>>(),
                timeline.turns.len(),
            )
        });
        assert_eq!(replica, live);

        driver.shutdown(cx, &workspace, &host);
        std::fs::remove_dir_all(root).expect("remove test data");
    }

    #[gpui::test]
    fn index_and_settings_replicas_follow_representative_commands(cx: &mut TestAppContext) {
        let root = scratch_root("tcode-replica-consistency-test");
        let session_store = SessionStore::open_at(root.clone()).expect("open test store");
        let seed_project = Project::from_root(root.join("seed"));
        let mut seed_session =
            SessionMeta::new(ProviderKind::Codex, seed_project.root.clone(), None);
        seed_session.project_id = Some(seed_project.id.clone());
        let seed_session_id = seed_session.id.clone();
        session_store
            .upsert_project(&seed_project)
            .expect("persist seed project");
        session_store
            .upsert_meta(&seed_session)
            .expect("persist seed session");
        let host = test_host(session_store);
        let mut driver = LocalHostDriver::new(&host);
        let workspace = driver.workspace(cx);
        driver.wait_until(cx, &workspace, "initial session index", |cx| {
            workspace.read_with(cx, |store, _| {
                store
                    .index_replica
                    .0
                    .iter()
                    .any(|meta| meta.id == seed_session_id)
            })
        });
        workspace.read_with(cx, |store, _| {
            assert!(
                store
                    .grouped_sessions()
                    .iter()
                    .any(|group| { group.sessions.iter().any(|meta| meta.id == seed_session_id) })
            );
            assert!(
                store
                    .flat_sessions()
                    .iter()
                    .any(|meta| meta.id == seed_session_id)
            );
            assert!(store.archived_groups().is_empty());
        });

        // The host validates a project root against its own filesystem, so this
        // directory has to exist before it will accept it.
        let created_root = root.join("created");
        std::fs::create_dir_all(&created_root).unwrap();
        driver.command(
            cx,
            &workspace,
            Command::CreateProject { root: created_root },
        );
        driver.command(
            cx,
            &workspace,
            Command::ArchiveSession {
                session_id: seed_session_id.clone(),
            },
        );
        let mut settings = workspace.read_with(cx, |store, _cx| store.settings());
        settings.word_wrap_diffs = !settings.word_wrap_diffs;
        let expected_word_wrap = settings.word_wrap_diffs;
        driver.command(
            cx,
            &workspace,
            Command::PatchSettings {
                patch: tcode_protocol::SettingsPatch::WordWrapDiffs(settings.word_wrap_diffs),
            },
        );
        driver.wait_until(cx, &workspace, "index and settings replicas", |cx| {
            workspace.read_with(cx, |store, _| {
                store.index_replica.1.len() == 2
                    && !store
                        .index_replica
                        .0
                        .iter()
                        .any(|meta| meta.id == seed_session_id)
                    && store.settings_replica.word_wrap_diffs == expected_word_wrap
            })
        });
        let link = driver.link.clone();
        let tcode_protocol::QueryResponse::ArchivedSessions(archived) = driver
            .await_with_events(
                cx,
                &workspace,
                link.query(tcode_protocol::Query::ArchivedSessions),
            )
            .expect("archived threads")
        else {
            panic!("archived threads")
        };
        assert!(
            archived
                .sessions
                .iter()
                .any(|meta| meta.id == seed_session_id)
        );

        workspace.read_with(cx, |store, _| {
            assert!(
                !store
                    .grouped_sessions()
                    .iter()
                    .any(|group| { group.sessions.iter().any(|meta| meta.id == seed_session_id) })
            );
            assert!(
                !store
                    .flat_sessions()
                    .iter()
                    .any(|meta| meta.id == seed_session_id)
            );
            assert_eq!(
                store
                    .project_summary(&seed_project.id)
                    .map(|(_, count)| count),
                Some(1),
                "the archived thread still counts toward its project"
            );
        });

        // Credential discovery can update settings after the command reply.
        // Let the UI consume those events before comparing the live replicas.
        driver.wait_until(
            cx,
            &workspace,
            "index and settings equal to live state",
            |cx| {
                let (live_index, live_settings) = update_host!(&host, |state, _| {
                    let index = state.index_snapshot();
                    (
                        (
                            serde_json::to_value(&index.sessions).unwrap(),
                            serde_json::to_value(&index.projects).unwrap(),
                        ),
                        serde_json::to_value(&state.settings).unwrap(),
                    )
                });
                workspace.read_with(cx, |store, _| {
                    serde_json::to_value(&store.index_replica.0).unwrap() == live_index.0
                        && serde_json::to_value(&store.index_replica.1).unwrap() == live_index.1
                        && serde_json::to_value(&store.settings_replica).unwrap() == live_settings
                })
            },
        );

        driver.shutdown(cx, &workspace, &host);
        std::fs::remove_dir_all(root).expect("remove test data");
    }

    #[gpui::test]
    fn session_status_replica_matches_live_after_queue_change(cx: &mut TestAppContext) {
        let root = scratch_root("tcode-session-status-replica-consistency-test");
        let session_store = SessionStore::open_at(root.clone()).expect("open test store");
        let meta = SessionMeta::new(ProviderKind::Codex, root.join("worktree"), None);
        let session_id = meta.id.clone();
        session_store.upsert_meta(&meta).expect("persist session");

        let host = test_host(session_store);
        let mut driver = LocalHostDriver::new(&host);
        let workspace = driver.workspace(cx);
        workspace.update(cx, |store, _| store.select_session(session_id.clone()));
        driver.wait_until(cx, &workspace, "selected session status", |cx| {
            workspace.read_with(cx, |store, _| {
                store
                    .session_status_replica
                    .as_ref()
                    .is_some_and(|status| status.session_id == session_id)
            })
        });
        let scripted = tcode_runtime::app::scripted_provider(ProviderKind::Codex);
        update_host!(&host, move |state, _| state
            .set_provider_launcher_for_test(scripted.launcher));
        driver.command(
            cx,
            &workspace,
            Command::SendTurn {
                session_id: session_id.clone(),
                text: "queued for replication".into(),
                attachment_paths: Vec::new(),
            },
        );
        driver.command(
            cx,
            &workspace,
            Command::AddReviewComment {
                session_id: session_id.clone(),
                comment: ReviewComment::new(
                    "src/lib.rs".into(),
                    4,
                    4,
                    ReviewSide::New,
                    "Replicated review draft".into(),
                    "+changed".into(),
                    "turn:0".into(),
                    "Turn 1".into(),
                    0,
                    1,
                ),
            },
        );
        // The scripted provider keeps moving the host (delivery, steering
        // support) after the queue change, so the replica is compared with the
        // live status of the same moment rather than a later snapshot.
        driver.wait_until(cx, &workspace, "replica equal to the live status", |cx| {
            let live_id = session_id.clone();
            let live = update_host!(&host, move |state, _| {
                state
                    .session_status_snapshot(&live_id)
                    .expect("live session status")
            });
            workspace.read_with(cx, |store, _| {
                store.session_status_replica.as_ref().is_some_and(|status| {
                    status.queued_messages.len() == 1
                        && status.review_comment_drafts.len() == 1
                        && *status == live
                })
            })
        });
        let replica = workspace.read_with(cx, |store, _| {
            store
                .session_status_replica
                .clone()
                .expect("session status replica")
        });

        assert_eq!(replica.queued_messages.len(), 1);
        assert_eq!(replica.queued_messages[0].text, "queued for replication");
        assert_eq!(replica.review_comment_drafts.len(), 1);
        assert_eq!(
            workspace.read_with(cx, |store, _cx| store.review_comments()),
            replica.review_comment_drafts
        );

        driver.shutdown(cx, &workspace, &host);
        std::fs::remove_dir_all(root).expect("remove test data");
    }

    #[gpui::test]
    fn native_rewind_prefill_events_remain_keyed_to_parked_sessions(cx: &mut TestAppContext) {
        let root = scratch_root("tcode-native-rewind-replica-test");
        let session_store = SessionStore::open_at(root.clone()).expect("open test store");
        let first = SessionMeta::new(ProviderKind::ClaudeCode, root.join("first"), None);
        let second = SessionMeta::new(ProviderKind::ClaudeCode, root.join("second"), None);
        session_store
            .upsert_meta(&first)
            .expect("persist first session");
        session_store
            .upsert_meta(&second)
            .expect("persist second session");

        let host = test_host(session_store);
        let mut driver = LocalHostDriver::new(&host);
        let workspace = driver.workspace(cx);
        workspace.update(cx, |store, _| store.select_session(first.id.clone()));
        driver.wait_until(cx, &workspace, "first selected session", |cx| {
            workspace.read_with(cx, |store, _| {
                store
                    .session_status_replica
                    .as_ref()
                    .is_some_and(|status| status.session_id == first.id)
            })
        });

        // This replica test deliberately owns both status subscriptions. Ordinary
        // navigation owns only its selected session; mux isolation is tested separately.
        driver
            .link
            .subscribe(tcode_protocol::Subscription {
                known_index: None,
                topic: Topic::SessionStatus {
                    session_id: second.id.clone(),
                },
                after: None,
            })
            .unwrap();
        for (session_id, text) in [
            (first.id.clone(), "first parked prefill".to_string()),
            (second.id.clone(), "second parked prefill".to_string()),
        ] {
            update_host!(&host, move |_state, cx| {
                cx.emit(HostEvent::Domain(EventEnvelope {
                    request_id: None,
                    topic: Topic::SessionStatus {
                        session_id: session_id.clone(),
                    },
                    event: ServerEvent::NativeRewindPrefill { session_id, text },
                }));
            });
        }
        driver.wait_until(cx, &workspace, "both rewind prefill events", |cx| {
            workspace.read_with(cx, |store, _| store.native_rewind_prefills.len() == 2)
        });

        assert_eq!(
            workspace.update(cx, |store, _cx| store.take_native_rewind_prefill()),
            Some("first parked prefill".into())
        );
        workspace.update(cx, |store, _| store.select_session(second.id.clone()));
        driver.wait_until(cx, &workspace, "second selected session", |cx| {
            workspace.read_with(cx, |store, _| {
                store
                    .session_status_replica
                    .as_ref()
                    .is_some_and(|status| status.session_id == second.id)
            })
        });
        assert_eq!(
            workspace.update(cx, |store, _cx| store.take_native_rewind_prefill()),
            Some("second parked prefill".into())
        );

        driver.shutdown(cx, &workspace, &host);
        std::fs::remove_dir_all(root).expect("remove test data");
    }

    #[gpui::test]
    fn classifier_stop_and_review_preserve_diagnostics_until_the_next_turn(
        cx: &mut TestAppContext,
    ) {
        let root = scratch_root("tcode-fallback-lifecycle-test");
        let session_store = SessionStore::open_at(root.clone()).expect("open test store");
        let meta = SessionMeta::new(ProviderKind::ClaudeCode, root.join("worktree"), None);
        session_store.upsert_meta(&meta).expect("persist session");

        let host = test_host(session_store);
        let mut driver = LocalHostDriver::new(&host);
        let workspace = driver.workspace(cx);
        workspace.update(cx, |store, _| store.select_session(meta.id.clone()));
        driver.wait_until(cx, &workspace, "selected session baseline", |cx| {
            workspace.read_with(cx, |store, _| store.baseline_ready())
        });

        let session_id = meta.id.clone();
        update_host!(&host, move |_state, cx| {
            cx.emit(HostEvent::Domain(EventEnvelope {
                request_id: None,
                topic: Topic::SessionStatus {
                    session_id: session_id.clone(),
                },
                event: ServerEvent::ModelFallbackBlocked {
                    session_id,
                    category: Some(agent::ClassifierCategory::Cyber),
                    model: Some("claude-sonnet-4-5".into()),
                    fallback_model: None,
                    detail: "request blocked by classifier".into(),
                },
            }));
        });
        driver.wait_until(cx, &workspace, "classifier block", |cx| {
            workspace.read_with(cx, |store, _| store.active_fallback_block().is_some())
        });

        let session_id = meta.id.clone();
        update_host!(&host, move |_state, cx| {
            cx.emit(HostEvent::Domain(EventEnvelope {
                request_id: None,
                topic: Topic::SessionStatus {
                    session_id: session_id.clone(),
                },
                event: ServerEvent::FallbackReviewReady {
                    session_id,
                    assessment: "looks like a false positive".into(),
                    draft: "I am auditing my own service.".into(),
                },
            }));
        });
        driver.wait_until(cx, &workspace, "review ready", |cx| {
            workspace.read_with(cx, |store, _| store.active_fallback_review().is_some())
        });
        workspace.read_with(cx, |store, _| {
            let block = store.active_fallback_block().unwrap();
            assert_eq!(block.category, Some(agent::ClassifierCategory::Cyber));
            assert_eq!(block.model.as_deref(), Some("claude-sonnet-4-5"));
            assert_eq!(block.detail, "request blocked by classifier");
            let review = store.active_fallback_review().unwrap();
            assert_eq!(review.assessment, "looks like a false positive");
            assert_eq!(review.draft, "I am auditing my own service.");
        });

        let session_id = meta.id.clone();
        update_host!(&host, move |state, cx| {
            state.record_event_for_replica_test(
                &session_id,
                1,
                &AgentEvent::TurnStarted {
                    turn_id: "turn-next".into(),
                },
                cx,
            );
        });
        driver.wait_until(
            cx,
            &workspace,
            "block and review cleared by the next turn",
            |cx| {
                workspace.read_with(cx, |store, _| {
                    store.active_fallback_block().is_none()
                        && store.active_fallback_review().is_none()
                })
            },
        );

        driver.shutdown(cx, &workspace, &host);
        std::fs::remove_dir_all(root).expect("remove test data");
    }

    #[gpui::test]
    fn providers_and_git_replicas_match_live_after_representative_mutations(
        cx: &mut TestAppContext,
    ) {
        let root = scratch_root("tcode-provider-git-replica-consistency-test");
        let session_store = SessionStore::open_at(root.clone()).expect("open test store");
        let mut meta = SessionMeta::new(ProviderKind::Codex, root.join("worktree"), None);
        meta.id = "git-replica".into();
        session_store.upsert_meta(&meta).unwrap();
        let host = test_host(session_store);
        let mut driver = LocalHostDriver::new(&host);
        let workspace = driver.workspace(cx);

        workspace.update(cx, |store, _| store.select_session("git-replica".into()));
        // Subscribing adopts the session and spawns a real git probe of the
        // (non-repo) cwd. Let that probe land before injecting the fixture
        // status, or its late result overwrites the injected one.
        driver.wait_until(cx, &workspace, "initial git probe", |cx| {
            workspace.read_with(cx, |store, _| store.git_status_replica.status.is_some())
        });
        driver.command(cx, &workspace, Command::ClearRelaunchMarker);
        update_host!(&host, |state, _cx| {
            state.acp_registry = Some(
                serde_json::from_value(serde_json::json!({
                    "agents": [{
                        "id": "replicated-agent",
                        "name": "Replicated Agent",
                        "version": "1.0.0",
                        "description": "registry refresh result",
                        "distribution": { "npx": { "package": "replicated-agent" } }
                    }]
                }))
                .expect("registry fixture"),
            );
            state.acp_registry_loading = false;
            state.acp_registry_error = None;
            state
                .providers
                .provider_versions
                .entry(ProviderKind::Codex)
                .or_default()
                .checking = true;
            state.git_status.insert(
                "git-replica".into(),
                GitStatus {
                    is_repo: true,
                    branch: Some("feature/replica".into()),
                    has_working_tree_changes: true,
                    changed_files: vec![GitFileEntry {
                        path: "src/replica.rs".into(),
                        insertions: 4,
                        deletions: 2,
                    }],
                    ..Default::default()
                },
            );
            state.git_busy.insert("git-replica".into());
        });
        driver.wait_until(cx, &workspace, "provider and git replicas", |cx| {
            workspace.read_with(cx, |store, _| {
                store.providers_replica.providers_checking
                    && store
                        .git_status_replica
                        .status
                        .as_ref()
                        .is_some_and(|status| status.branch.as_deref() == Some("feature/replica"))
            })
        });

        let (live_providers, live_git) = update_host!(&host, |state, _| {
            (
                state.providers_status_snapshot(),
                state.git_status_snapshot("git-replica"),
            )
        });
        let (replica_providers, replica_git) = workspace.read_with(cx, |store, _| {
            (
                store.providers_replica.clone(),
                store.git_status_replica.clone(),
            )
        });

        assert_eq!(replica_providers, live_providers);
        assert_eq!(replica_git, live_git);
        assert!(replica_providers.providers_checking);
        assert_eq!(
            replica_providers.acp_marketplace_items[0].id,
            "replicated-agent"
        );
        assert_eq!(
            replica_git.status.expect("git replica").changed_files[0].path,
            "src/replica.rs"
        );

        driver.shutdown(cx, &workspace, &host);
        std::fs::remove_dir_all(root).expect("remove test data");
    }

    /// A draft's client state follows its project, so deleting the project
    /// takes the draft's `ConversationUiState` with it instead of stranding an
    /// entry keyed by the transient draft session id.
    #[gpui::test]
    fn removing_a_project_clears_draft_state_and_reconciles_only_its_active_view(
        cx: &mut TestAppContext,
    ) {
        for viewing_draft in [false, true] {
            let root = scratch_root("remove-project-draft-ui");
            let disk = SessionStore::open_at(root.clone()).expect("open test store");
            for project in ["doomed", "kept"] {
                disk.upsert_project(&project_at(project, &root))
                    .expect("persist project");
            }
            disk.upsert_meta(&thread(&root, "kept-thread", "kept", None))
                .expect("persist session");
            let host = test_host(disk);
            let mut driver = LocalHostDriver::new(&host);
            let workspace = driver.workspace(cx);

            workspace.update(cx, |store, cx| {
                store.start_draft("doomed".into(), root.clone(), cx)
            });
            driver.wait_until(cx, &workspace, "draft for the doomed project", |cx| {
                workspace.read_with(cx, |store, _| {
                    store.session_status_replica.as_ref().is_some_and(|status| {
                        status.draft && status.project_id.as_deref() == Some("doomed")
                    })
                })
            });
            mark_draft_state(cx, &workspace);

            if !viewing_draft {
                workspace.update(cx, |store, _| store.select_session("kept-thread".into()));
                driver.wait_until(cx, &workspace, "kept thread selected", |cx| {
                    selected_status(cx, &workspace, "kept-thread")
                });
            }
            workspace.update(cx, |store, _| store.delete_project("doomed".into()));
            driver.wait_until(cx, &workspace, "project removed", |cx| {
                workspace.read_with(cx, |store, _| {
                    !store
                        .index_replica
                        .1
                        .iter()
                        .any(|project| project.id == "doomed")
                })
            });
            assert_no_draft_state(cx, &workspace);
            if viewing_draft {
                driver.wait_until(cx, &workspace, "kept project's draft", |cx| {
                    workspace.read_with(cx, |store, _| {
                        store.session_status_replica.as_ref().is_some_and(|status| {
                            status.draft && status.project_id.as_deref() == Some("kept")
                        })
                    })
                });
            } else {
                assert!(
                    selected_status(cx, &workspace, "kept-thread"),
                    "deleting a background project moved the user"
                );
            }

            driver.shutdown(cx, &workspace, &host);
            let _ = std::fs::remove_dir_all(&root);
        }
    }

    /// Committing a draft into a real session moves its client state from the
    /// project-draft key to the new thread key, so the user keeps the panel
    /// layout they were typing in.
    #[gpui::test]
    fn committing_a_draft_carries_its_conversation_state_to_the_thread(cx: &mut TestAppContext) {
        let root = scratch_root("commit-draft-ui");
        let disk = SessionStore::open_at(root.clone()).expect("open test store");
        disk.upsert_project(&project_at("p", &root))
            .expect("persist project");
        let host = test_host(disk);
        let mut driver = LocalHostDriver::new(&host);
        let workspace = driver.workspace(cx);

        workspace.update(cx, |store, cx| {
            store.start_draft("p".into(), root.clone(), cx)
        });
        driver.wait_until(cx, &workspace, "draft for p", |cx| {
            workspace.read_with(cx, |store, _| {
                store
                    .session_status_replica
                    .as_ref()
                    .is_some_and(|status| status.draft && status.project_id.as_deref() == Some("p"))
            })
        });
        let draft_id = workspace
            .read_with(cx, |store, _| store.selected_session_id.clone())
            .expect("draft selected");
        workspace.update(cx, |store, _| {
            store
                .conversation_ui
                .get_mut(&ConversationDestination::ProjectDraft("p".into()))
                .expect("draft conversation state")
                .right_panel_open = true;
        });

        let provider = workspace.read_with(cx, |store, _| {
            store.session_status_replica.as_ref().unwrap().provider
        });
        let scripted = tcode_runtime::app::scripted_provider(provider);
        let launcher = scripted.launcher.clone();
        update_host!(&host, move |state, _| state
            .set_provider_launcher_for_test(launcher));
        driver.command(
            cx,
            &workspace,
            Command::SendTurn {
                session_id: draft_id.clone(),
                text: "first turn".into(),
                attachment_paths: Vec::new(),
            },
        );
        driver.wait_until(cx, &workspace, "draft committed", |cx| {
            workspace.read_with(cx, |store, _| {
                store
                    .session_status_replica
                    .as_ref()
                    .is_some_and(|status| !status.draft && status.session_id == draft_id)
            })
        });
        workspace.read_with(cx, |store, _| {
            assert!(
                store
                    .conversation_ui
                    .get(&ConversationDestination::Thread(draft_id.clone()))
                    .is_some_and(|ui| ui.right_panel_open),
                "the committed thread lost the state the user left in its draft"
            );
            assert!(
                !store
                    .conversation_ui
                    .contains_key(&ConversationDestination::ProjectDraft("p".into())),
                "the committed draft's state was copied instead of moved"
            );
        });

        driver.shutdown(cx, &workspace, &host);
        let _ = std::fs::remove_dir_all(&root);
    }
}
