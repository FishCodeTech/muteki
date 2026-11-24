use gpui::{Context, Entity, EventEmitter, SharedString};
use serde::{Deserialize, Serialize};

use crate::store::WorkspaceStore;

/// One place the window can be. The window keeps a *history* of them, and every
/// Back — the toolbar control, the Android gesture, the desktop Back row — pops
/// that one history. There is no second navigation authority: the compact
/// navigation stack mirrors it, and the wide layout derives its [`Route`] from
/// whichever destination is on top.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Destination {
    /// Which host this window talks to: the root, and a place that can also be
    /// *visited* from an attached workspace without detaching from it.
    Hosts,
    /// The pairing form, pushed from Hosts (or from a repair row that
    /// prefilled it).
    Pair,
    Threads,
    Thread,
    /// The thread's terminal, diff/plan or preview, full width.
    Panel,
    /// One of the thread's pull requests, full width.
    PullRequest,
    /// The settings root: the section list in compact, the whole route in wide.
    Settings,
    /// One settings section's detail. Which section it is belongs to the page;
    /// *that a detail is open* is navigation and belongs here.
    SettingsSection,
    SettingsThreadRules,
    /// The phone's page for pinning, settling and reordering threads by handle.
    ArrangeThreads,
}

/// The shell's client-local checkpoint, independent of the host's settings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct NavigationSnapshot {
    pub history: Vec<Destination>,
    pub host_id: Option<String>,
    pub session_id: Option<String>,
    #[serde(default)]
    pub panel: NavigationPanel,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum NavigationPanel {
    Terminal,
    #[default]
    Diff,
    Plan,
    Agents,
    PullRequests,
    Preview,
}

impl NavigationPanel {
    pub fn id(self) -> &'static str {
        match self {
            Self::Terminal => "terminal",
            Self::Diff => "diff",
            Self::Plan => "plan",
            Self::Agents => "agents",
            Self::PullRequests => "pr",
            Self::Preview => "preview",
        }
    }
}

impl NavigationSnapshot {
    pub fn from_preferences(value: serde_json::Value) -> Option<Self> {
        let mut snapshot: Self = serde_json::from_value(value).ok()?;
        // Ignore malformed/unbounded local state rather than building an
        // arbitrary number of mounted views during startup.
        if snapshot.history.len() > 128 {
            return None;
        }
        snapshot.history.retain(|destination| {
            !matches!(
                destination,
                Destination::Settings
                    | Destination::SettingsSection
                    | Destination::SettingsThreadRules
                    | Destination::ArrangeThreads
                    | Destination::Pair
                    // A cold start reads the pull request afresh from its thread.
                    | Destination::PullRequest
            )
        });
        if snapshot.history.first() != Some(&Destination::Hosts) {
            snapshot.history.insert(0, Destination::Hosts);
        }
        snapshot.history.dedup();
        if snapshot.host_id.is_none() {
            snapshot.without_host();
        } else if snapshot.session_id.is_none() {
            snapshot.without_thread();
        }
        Some(snapshot)
    }

    pub fn without_host(&mut self) {
        self.host_id = None;
        self.session_id = None;
        self.history = vec![Destination::Hosts];
    }

    fn without_thread(&mut self) {
        self.session_id = None;
        if let Some(index) = self.history.iter().position(|destination| {
            matches!(
                destination,
                Destination::Thread | Destination::Panel | Destination::PullRequest
            )
        }) {
            self.history.truncate(index);
        }
    }
}

impl Destination {
    /// Which wide surface shows this destination. The wide layout shows a whole
    /// hierarchy at once, so several destinations share one route.
    pub fn route(self) -> Route {
        match self {
            Self::Hosts | Self::Pair => Route::Hosts,
            Self::Settings | Self::SettingsSection | Self::SettingsThreadRules => Route::Settings,
            Self::Threads
            | Self::Thread
            | Self::Panel
            | Self::PullRequest
            | Self::ArrangeThreads => Route::Chat,
        }
    }

    /// The short, fixed label a Back control carries when this destination is
    /// the one it returns to. Deliberately not the page's own title: a Back
    /// control names a kind of place, so it stays the same width whatever the
    /// machine or thread underneath is called, and never truncates. Pair is
    /// reached from Machines and answers with its caller's label.
    pub fn back_label(self) -> SharedString {
        crate::tr!(match self {
            Self::Hosts | Self::Pair => "hosts.title",
            Self::Threads | Self::ArrangeThreads => "mobile.threads",
            Self::Thread => "mobile.thread",
            Self::Panel => "chat.panels",
            Self::PullRequest => "pull_requests.title",
            Self::Settings | Self::SettingsSection | Self::SettingsThreadRules => "settings.title",
        })
        .into_owned()
        .into()
    }
}

/// Window-global UI state owned by the GPUI layer.
pub struct WindowState {
    /// The layout the window calls for. Derived from the build and the
    /// viewport by [`crate::window_seam::window_is_compact`] and never
    /// persisted: widening a window is not a preference.
    pub compact: bool,
    /// Where the window has been, oldest first. Never empty: `history[0]` is
    /// [`Destination::Hosts`], the root the platform's Back gesture falls off.
    history: Vec<Destination>,
    pub palette_open: bool,
    /// Whether the palette focuses its search input when it opens. A tap on
    /// the search pill wants the keyboard; the mobile "+" button lists
    /// projects to tap and must not raise it.
    pub palette_focuses_query: bool,
    pub sidebar_collapsed: bool,
    pub quit_prompt_epoch: u64,
    pub quit_prompt_open: bool,
    pub pending_settings_section: Option<String>,
}

impl WindowState {
    pub fn new(sidebar_collapsed: bool) -> Self {
        Self {
            compact: false,
            history: vec![Destination::Hosts],
            palette_open: false,
            palette_focuses_query: true,
            sidebar_collapsed,
            quit_prompt_epoch: 0,
            quit_prompt_open: false,
            pending_settings_section: None,
        }
    }

    pub fn with_compact(mut self, compact: bool) -> Self {
        self.compact = compact;
        self
    }

    /// Set the layout for the width the window now has. Returns whether the
    /// rule flipped, so the shell only reconciles navigation when it did.
    pub fn set_compact(&mut self, compact: bool, cx: &mut Context<Self>) -> bool {
        if self.compact == compact {
            return false;
        }
        self.compact = compact;
        cx.notify();
        true
    }

    pub fn history(&self) -> &[Destination] {
        &self.history
    }

    pub(crate) fn restore(&mut self, snapshot: &NavigationSnapshot, cx: &mut Context<Self>) {
        self.history.clone_from(&snapshot.history);
        cx.notify();
    }

    /// A restored thread was archived/deleted while this client was away.
    pub(crate) fn discard_restored_thread(&mut self, cx: &mut Context<Self>) {
        if let Some(index) = self.history.iter().position(|destination| {
            matches!(
                destination,
                Destination::Thread | Destination::Panel | Destination::PullRequest
            )
        }) {
            self.history.truncate(index);
            cx.notify();
        }
    }

    pub fn destination(&self) -> Destination {
        *self.history.last().expect("the history is never empty")
    }

    /// The destination Back returns to, or `None` at the root.
    pub fn parent(&self) -> Option<Destination> {
        (self.history.len() > 1).then(|| self.history[self.history.len() - 2])
    }

    pub fn route(&self) -> Route {
        self.destination().route()
    }

    /// Push `destination`. Visiting somewhere the window already is does
    /// nothing, so a repeated tap cannot stack the same page twice.
    pub fn go(&mut self, destination: Destination, cx: &mut Context<Self>) {
        if self.destination() == destination {
            return;
        }
        self.history.push(destination);
        cx.notify();
    }

    /// Leave the pairing page for the Machines page it was opened over. Wide,
    /// [`Self::back`] leaves the whole Hosts route, Pair included.
    pub fn close_pair(&mut self, cx: &mut Context<Self>) {
        if self.destination() != Destination::Pair {
            return;
        }
        self.history.pop();
        if self.destination() != Destination::Hosts {
            self.history.push(Destination::Hosts);
        }
        cx.notify();
    }

    /// Pop one step. `false` only where there is nothing left to pop — in
    /// compact that is the root, where the platform closes the app; in wide it
    /// is the workspace, which is not a page you can leave.
    pub fn back(&mut self, cx: &mut Context<Self>) -> bool {
        if self.history.len() < 2 {
            return false;
        }
        if self.compact {
            self.history.pop();
            cx.notify();
            return true;
        }
        // Wide shows the whole workspace hierarchy at once, so only a change of
        // route is a step the user can see.
        let route = self.route();
        if route == Route::Chat {
            return false;
        }
        while self.history.len() > 1 && self.route() == route {
            self.history.pop();
        }
        cx.notify();
        true
    }

    /// A fresh attachment: the previous host's pages are not this host's, so the
    /// history restarts at its thread list.
    pub fn enter_workspace(&mut self, cx: &mut Context<Self>) {
        self.history.truncate(1);
        self.history.push(Destination::Threads);
        cx.notify();
    }

    /// No attachment: only the root remains.
    pub fn leave_workspace(&mut self, cx: &mut Context<Self>) {
        self.history.truncate(1);
        cx.notify();
    }

    /// "Show me this thread." It is a navigation intent, not a layout decision:
    /// every width emits it and the shell decides how to present it — a wide
    /// window already shows the thread beside the list, a compact one pushes it.
    pub fn open_thread(&mut self, cx: &mut Context<Self>) {
        cx.emit(OpenThread);
    }

    /// Sidebar-driven conversation navigation replaces the wide Hosts visit
    /// with Chat. Compact keeps the visit on its stack so Back still returns
    /// through every page exactly as it did before.
    pub fn leave_route_for_chat(&mut self, cx: &mut Context<Self>) {
        if self.compact || self.route() != Route::Hosts {
            return;
        }
        while self.history.len() > 1 && self.route() == Route::Hosts {
            self.history.pop();
        }
        if self.route() != Route::Chat {
            return;
        }
        *self.history.last_mut().expect("the history is never empty") = Destination::Thread;
        cx.notify();
    }

    pub fn toggle_sidebar_collapsed(
        &mut self,
        store: &Entity<WorkspaceStore>,
        cx: &mut Context<Self>,
    ) {
        self.sidebar_collapsed = !self.sidebar_collapsed;
        store.update(cx, |store, cx| {
            store.set_sidebar_collapsed(self.sidebar_collapsed, cx)
        });
        cx.notify();
    }

    /// Switch to the settings route (closes the palette).
    pub fn open_settings(&mut self, cx: &mut Context<Self>) {
        self.palette_open = false;
        self.go(Destination::Settings, cx);
    }

    /// Leave settings, however deep in it the window is.
    pub fn close_settings(&mut self, cx: &mut Context<Self>) {
        while self.history.len() > 1 && self.route() == Route::Settings {
            self.history.pop();
        }
        cx.notify();
    }

    pub fn open_palette(&mut self, cx: &mut Context<Self>) {
        self.palette_open = true;
        self.palette_focuses_query = true;
        cx.notify();
    }

    /// Open the palette as a list to tap through, leaving the search input
    /// unfocused so no software keyboard comes up.
    pub fn open_palette_without_keyboard(&mut self, cx: &mut Context<Self>) {
        self.palette_open = true;
        self.palette_focuses_query = false;
        cx.notify();
    }

    pub fn close_palette(&mut self, cx: &mut Context<Self>) {
        self.palette_open = false;
        cx.notify();
    }

    pub fn toggle_palette(&mut self, cx: &mut Context<Self>) {
        self.palette_open = !self.palette_open;
        cx.notify();
    }
}

/// The top-level window surface: the chat workspace, the hosts list or
/// settings. Derived from [`WindowState::destination`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Route {
    #[default]
    Chat,
    Hosts,
    Settings,
}

#[derive(Clone, Copy)]
pub struct OpenThread;
impl EventEmitter<OpenThread> for WindowState {}

#[derive(Clone, Copy)]
pub(crate) struct OpenPanel(pub NavigationPanel);
impl EventEmitter<OpenPanel> for WindowState {}
