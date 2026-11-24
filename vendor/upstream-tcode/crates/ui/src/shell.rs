//! One responsive shell, on every client.
//!
//! [`AppShell`] is the window: it owns navigation, the layout rule and the
//! back stack, and it survives everything below it. Beneath it sits at most one
//! *attachment* — a link to a host and the views over that host's workspace.
//! Switching hosts replaces the attachment; it does not replace the window's
//! navigation root, and resizing the window never touches the attachment at all.
//!
//! Layout is decided by one rule (`crate::window_seam`): a desktop build is
//! always the wide split, and a mobile build is a navigation stack — hosts,
//! threads, thread, panel — under 900px of usable width and the wide split at
//! or above it. Wide and unattached, the shell is the hosts page and nothing
//! else until it attaches somewhere.

use std::cell::Cell;
use std::collections::HashMap;
use std::rc::Rc;
use std::time::Duration;
#[cfg(not(target_family = "wasm"))]
use std::time::Instant;
#[cfg(target_family = "wasm")]
use web_time::Instant;

use crate::overlay::{Notification, OverlayExt as _};
use crate::theme::ActiveTheme as _;
use crate::typography::TextRole as _;
use crate::widgets::tabs::{TabBar, TabItem};
use crate::window_state::{NavigationPanel, NavigationSnapshot};
use gpui::{
    AnyElement, AnyView, AnyWindowHandle, App, AppContext as _, ClipboardItem, Context, Div,
    ElementId, Entity, Focusable as _, Global, HitboxBehavior, InteractiveElement as _,
    IntoElement, MouseButton, MouseDownEvent, ParentElement as _, Pixels, Render, Role,
    SharedString, StatefulInteractiveElement as _, Styled as _, Subscription, Task, TouchPhase,
    WeakEntity, Window, actions, canvas, div, prelude::FluentBuilder as _, px,
};
use gpui_base::{
    NavMotion, NavOperation, NavStack, NavStackState, ResizableState, h_flex, h_resizable,
    motion::PresencePhase, motion::Transition, resizable_panel, v_flex,
};
use tcode_client::host::ClientHost;
use tcode_core::ui::RightTab;
use tcode_protocol::{
    RuntimeEffect, RuntimeError, RuntimeNotification as RuntimeEvent, RuntimeOperationId,
    RuntimeToast,
};

use crate::attachment::{Attachment, LocalTransport, same_target};
use crate::chat::ChatView;
use crate::diff::DiffPanel;
use crate::icon::{Icon, IconName};
use crate::palette::CommandPalette;
use crate::preview_panel::PreviewPanel;
#[cfg(preview_backend)]
use crate::preview_panel::lifecycle::BrowserLifecycle;
use crate::remote::{AttachmentTarget, RemotePanel};
use crate::runtime_event::{
    RuntimeEventSeverity, RuntimeToastAction, RuntimeToastDisposition, apply_runtime_effect,
    present_runtime_event, present_runtime_toast,
};
use crate::settings_page::SettingsPage;
use crate::sidebar::SessionsSidebar;
use crate::store::{StoreChange, TopicKind, WorkspaceStore};
use crate::toast::{RuntimeToastNotification, ToastAction, ToastId, ToastKind};
use crate::window_caption;
use crate::window_state::{Destination, OpenPanel, OpenThread, Route, WindowState};

actions!(tcode, [Quit, TogglePalette]);

/// Transient per-frame state backing [`window_drag_area`].
struct WindowDragState {
    should_move: bool,
}

impl Render for WindowDragState {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
    }
}

/// Whether this client's window can be dragged by its content at all. A phone
/// or browser surface has no movable window, so the handlers below are not just
/// useless there — they would arm on presses that belong to the content.
pub(crate) const WINDOW_DRAGGABLE: bool = cfg!(not(any(
    target_os = "ios",
    target_os = "android",
    target_family = "wasm"
)));

/// Make `el` a window-drag handle (the window has no separate titlebar, so the
/// column top rows are the drag surface). Mirrors gpui-component's `TitleBar`
/// mechanics: a press arms a move, the first drag calls `start_window_move`.
/// Child buttons that stop propagation on mouse-down won't arm a drag.
pub(crate) fn window_drag_area<E: gpui::InteractiveElement>(
    id: impl Into<ElementId>,
    el: E,
    window: &mut Window,
    cx: &mut App,
) -> E {
    if !WINDOW_DRAGGABLE {
        return el;
    }
    let state = window.use_keyed_state(id, cx, |_, _| WindowDragState { should_move: false });
    el.on_mouse_down_out(window.listener_for(&state, |state, _, _, _| {
        state.should_move = false;
    }))
    .on_mouse_down(
        MouseButton::Left,
        window.listener_for(&state, |state, event: &MouseDownEvent, window, _| {
            // A press that moves the window leaves the focus where it was.
            window.prevent_default();
            // Double-click zooms/maximizes the window like a native titlebar.
            if event.click_count >= 2 {
                state.should_move = false;
                window.titlebar_double_click();
            } else {
                state.should_move = true;
            }
        }),
    )
    .on_mouse_up(
        MouseButton::Left,
        window.listener_for(&state, |state, _, _, _| {
            state.should_move = false;
        }),
    )
    .on_mouse_move(window.listener_for(&state, |state, _, window, _| {
        if state.should_move {
            state.should_move = false;
            window.start_window_move();
        }
    }))
}

/// One entry of the compact [`NavStack`], standing for one entry of the
/// window's navigation history. It owns nothing: it renders whichever shell
/// destination it stands for, so every page stays mounted while a push or pop
/// animates. One view per *entry*, not per destination — the same destination
/// can appear twice (Hosts visited from an open thread).
struct DestinationView {
    shell: WeakEntity<AppShell>,
    destination: Destination,
}

impl Render for DestinationView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let destination = self.destination;
        self.shell
            .upgrade()
            .map(|shell| {
                shell.update(cx, |shell, cx| match destination {
                    Destination::Hosts => shell.render_hosts_page(window, cx),
                    Destination::Pair => shell.render_pair_page(window, cx),
                    Destination::Threads => shell.render_threads_page(window, cx),
                    Destination::Thread => shell.render_thread_page(window, cx),
                    Destination::Panel => shell.render_panel_page(window, cx),
                    Destination::PullRequest => shell.render_pull_request_page(window, cx),
                    Destination::Settings => {
                        shell.render_settings_page(Destination::Settings, window, cx)
                    }
                    Destination::SettingsSection | Destination::SettingsThreadRules => {
                        shell.render_settings_page(destination, window, cx)
                    }
                    Destination::ArrangeThreads => shell.render_arrange_page(window, cx),
                })
            })
            .unwrap_or_else(|| div().into_any_element())
    }
}

/// The views and transport for one host. Everything here is replaced when the
/// window attaches somewhere else, and nothing here is touched by a resize.
struct ShellAttachment {
    link: Attachment,
    sidebar: Entity<SessionsSidebar>,
    chat: Entity<ChatView>,
    diff: Entity<DiffPanel>,
    preview: Entity<PreviewPanel>,
    settings_page: Entity<SettingsPage>,
    palette: Entity<CommandPalette>,
    /// The workspace split's state, owned here rather than left to the group's
    /// internal state so the right panel can be given a real width when it
    /// opens. A fresh panel's size starts at the group's 100px minimum and the
    /// group latches the *first* width it measures — which, for a panel that
    /// appears mid-session, is that minimum. Left alone, the diff panel opens
    /// pinned to its 320px floor.
    split: Entity<ResizableState>,
    collapsed_split: Entity<ResizableState>,
    /// The width the right panel opens at: the default until the user drags the
    /// panel's own handle, then whatever they chose (for this run). A sidebar
    /// drag never changes it, even when it squeezes the panel.
    right_width: Rc<Cell<Pixels>>,
    floating_split: Entity<ResizableState>,
    floating_focus: Entity<gpui_base::PopoverState>,
    floating_restore_pending: bool,
    /// Keep giving the docked right panel its width until the group reports
    /// it, then stop so the restore never fights a drag. Armed when the panel
    /// docks and on every window width change: the group rescales its panels
    /// by ratio and so drifts the panel off the width the user chose.
    right_restore_pending: bool,
    sidebar_width: Rc<Cell<Pixels>>,
    /// Collapsed-only overlay visibility. Purely transient and never persisted;
    /// expanded/non-workspace renders clear it synchronously.
    sidebar_overlay_visible: bool,
    /// Last store selection seen by the shell, so selections made outside the
    /// sidebar's `OpenThread` event (notably an async draft) still switch the
    /// wide content column back from Hosts.
    observed_session_id: Option<String>,
    /// Whether this attachment's host settings have been adopted once.
    adopted: bool,
    /// Whether this attachment's saved record has been stamped with the time
    /// this window reached it.
    stamped: bool,
    _tasks: Vec<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

/// What bootstrap tells the shell that is not the window itself.
#[derive(Default)]
pub struct ShellSetup {
    /// This client's identity, saved hosts and preferences.
    pub client_host: Option<Rc<dyn ClientHost>>,
    /// How to reach a host running inside this process. A client that can never
    /// host has none.
    pub local: Option<LocalTransport>,
    /// Why that host did not start at launch. Machines says so from the first
    /// frame, whatever the window attaches to, and the window attaches to that
    /// host only when asked again.
    pub local_failure: Option<String>,
    /// Where to attach at launch; `None` opens on the hosts list.
    pub initial: Option<AttachmentTarget>,
    /// A browser link that could not be paired opens the normal form with the
    /// one-shot failure visible after its URL fragment has been cleared.
    pub initial_pairing_error: Option<String>,
    /// Wait for the first snapshots while attaching. Only desktop bootstrap
    /// asks for it, and only because it applies locale and theme from them
    /// before the first frame; a single-threaded client cannot wait at all.
    pub seed_blocking: bool,
    /// Mobile bootstrap restores client-local navigation. Desktop defaults to
    /// a fresh path; the phone example can opt into the mobile policy.
    pub restore_navigation: bool,
}

struct NavigationPersistence {
    scheduled: Option<NavigationSnapshot>,
    task: Option<Task<()>>,
}

struct PendingNavigationRestore {
    session_id: String,
    panel: NavigationPanel,
    panel_open: bool,
}

pub struct AppShell {
    window_state: Entity<WindowState>,
    setup: ShellSetup,
    attachment: Option<ShellAttachment>,
    /// Saved hosts, pairing by invitation and authentication repair: the hosts
    /// destination, and the same panel Settings → Remote shows.
    hosts: Entity<RemotePanel>,
    nav: Entity<NavStackState>,
    back_swipe: Option<BackSwipe>,
    // Settlement outlives the pan contact, including GPUI's fling cancellation.
    page_back: Option<PageBack>,
    /// The navigation history as the compact stack currently has it mounted.
    /// [`AppShell::sync_nav`] reconciles it with `window_state`'s history, which
    /// is the only authority for where the window is.
    mounted: Vec<Destination>,
    /// Which surface the compact panel destination is showing.
    panel_shows_terminal: bool,
    navigation: Option<NavigationPersistence>,
    pending_navigation_restore: Option<PendingNavigationRestore>,
    operation_toasts: HashMap<RuntimeOperationId, ToastId>,
    next_toast_id: ToastId,
    /// Tracks the palette's open state across frames so it can be focused on the
    /// open transition.
    palette_was_open: bool,
    /// Viewport width and sidebar collapse last seen by render; a change in
    /// either re-lays the split out, which arms the right-panel restores.
    last_split_layout: Option<(Pixels, bool)>,
    /// The title last given to the OS window. The title is in the current
    /// language, and a language switch reaches the shell only as a redraw.
    window_title: String,
    _subscriptions: Vec<Subscription>,
}

struct BackSwipe {
    origin: gpui::Point<Pixels>,
    progress: crate::edge_back::BackProgress,
    sheet: Option<Entity<crate::overlay::SheetBack>>,
    last_moved_at: Instant,
    finished: bool,
}

struct PageBack {
    previous: AnyView,
    current: AnyView,
    progress: f32,
    target: Option<f32>,
}

/// The right panel's default width.
const RIGHT_PANEL_WIDTH: f32 = 560.;
/// The panel's narrowest: the icon tabs and the panel's own controls fit in
/// 320pt, beside the window's caption buttons where the panel's strip holds them.
const RIGHT_PANEL_MIN_WIDTH: f32 = 320. + window_caption::RIGHT_PANEL_CAPTION_WIDTH;
/// The chat column never gets narrower than this beside the right panel, plus
/// whatever its header holds beyond the plain one (`header_extra_width`). At
/// 16pt padding the compact header fits: a title kept to a few words beside a
/// "+10" pull request badge, the git and Open split buttons as icon and
/// chevron, and five panel toggles measure 531pt. Where the chat's minimum and
/// the panel's do not both fit beside the sidebar, the panel floats over the
/// chat instead.
const CHAT_MIN_WIDTH: f32 = crate::chat::HEADER_COMPACT_MIN_WIDTH;
const SIDEBAR_WIDTH: f32 = 255.;
/// Collapsed only: width of the window's left-edge activation region.
const SIDEBAR_HOVER_EDGE: f32 = 12.;
/// Compact push/pop duration.
const NAV_TRANSITION: Duration = crate::edge_back::SETTLE;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SidebarHoverTransition {
    Trigger(bool),
    Overlay(bool),
}

/// Apply the asymmetric sibling-hover contract. The fixed trigger can open the
/// overlay but cannot close it; once mounted, the overlay owns closing itself.
fn next_sidebar_overlay_visibility(
    currently_visible: bool,
    transition: SidebarHoverTransition,
    collapsed: bool,
    route: Route,
    popover_open: bool,
) -> bool {
    if !collapsed || route != Route::Chat {
        return false;
    }

    match transition {
        SidebarHoverTransition::Trigger(true) | SidebarHoverTransition::Overlay(true) => true,
        SidebarHoverTransition::Trigger(false) => currently_visible,
        // A menu spawned from the sidebar (new-thread dropdown, row context
        // menu) is an occluding deferred layer: hovering it reads as leaving
        // the overlay. Treat an open popover's lifetime as continued hover;
        // the render pass reaps the overlay once the popover dismisses.
        SidebarHoverTransition::Overlay(false) => currently_visible && popover_open,
    }
}

impl AppShell {
    pub fn new(
        window_state: Entity<WindowState>,
        mut setup: ShellSetup,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut restored = setup
            .restore_navigation
            .then(|| {
                setup
                    .client_host
                    .as_ref()?
                    .load_preferences()
                    .navigation
                    .and_then(NavigationSnapshot::from_preferences)
            })
            .flatten();
        if let Some(snapshot) = &mut restored {
            setup.initial = setup.client_host.as_ref().and_then(|host| {
                host.load_hosts()
                    .into_iter()
                    .find(|saved| Some(&saved.host_id) == snapshot.host_id.as_ref())
                    .map(|host| AttachmentTarget::Remote(Box::new(host)))
            });
            if setup.initial.is_none() {
                snapshot.without_host();
            }
        }
        if let Some(host) = setup.client_host.as_ref().and_then(|client| {
            client.load_hosts().into_iter().find(|host| {
                client
                    .outbox_storage(&host.host_id)
                    .and_then(|storage| storage.load().ok())
                    .is_some_and(|entries| !entries.is_empty())
            })
        }) {
            setup.initial = Some(AttachmentTarget::Remote(Box::new(host)));
            restored = None;
        }
        // Seed the layout from the window before any child view exists, so the
        // first frame is already the right one rather than a wide split that
        // reflows on the second.
        let compact = crate::window_seam::window_is_compact(window, cx);
        window_state.update(cx, |state, cx| {
            state.set_compact(compact, cx);
        });
        let subscriptions = vec![
            // The one place navigation reaches the stack: whoever moved the
            // history — a nav bar, the sidebar, the palette, Android Back —
            // gets the same push or pop out of it.
            cx.observe_in(&window_state, window, |this, _, window, cx| {
                if this.mounted != this.window_state.read(cx).history() {
                    this.blur_navigation(window, cx);
                }
                this.sync_nav(NavMotion::Animated, cx);
                this.schedule_navigation_save(cx);
                cx.notify();
            }),
            cx.subscribe_in(&window_state, window, |this, _, _: &OpenThread, w, cx| {
                this.open_thread(w, cx);
            }),
            cx.subscribe(&window_state, |this, _, event: &OpenPanel, cx| {
                this.show_panel(event.0.id(), cx);
                if this.window_state.read(cx).compact {
                    this.go(Destination::Panel, cx);
                }
            }),
            // The one width observer. It updates the layout only when the rule
            // flips; `render` keeps its own width tracking for the split.
            cx.observe_window_bounds(window, |this, window, cx| {
                this.sync_layout(window, cx);
            }),
        ];
        let hosts = cx.new(|cx| {
            let mut panel = RemotePanel::new(None, window_state.clone(), window, cx);
            panel.set_pairing_error(setup.initial_pairing_error.clone());
            panel
        });
        let mut shell = Self {
            window_state,
            attachment: None,
            hosts,
            nav: cx.new(|_| NavStackState::new()),
            back_swipe: None,
            page_back: None,
            mounted: Vec::new(),
            panel_shows_terminal: false,
            navigation: setup.restore_navigation.then_some(NavigationPersistence {
                scheduled: None,
                task: None,
            }),
            pending_navigation_restore: None,
            operation_toasts: HashMap::new(),
            next_toast_id: 1,
            palette_was_open: false,
            last_split_layout: None,
            window_title: String::new(),
            _subscriptions: subscriptions,
            setup,
        };
        if let Some(reason) = shell.setup.local_failure.take() {
            shell
                .hosts
                .update(cx, |hosts, cx| hosts.set_local_failure(Some(reason), cx));
            if shell.setup.initial == Some(AttachmentTarget::Local) {
                shell.setup.initial = None;
            }
        }
        if let Some(target) = shell.setup.initial.take() {
            shell.attach(target, window, cx);
        }
        if let Some(snapshot) = restored {
            shell
                .window_state
                .update(cx, |state, cx| state.restore(&snapshot, cx));
            shell.panel_shows_terminal = snapshot.panel == NavigationPanel::Terminal;
            if let Some(session_id) = snapshot.session_id
                && let Some(store) = shell.store()
            {
                // Select once, before the baseline can trigger an empty-workspace
                // draft fallback. Validation below never selects it again.
                store.update(cx, |store, _| store.select_session(session_id.clone()));
                if let Some(attachment) = &mut shell.attachment {
                    attachment.observed_session_id = Some(session_id.clone());
                }
                shell.pending_navigation_restore = Some(PendingNavigationRestore {
                    session_id,
                    panel: snapshot.panel,
                    panel_open: snapshot.history.contains(&Destination::Panel),
                });
            }
        }
        shell.schedule_navigation_save(cx);
        shell
    }

    pub fn window_state(&self) -> Entity<WindowState> {
        self.window_state.clone()
    }

    /// The workspace this window is attached to, if any.
    pub fn store(&self) -> Option<Entity<WorkspaceStore>> {
        self.attachment
            .as_ref()
            .map(|attachment| attachment.link.store.clone())
    }

    pub fn link(&self) -> Option<tcode_client::HostLink> {
        self.attachment
            .as_ref()
            .map(|attachment| attachment.link.link())
    }

    #[cfg(preview_backend)]
    #[allow(private_interfaces)]
    #[doc(hidden)]
    pub fn preview_lifecycle(&self, cx: &App) -> Option<Entity<BrowserLifecycle>> {
        Some(self.attachment.as_ref()?.preview.read(cx).lifecycle())
    }
}

// ---------------------------------------------------------------------------
// Attachment lifecycle
// ---------------------------------------------------------------------------

impl AppShell {
    fn navigation_snapshot(&self, cx: &App) -> NavigationSnapshot {
        let store = self.store();
        let host_id = store
            .as_ref()
            .and_then(|store| store.read(cx).remote_host_id().map(str::to_owned));
        let session_id = store
            .as_ref()
            .and_then(|store| store.read(cx).active_session_id());
        let panel = self
            .pending_navigation_restore
            .as_ref()
            .map(|pending| pending.panel)
            .unwrap_or_else(|| {
                if self.panel_shows_terminal {
                    NavigationPanel::Terminal
                } else {
                    match store
                        .as_ref()
                        .map(|store| store.read(cx).panel_state().right_tab)
                    {
                        Some(RightTab::Plan) => NavigationPanel::Plan,
                        Some(RightTab::Agents) => NavigationPanel::Agents,
                        Some(RightTab::PullRequests) => NavigationPanel::PullRequests,
                        Some(RightTab::Preview) => NavigationPanel::Preview,
                        _ => NavigationPanel::Diff,
                    }
                }
            });
        NavigationSnapshot {
            history: self.window_state.read(cx).history().to_vec(),
            host_id,
            session_id,
            panel,
        }
    }

    fn schedule_navigation_save(&mut self, cx: &mut Context<Self>) {
        if self.navigation.is_none() {
            return;
        }
        let Some(host) = self.setup.client_host.clone() else {
            return;
        };
        let snapshot = self.navigation_snapshot(cx);
        let navigation = self.navigation.as_mut().expect("enabled above");
        if navigation.scheduled.as_ref() == Some(&snapshot) {
            return;
        }
        navigation.scheduled = Some(snapshot.clone());
        navigation.task = Some(cx.spawn(async move |_, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(150))
                .await;
            let mut preferences = host.load_preferences();
            let member_settings = preferences
                .navigation
                .as_ref()
                .and_then(|navigation| navigation.get("member_settings"))
                .cloned();
            preferences.navigation = serde_json::to_value(snapshot).ok();
            if let Some(member_settings) = member_settings {
                preferences.navigation.as_mut().unwrap()["member_settings"] = member_settings;
            }
            host.save_preferences(&preferences);
        }));
    }

    fn reconcile_navigation_restore(&mut self, cx: &mut Context<Self>) {
        let Some(pending) = &self.pending_navigation_restore else {
            return;
        };
        let Some(store) = self.store() else {
            self.pending_navigation_restore = None;
            return;
        };
        if !store.read(cx).index_hydrated() {
            if store.read(cx).active_session_id().as_deref() != Some(&pending.session_id) {
                self.pending_navigation_restore = None;
            }
            return;
        }
        if !store
            .read(cx)
            .sidebar_sessions()
            .iter()
            .any(|meta| meta.id == pending.session_id && meta.archived_at.is_none())
        {
            let pending = self
                .pending_navigation_restore
                .take()
                .expect("checked above");
            store.update(cx, |store, cx| {
                if store.active_session_id().as_deref() == Some(&pending.session_id) {
                    store.leave_session();
                    cx.notify();
                }
            });
            self.window_state
                .update(cx, |state, cx| state.discard_restored_thread(cx));
            return;
        }
        if store.read(cx).active_session_id().as_deref() != Some(&pending.session_id) {
            // A user selection made while syncing supersedes restoration.
            self.pending_navigation_restore = None;
            return;
        }
        if store.read(cx).chat_loading() {
            return;
        }
        let pending = self
            .pending_navigation_restore
            .take()
            .expect("checked above");
        if pending.panel_open {
            self.show_panel(pending.panel.id(), cx);
        } else {
            let tab = match pending.panel {
                NavigationPanel::Plan => RightTab::Plan,
                NavigationPanel::Agents => RightTab::Agents,
                NavigationPanel::PullRequests => RightTab::PullRequests,
                NavigationPanel::Preview => RightTab::Preview,
                _ => RightTab::Diff,
            };
            store.update(cx, |store, cx| store.set_right_tab(tab, cx));
        }
    }

    /// Point this window at `target`, replacing the attachment beneath the
    /// navigation root. The window, its stack and its overlays stay put.
    pub fn switch_to(
        &mut self,
        target: AttachmentTarget,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Already there: go to that host's workspace instead of tearing its
        // link down and building the same one again. A link the machine
        // refused is not reused: it stopped reconnecting, and re-pairing
        // lands here with the same machine.
        if self.attachment.as_ref().is_some_and(|current| {
            same_target(&current.link.target, &target)
                && !matches!(
                    current.link.store.read(cx).connection_state(),
                    tcode_client::ConnectionState::Offline { reason } if reason.is_terminal()
                )
        }) {
            self.go(Destination::Threads, cx);
            return;
        }
        // Never drop a working attachment for one this client cannot open.
        let reachable = match &target {
            AttachmentTarget::Local => self.setup.local.is_some(),
            AttachmentTarget::Remote(_) => self.setup.client_host.is_some(),
        };
        if !reachable {
            log::error!("this client cannot reach {target:?}");
            return;
        }
        self.blur_navigation(window, cx);
        self.attach(target, window, cx);
    }

    /// Leave the current host without opening another one. The host keeps
    /// running; only this window's link to it closes.
    pub fn detach(&mut self, cx: &mut Context<Self>) {
        let Some(attachment) = self.attachment.take() else {
            return;
        };
        attachment.link.close(cx).close();
        self.hosts.update(cx, |hosts, cx| hosts.set_store(None, cx));
        self.window_state
            .update(cx, |state, cx| state.leave_workspace(cx));
        cx.notify();
    }

    fn attach(&mut self, target: AttachmentTarget, window: &mut Window, cx: &mut Context<Self>) {
        // A local host that does not start is an attachment failure like any
        // other: Hosts shows why, and whatever this window was on stays.
        let local = match (&target, &self.setup.local) {
            (AttachmentTarget::Local, Some(start)) => match start(cx) {
                Ok(transport) => Some(transport),
                Err(reason) => {
                    log::error!("the local host is unavailable: {reason}");
                    self.hosts
                        .update(cx, |hosts, cx| hosts.set_local_failure(Some(reason), cx));
                    cx.notify();
                    return;
                }
            },
            _ => None,
        };
        if local.is_some() {
            self.hosts
                .update(cx, |hosts, cx| hosts.set_local_failure(None, cx));
        }
        self.pending_navigation_restore = None;
        if let Some(old) = self.attachment.take() {
            old.link.close(cx).close();
        }
        let Some(link) = Attachment::open(
            target,
            local,
            self.setup.client_host.clone(),
            self.setup.seed_blocking,
            cx,
        ) else {
            log::error!("this client cannot reach the requested host");
            self.window_state
                .update(cx, |state, cx| state.leave_workspace(cx));
            cx.notify();
            return;
        };
        if let Some(host) = &self.setup.client_host {
            host.set_last_host_id(match &link.target {
                AttachmentTarget::Local => None,
                AttachmentTarget::Remote(host) => Some(host.host_id.as_str()),
            });
        }

        let store = link.store.clone();
        let pending_session = store
            .read(cx)
            .pending_sessions()
            .first()
            .map(|(id, _)| id.clone());
        if let Some(id) = &pending_session {
            store.update(cx, |store, _| store.select_session(id.clone()));
        }
        let preview =
            cx.new(|cx| PreviewPanel::new(store.clone(), self.window_state.clone(), window, cx));
        let sidebar =
            cx.new(|cx| SessionsSidebar::new(store.clone(), self.window_state.clone(), cx));
        let subscriptions = vec![
            cx.observe_in(&store, window, move |this, store, window, cx| {
                this.stamp_connected(cx);
                let selected = store.read(cx).active_session_id();
                if let Some(attachment) = &mut this.attachment
                    && attachment.observed_session_id != selected
                {
                    attachment.observed_session_id = selected.clone();
                    this.blur_navigation(window, cx);
                    if selected.is_some() {
                        this.window_state
                            .update(cx, |state, cx| state.leave_route_for_chat(cx));
                    }
                }
                this.reconcile_navigation_restore(cx);
                this.schedule_navigation_save(cx);
                cx.notify();
            }),
            cx.subscribe_in(&store, window, |this, _, event: &RuntimeEvent, w, cx| {
                this.present_app_event(event, w, cx);
            }),
            cx.subscribe_in(
                &store,
                window,
                |this, _, change: &StoreChange, window, cx| {
                    if change.topic == TopicKind::Settings {
                        this.adopt_host_settings(window, cx);
                    }
                    cx.notify();
                },
            ),
        ];
        // Every client pumps preview requests: one without a backend still has
        // to answer `unsupported` for anything that reaches it.
        let requests = store.read(cx).remote_preview_requests();
        let preview_pump = {
            let preview = preview.clone();
            let store = store.downgrade();
            cx.spawn_in(window, async move |_, cx| {
                while let Ok(envelope) = requests.recv().await {
                    let tcode_protocol::ServerEvent::PreviewRequest {
                        request_id,
                        session_id,
                        request,
                    } = envelope.event
                    else {
                        continue;
                    };
                    let (reply, receiver) = async_channel::bounded(1);
                    if preview
                        .update_in(cx, |panel, window, cx| {
                            panel.handle_op(session_id, request, reply, window, cx)
                        })
                        .is_err()
                    {
                        break;
                    }
                    // Each operation waits independently: a slow wait_for must
                    // not block a second client's navigation or a screenshot.
                    let store = store.clone();
                    cx.spawn(async move |cx| {
                        let response = receiver
                            .recv()
                            .await
                            .unwrap_or_else(|_| Err("preview panel dropped request".into()));
                        let _ =
                            store.update(cx, |store, _| store.preview_reply(request_id, response));
                    })
                    .detach();
                }
            })
        };

        let observed_session_id = store.read(cx).active_session_id();
        self.attachment = Some(ShellAttachment {
            chat: cx.new(|cx| ChatView::new(store.clone(), self.window_state.clone(), window, cx)),
            diff: cx.new(|cx| DiffPanel::new(store.clone(), self.window_state.clone(), cx)),
            settings_page: cx
                .new(|cx| SettingsPage::new(store.clone(), self.window_state.clone(), window, cx)),
            palette: cx.new(|cx| {
                CommandPalette::new(store.clone(), self.window_state.clone(), window, cx)
            }),
            preview,
            sidebar,
            split: cx.new(|_| ResizableState::default()),
            collapsed_split: cx.new(|_| ResizableState::default()),
            right_width: Rc::new(Cell::new(px(RIGHT_PANEL_WIDTH))),
            floating_split: cx.new(|_| ResizableState::default()),
            floating_focus: cx.new(|cx| {
                let mut focus = gpui_base::PopoverState::new(false, cx);
                let store = link.store.downgrade();
                focus.set_on_open_change(Some(Rc::new(move |open, _, cx| {
                    if !open {
                        _ = store.update(cx, |store, cx| store.close_diff_panel(cx));
                    }
                })));
                focus
            }),
            floating_restore_pending: true,
            right_restore_pending: true,
            sidebar_width: Rc::new(Cell::new(px(SIDEBAR_WIDTH))),
            sidebar_overlay_visible: false,
            observed_session_id,
            adopted: false,
            stamped: false,
            link,
            _tasks: vec![preview_pump],
            _subscriptions: subscriptions,
        });
        self.hosts
            .update(cx, |hosts, cx| hosts.set_store(Some(store), cx));
        self.adopt_host_settings(window, cx);
        // The previous host's pages are not this host's: land on its threads.
        self.window_state
            .update(cx, |state, cx| state.enter_workspace(cx));
        if pending_session.is_some() {
            self.window_state
                .update(cx, |state, cx| state.open_thread(cx));
        }
        cx.notify();
    }

    /// Record on the saved record when this window actually reached the host,
    /// so its row in Hosts can say how recently it worked. Only a live
    /// connection counts; an attempt that never came up is not one.
    fn stamp_connected(&mut self, cx: &mut Context<Self>) {
        let Some(attachment) = &mut self.attachment else {
            return;
        };
        let AttachmentTarget::Remote(host) = &attachment.link.target else {
            return;
        };
        if attachment.stamped
            || !attachment
                .link
                .store
                .read(cx)
                .connection_state()
                .is_connected()
        {
            return;
        }
        attachment.stamped = true;
        if let Some(client) = cx.try_global::<crate::remote::ClientAttachment>() {
            // The timestamp mutation must share the transport's persistence
            // transaction so it cannot restore a pre-migration address.
            client
                .host()
                .stamp_connected(&host.host_id, crate::time::now_secs());
        }
    }

    /// Take the attached host's language, theme and sidebar state once its
    /// settings exist. Client preferences already override them inside the
    /// store, so this is the same value the settings page shows.
    fn adopt_host_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(attachment) = &mut self.attachment else {
            return;
        };
        let store = attachment.link.store.read(cx);
        if (attachment.adopted && store.scope().is_full()) || !store.settings_hydrated() {
            return;
        }
        attachment.adopted = true;
        let settings = store.settings();
        crate::settings::apply_locale(settings.language.as_deref());
        crate::settings_page::apply_theme(settings.theme_mode, window, cx);
        let collapsed = settings.sidebar_collapsed;
        self.window_state.update(cx, |state, cx| {
            state.sidebar_collapsed = collapsed;
            cx.notify();
        });
    }
}

// ---------------------------------------------------------------------------
// Navigation
// ---------------------------------------------------------------------------

impl AppShell {
    fn compact(&self, cx: &App) -> bool {
        self.window_state.read(cx).compact
    }

    pub(crate) fn wake_connection(&self, wake: tcode_client::recovery::Wake) {
        if let Some(attachment) = &self.attachment {
            attachment.link.link().wake(wake);
        }
    }

    /// Recompute the layout from the window. Only a flip does anything, and a
    /// flip reconciles navigation to where the state already is — it is not a
    /// Back gesture and must not animate like one.
    fn sync_layout(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let compact = crate::window_seam::window_is_compact(window, cx);
        let flipped = self
            .window_state
            .update(cx, |state, cx| state.set_compact(compact, cx));
        if flipped {
            self.move_edge_back(TouchPhase::Cancelled, window, cx);
            self.page_back = None;
            // The visit survives the resize; only a thread selected in the wide
            // split — which is not a page there — has to become one here.
            if compact
                && self.destination(cx) == Destination::Threads
                && self
                    .attachment
                    .as_ref()
                    .is_some_and(|a| a.link.store.read(cx).active_session_id().is_some())
            {
                self.window_state
                    .update(cx, |state, cx| state.go(Destination::Thread, cx));
            }
            self.sync_nav(NavMotion::Immediate, cx);
            cx.notify();
        }
    }

    fn destination(&self, cx: &App) -> Destination {
        self.window_state.read(cx).destination()
    }

    /// Mirror the window's navigation history into the compact stack: pop back
    /// to what the two still share, then push the rest. One view per history
    /// entry, so the same destination visited twice is two independent pages.
    fn sync_nav(&mut self, motion: NavMotion, cx: &mut Context<Self>) {
        let history = self.window_state.read(cx).history().to_vec();
        if self.mounted != history {
            self.page_back = None;
        }
        if self.mounted != history
            && let Some(swipe) = &mut self.back_swipe
        {
            swipe.finished = true;
            if let Some(sheet) = &swipe.sheet {
                sheet.update(cx, |sheet, cx| {
                    sheet.dragging = false;
                    sheet.progress = 0.;
                    App::notify(cx, sheet.parent);
                });
            }
        }
        let common = self
            .mounted
            .iter()
            .zip(&history)
            .take_while(|(mounted, target)| mounted == target)
            .count();
        let shell = cx.entity().downgrade();
        let mut pushed = Vec::new();
        self.nav.update(cx, |nav, cx| {
            while nav.depth() > common {
                if nav.pop(motion, cx).is_none() {
                    break;
                }
            }
            for destination in &history[common.min(history.len())..] {
                let page = AnyView::from(cx.new(|_| DestinationView {
                    shell: shell.clone(),
                    destination: *destination,
                }));
                nav.push(page, motion, cx);
                pushed.push(*destination);
            }
        });
        self.mounted.truncate(common);
        self.mounted.extend(pushed);
        let state = self.window_state.read(cx);
        let on_screen = if state.compact {
            state.destination() == Destination::Thread
        } else {
            state.route() == Route::Chat
        };
        if let Some(attachment) = &self.attachment {
            attachment.link.store.update(cx, |store, cx| {
                store.set_conversation_on_screen(on_screen, cx);
            });
        }
    }

    /// Navigating dismisses a software keyboard. Only a mobile build has one,
    /// and every compact window is a mobile build's.
    fn blur_navigation(&self, window: &mut Window, cx: &mut Context<Self>) {
        if crate::window_seam::is_mobile(cx) {
            window.blur(cx);
        }
    }

    fn go(&mut self, destination: Destination, cx: &mut Context<Self>) {
        self.window_state
            .update(cx, |state, cx| state.go(destination, cx));
    }

    /// "Show me this thread." Compact pushes; wide switches the content route.
    fn open_thread(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let selected = self
            .store()
            .and_then(|store| store.read(cx).active_session_id());
        if self
            .pending_navigation_restore
            .as_ref()
            .is_some_and(|pending| selected.as_deref() != Some(pending.session_id.as_str()))
        {
            self.pending_navigation_restore = None;
        }
        self.blur_navigation(window, cx);
        if self.compact(cx) {
            self.go(Destination::Thread, cx);
        } else {
            self.window_state
                .update(cx, |state, cx| state.leave_route_for_chat(cx));
        }
        if let Some(attachment) = &self.attachment {
            attachment
                .chat
                .update(cx, |chat, cx| chat.focus_composer(window, cx));
        }
        // Selecting another thread can leave the destination unchanged. Its
        // checkpoint must not depend on the host delivering a notification.
        self.schedule_navigation_save(cx);
    }

    /// Answer a platform Back gesture (or a Back control). `true` means it was
    /// consumed; `false` only at the root, where the platform closes the app.
    pub fn back(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        // A composition or a software keyboard is on top of everything.
        if crate::window_seam::keyboard_covers_window(window) {
            window.blur(cx);
            return true;
        }
        // Dialogs, menus and popovers own Cancel.
        if crate::overlay::dialog_open(window, cx)
            || gpui_base::GlobalState::is_in_deferred_context(cx)
        {
            window.dispatch_action(Box::new(gpui_base::actions::Cancel), cx);
            return true;
        }
        if self.window_state.read(cx).palette_open {
            self.window_state
                .update(cx, |state, cx| state.close_palette(cx));
            return true;
        }
        // An agent is reached from its lead, so Back returns there first.
        if self.window_state.read(cx).compact
            && self.destination(cx) == Destination::Thread
            && let Some((store, parent)) = self.agent_lead(cx)
        {
            store.update(cx, |store, _| store.select_session(parent));
            return true;
        }
        // Everything below is one history: settings detail, settings root, a
        // Hosts visit, Pair, and the workspace pages all pop the same way.
        // Leaving a page never touches the attachment.
        let leaving_thread =
            self.window_state.read(cx).compact && self.destination(cx) == Destination::Thread;
        let consumed = self.window_state.update(cx, |state, cx| state.back(cx));
        if consumed
            && leaving_thread
            && self.destination(cx) == Destination::Threads
            && let Some(attachment) = &self.attachment
        {
            attachment
                .preview
                .update(cx, |preview, cx| preview.release_preview(cx));
        }
        consumed
    }
}

/// The window whose shell answers the platform's Back gesture. Window-scoped on
/// purpose: a first-window lookup answers for whichever window happens to be
/// first, which is not the one the gesture arrived at.
struct ShellBackTarget {
    window: AnyWindowHandle,
    shell: WeakEntity<AppShell>,
}

impl Global for ShellBackTarget {}

pub(crate) fn set_back_target(window: AnyWindowHandle, shell: &Entity<AppShell>, cx: &mut App) {
    cx.set_global(ShellBackTarget {
        window,
        shell: shell.downgrade(),
    });
}

/// This window's shell, for the surfaces inside it that act on the window
/// rather than on themselves (the Hosts rows and their menu).
fn current_shell(cx: &App) -> Option<Entity<AppShell>> {
    cx.try_global::<ShellBackTarget>()?.shell.upgrade()
}

/// The window the platform's gestures arrive at, with its shell.
pub(crate) fn current_window_shell(cx: &App) -> Option<(AnyWindowHandle, Entity<AppShell>)> {
    let target = cx.try_global::<ShellBackTarget>()?;
    Some((target.window, target.shell.upgrade()?))
}

impl AppShell {
    /// The chat timeline while it is the page on screen with nothing layered
    /// over it: a scrolling screenshot of anything else would read back the
    /// palette, a dialog or another page.
    pub(crate) fn capture_chat(&self, cx: &App) -> Option<Entity<ChatView>> {
        let attachment = self.attachment.as_ref()?;
        let state = self.window_state.read(cx);
        let on_screen = if state.compact {
            state.destination() == Destination::Thread
        } else {
            state.route() == Route::Chat
        };
        (on_screen
            && !state.palette_open
            && self.pending_navigation_restore.is_none()
            && !gpui_base::GlobalState::is_in_deferred_context(cx))
        .then(|| attachment.chat.clone())
    }
}

/// App-level dispatch also reaches the shell when no child has keyboard focus.
pub(crate) fn navigate_thread(action: &crate::shortcut::NavigateThread, cx: &mut App) {
    let Some(shell) = current_shell(cx) else {
        return;
    };
    shell.update(cx, |shell, cx| {
        if let Some(attachment) = &shell.attachment
            && attachment
                .sidebar
                .update(cx, |sidebar, cx| sidebar.navigate_thread(action, cx))
        {
            shell.window_state.update(cx, |state, cx| {
                state.close_palette(cx);
                state.go(Destination::Thread, cx);
            });
        }
    });
}

pub(crate) fn undo_thread_action(action: &crate::sidebar::ThreadUndo, cx: &mut App) {
    let Some((window, shell)) = current_window_shell(cx) else {
        return;
    };
    let action = action.clone();
    cx.defer(move |cx| {
        let _ = window.update(cx, |_, window, cx| {
            shell.update(cx, |shell, cx| shell.on_thread_undo(&action, window, cx));
        });
    });
}

/// Point this window at another host.
pub(crate) fn switch_current(target: AttachmentTarget, window: &mut Window, cx: &mut App) {
    let Some(shell) = current_shell(cx) else {
        return;
    };
    // A child (notably the pairing confirmation) can call this from its
    // entity listener. Attachment setup updates those children in turn.
    window.defer(cx, move |window, cx| {
        shell.update(cx, |shell, cx| shell.switch_to(target, window, cx));
    });
}

/// Leave the attached host without opening another one.
pub(crate) fn detach_current(cx: &mut App) {
    let Some(shell) = current_shell(cx) else {
        return;
    };
    // The Hosts menu calls this from its panel's listener, and detaching
    // updates that panel.
    cx.defer(move |cx| shell.update(cx, |shell, cx| shell.detach(cx)));
}

/// Route a platform Back gesture into the shell that owns the window.
pub fn handle_back(cx: &mut App) -> bool {
    let Some(target) = cx.try_global::<ShellBackTarget>() else {
        return false;
    };
    let (window, shell) = (target.window, target.shell.clone());
    window
        .update(cx, |_, window, cx| {
            shell
                .update(cx, |shell, cx| shell.back(window, cx))
                .unwrap_or(false)
        })
        .unwrap_or(false)
}

/// Whether the top sheet or a compact page can answer the native edge gesture.
pub fn can_swipe_back(cx: &mut App) -> bool {
    let Some((window, shell)) = current_window_shell(cx) else {
        return false;
    };
    window
        .update(cx, |_, window, cx| {
            crate::overlay::active_sheet(window, cx).is_some() || {
                let shell = shell.read(cx);
                !crate::overlay::dialog_open(window, cx)
                    && shell.compact(cx)
                    && !shell.window_state.read(cx).palette_open
                    && !gpui_base::GlobalState::is_in_deferred_context(cx)
                    && shell.nav.read(cx).depth() > 1
            }
        })
        .unwrap_or(false)
}

/// UIKit supplies continuous translation and velocity; thresholds stay in Rust.
pub fn update_edge_back(phase: TouchPhase, travel: f32, velocity: f32, cx: &mut App) {
    let Some((window, shell)) = current_window_shell(cx) else {
        return;
    };
    let _ = window.update(cx, |_, window, cx| {
        shell.update(cx, |shell, cx| {
            if phase == TouchPhase::Started {
                shell.begin_edge_back(
                    gpui::point(px(0.), px(0.)),
                    window.viewport_size().width,
                    window,
                    cx,
                );
            }
            if let Some(swipe) = &mut shell.back_swipe {
                swipe.progress.travel = px(travel);
                swipe.progress.velocity = velocity;
            }
            shell.move_edge_back(phase, window, cx);
        });
    });
}

/// The per-thread notification slot for its pull request watch.
struct PullRequestWatchNotice;

// ---------------------------------------------------------------------------
// Toasts and runtime events
// ---------------------------------------------------------------------------

impl AppShell {
    fn present_app_event(
        &mut self,
        event: &RuntimeEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let toast = match event {
            RuntimeEvent::Effect(RuntimeEffect::ApplyLocale { .. }) => {
                let Some(attachment) = &self.attachment else {
                    return;
                };
                let language = attachment.link.store.read(cx).settings().language;
                apply_runtime_effect(&RuntimeEffect::ApplyLocale { language });
                cx.notify();
                return;
            }
            RuntimeEvent::Effect(RuntimeEffect::CopyToClipboard { text }) => {
                cx.write_to_clipboard(ClipboardItem::new_string(text.clone()));
                return;
            }
            RuntimeEvent::Error(RuntimeError::WorkingDirectoryMissing {
                project_id: Some(project_id),
                cwd,
                ..
            }) if self.store().is_some() => {
                let store = self.store().expect("checked above");
                crate::change_project_root_dialog::open_missing_directory(
                    store,
                    project_id.clone(),
                    cwd.clone(),
                    window,
                    cx,
                );
                return;
            }
            RuntimeEvent::Error(_) | RuntimeEvent::Notice(_) => {
                let presented = present_runtime_event(event);
                let notification = match presented.severity {
                    RuntimeEventSeverity::Error => Notification::error(presented.message),
                    RuntimeEventSeverity::Warning => Notification::warning(presented.message),
                    RuntimeEventSeverity::Success => Notification::success(presented.message),
                };
                window.push_notification(notification, cx);
                return;
            }
            RuntimeEvent::Toast(RuntimeToast::PullRequestWatch {
                session_id,
                number,
                notice,
            }) => {
                self.present_pull_request_watch(session_id, *number, notice, window, cx);
                return;
            }
            RuntimeEvent::Toast(RuntimeToast::PullRequestStack {
                session_id,
                target,
                stack,
                layers,
                base,
                result,
                late,
            }) => {
                if let Some(store) = self.store() {
                    crate::pull_requests::present_stack_result(
                        &store, session_id, target, *stack, layers, base, result, *late, window, cx,
                    );
                }
                return;
            }
            RuntimeEvent::Toast(toast) => toast,
        };

        let presented = present_runtime_toast(toast);
        let toast_id = match presented.disposition {
            RuntimeToastDisposition::Push => self.take_toast_id(),
            RuntimeToastDisposition::Start(operation) => {
                let toast_id = self.take_toast_id();
                self.operation_toasts.insert(operation, toast_id);
                toast_id
            }
            RuntimeToastDisposition::Update(operation) => {
                let Some(id) = self.operation_toasts.get(&operation) else {
                    return;
                };
                *id
            }
            RuntimeToastDisposition::Finish(operation) => self
                .operation_toasts
                .remove(&operation)
                .unwrap_or_else(|| self.take_toast_id()),
        };

        let action = presented.action.and_then(|action| {
            let store = self.store()?;
            let label = match &action {
                RuntimeToastAction::RetryGit(_) => crate::tr!("git.toast.retry"),
                RuntimeToastAction::UpdateProviders(_) => crate::tr!("providers.update_all"),
            }
            .into_owned()
            .into();
            Some(ToastAction {
                label,
                handler: Rc::new(move |window, cx| {
                    window.remove_notification1::<RuntimeToastNotification>(toast_id as usize, cx);
                    store.update(cx, |store, _cx| match action.clone() {
                        RuntimeToastAction::RetryGit(request) => store.retry_git_action(request),
                        RuntimeToastAction::UpdateProviders(providers) => {
                            store.update_providers(providers)
                        }
                    });
                }),
            })
        });
        self.show_toast(
            toast_id,
            presented.kind,
            (presented.title, presented.detail, presented.progress),
            action,
            window,
            cx,
        );
    }

    /// One slot per thread: a newer wake or end of its watch replaces the older notice.
    fn present_pull_request_watch(
        &mut self,
        session_id: &str,
        number: u64,
        notice: &tcode_core::pull_request_watch::WatchNotice,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(store) = self.store() else {
            return;
        };
        // The open thread's transcript already shows the message.
        if window.is_window_active()
            && store.read(cx).active_session_id().as_deref() == Some(session_id)
        {
            return;
        }
        let title = store
            .read(cx)
            .thread_meta(session_id)
            .map(|meta| meta.title.clone())
            .unwrap_or_default();
        let (kind, message) = crate::pull_requests::watch_notice_toast(number, notice);
        let id = session_id.to_owned();
        window.push_notification(
            Notification::new()
                .id1::<PullRequestWatchNotice>(SharedString::from(id.clone()))
                .title(title)
                .message(message)
                .with_type(kind)
                .action(move |_, _, _| {
                    use crate::sizing::Sizable as _;
                    let store = store.clone();
                    let id = id.clone();
                    crate::widgets::button::Button::new("pull-request-watch-open-thread")
                        .small()
                        .outline()
                        .label(crate::tr!("pull_requests.open_thread"))
                        .on_click(move |_, window, cx| {
                            window.remove_notification1::<PullRequestWatchNotice>(
                                SharedString::from(id.clone()),
                                cx,
                            );
                            store.update(cx, |store, _| store.select_session(id.clone()));
                        })
                })
                .autohide(true),
            cx,
        );
    }

    fn take_toast_id(&mut self) -> ToastId {
        let id = self.next_toast_id;
        self.next_toast_id += 1;
        id
    }

    fn show_toast(
        &mut self,
        id: ToastId,
        kind: ToastKind,
        content: (String, Option<String>, Option<f32>),
        action: Option<ToastAction>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (title, detail, progress) = content;
        let has_action = action.is_some();
        window.push_notification(
            crate::toast::notification(id, kind, title, detail.map(Into::into), action, progress),
            cx,
        );

        let delay = match kind {
            ToastKind::Success | ToastKind::Info => Some(Duration::from_secs(4)),
            ToastKind::Warning => Some(Duration::from_secs(6)),
            ToastKind::Error | ToastKind::Loading => None,
        };
        if let Some(delay) = delay.filter(|_| !has_action) {
            cx.spawn_in(window, async move |this, cx| {
                cx.background_executor().timer(delay).await;
                _ = this.update_in(cx, |_, window, cx| {
                    window.remove_notification1::<RuntimeToastNotification>(id as usize, cx);
                });
            })
            .detach();
        }
    }

    fn on_open_source_control(
        &mut self,
        _: &crate::pull_requests::OpenSourceControl,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        window.dispatch_action(Box::new(gpui_base::actions::Cancel), cx);
        self.window_state.update(cx, |state, cx| {
            state.pending_settings_section = Some("source_control".into());
            state.open_settings(cx);
        });
    }

    fn on_open_thread_anchor(
        &mut self,
        anchor: &crate::markdown::OpenThreadAnchor,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(store) = self.store() else {
            return;
        };
        if store.read(cx).thread_meta(&anchor.thread_id).is_none() {
            window.push_notification(
                crate::overlay::Notification::warning(
                    crate::tr!("control.anchor_missing").into_owned(),
                ),
                cx,
            );
            return;
        }
        let anchor = anchor.clone();
        store.update(cx, |store, cx| {
            match anchor.position {
                Some(position) => store.select_session_at_position(anchor.thread_id, position, cx),
                None => store.select_session(anchor.thread_id),
            }
            cx.notify();
        });
        self.window_state
            .update(cx, |state, cx| state.open_thread(cx));
    }

    fn on_thread_undo(
        &mut self,
        _: &crate::sidebar::ThreadUndo,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(attachment) = &self.attachment {
            attachment
                .sidebar
                .update(cx, |sidebar, cx| sidebar.undo_lifecycle(window, cx));
        }
    }

    fn on_toggle_palette(
        &mut self,
        _: &TogglePalette,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.window_state
            .update(cx, |state, cx| state.toggle_palette(cx));
    }
}

// ---------------------------------------------------------------------------
// Compact chrome
// ---------------------------------------------------------------------------

/// The least room a nav bar keeps on each side for its controls, so the
/// centered title keeps the same box on every page whose controls fit it: two
/// 44pt icon buttons. A wider back label ([`Destination::back_label`]), which
/// never truncates, takes more.
const NAV_CONTROL_WIDTH: f32 = 104.;

/// Navigation header with a centered title and at most two trailing actions.
fn nav_bar(
    back: Option<AnyElement>,
    title: SharedString,
    subtitle: Option<AnyElement>,
    actions: Vec<AnyElement>,
    window: &mut Window,
    cx: &mut App,
) -> Div {
    debug_assert!(
        actions.len() <= 2,
        "navigation bar allows at most two trailing actions"
    );
    // A compact window on the desktop is still a window: it keeps the platform's
    // own controls out of the content and stays draggable by its top strip.
    let traffic_lights = window_caption::traffic_light_inset(window);
    let hosts_caption = window_caption::compact_hosts_caption();
    let (dense, tight) = (cx.theme().tokens.spacing.sm, cx.theme().tokens.spacing.xs);
    // The sides carry the strip's insets themselves, so their floor is
    // measured from the window's edge (or the platform's controls). The 44pt
    // controls keep their glyphs clear of the edge inside their own hit box,
    // so the inset is the dense one, not the page inset.
    let strip = crate::material::bar("compact-nav", crate::material::Bar::Strip, false, cx)
        .px_0()
        .gap(Pixels::ZERO)
        .pl(traffic_lights);
    v_flex()
        .flex_none()
        .w_full()
        .bg(crate::material::content_surface(cx))
        .child(
            window_drag_area("compact-nav-drag", strip, window, cx)
                // The two sides grow alike from nothing, so the title is centered
                // while it fits; a side never shrinks below its controls, so a
                // label wider than the reserved room pushes the title, which
                // truncates, instead of running into it.
                .child(
                    h_flex()
                        .flex_1()
                        .flex_basis(px(0.))
                        .min_w(px(NAV_CONTROL_WIDTH))
                        .h_full()
                        .items_center()
                        .when(traffic_lights == Pixels::ZERO, |side| side.pl(dense))
                        .pr(tight)
                        .children(back)
                        .child(window_caption::drag_region(div().flex_1().h_full())),
                )
                .child(
                    v_flex()
                        .min_w_0()
                        .items_center()
                        .justify_center()
                        .child(
                            div()
                                .max_w_full()
                                .min_w_0()
                                .overflow_hidden()
                                .text_title(cx)
                                .child(crate::widgets::fade_text(title)),
                        )
                        .children(subtitle),
                )
                .child(
                    h_flex()
                        .flex_1()
                        .flex_basis(px(0.))
                        .min_w(px(NAV_CONTROL_WIDTH))
                        .h_full()
                        .items_center()
                        .justify_end()
                        .gap(tight)
                        .when(!hosts_caption, |side| side.pr(dense))
                        .child(window_caption::drag_region(div().flex_1().h_full()))
                        .children(actions)
                        .children(
                            hosts_caption.then(|| window_caption::caption_controls(window, cx)),
                        ),
                ),
        )
        .child(crate::material::faded_hairline(cx))
}

/// Back control labelled with the parent destination's short fixed label,
/// which is never truncated.
fn back_button(id: &'static str, parent: SharedString) -> crate::widgets::button::Button {
    use crate::widgets::button::{Button, ButtonVariants as _};
    Button::new(id)
        .ghost()
        .touch(true)
        .icon(Icon::empty().path("icons/chevron-left.svg"))
        .label(parent)
        // A control press must not arm the ancestor window drag region.
        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
}

/// A nav-bar icon button. Blue is reserved for primary actions and live state.
fn nav_icon_button(
    id: impl Into<ElementId>,
    aria_label: impl Into<SharedString>,
    icon: IconName,
) -> crate::widgets::button::Button {
    crate::material::toolbar_icon_button(id, icon, aria_label, true)
        // A control press must not arm the ancestor window drag region.
        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
}

/// The muted second line under a nav-bar title: how the machine a thread
/// list belongs to is reached, the project a thread lives in.
fn nav_subtitle(text: impl Into<SharedString>, cx: &App) -> AnyElement {
    div()
        .max_w_full()
        .min_w_0()
        .overflow_hidden()
        .text_secondary(true, cx)
        .child(crate::widgets::fade_text(text.into()))
        .into_any_element()
}

fn compact_label(key: &str) -> String {
    crate::tr!(format!("mobile.{key}")).into_owned()
}

impl AppShell {
    /// The lead thread of the open agent, when one is open.
    fn agent_lead(&self, cx: &App) -> Option<(Entity<WorkspaceStore>, String)> {
        let store = self.attachment.as_ref()?.link.store.clone();
        let parent = {
            let store = store.read(cx);
            let active = store.active_session_id()?;
            let sessions = store.sidebar_sessions();
            let parent = sessions
                .iter()
                .find(|meta| meta.id == active)?
                .parent_session_id
                .clone()?;
            sessions
                .iter()
                .any(|meta| meta.id == parent)
                .then_some(parent)?
        };
        Some((store, parent))
    }

    /// Back to whatever is under this page, labelled with that destination's
    /// short fixed label. `None` at the root, where the platform owns Back.
    fn back_control(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let label = if self.destination(cx) == Destination::Thread && self.agent_lead(cx).is_some()
        {
            crate::tr!("mobile.thread").into_owned().into()
        } else {
            self.window_state.read(cx).parent()?.back_label()
        };
        Some(
            back_button("compact-back", label)
                .debug_selector(|| "compact-back".into())
                .on_click(cx.listener(|this, _, window, cx| {
                    this.back(window, cx);
                }))
                .into_any_element(),
        )
    }

    fn palette_action(&self, cx: &mut Context<Self>) -> AnyElement {
        nav_icon_button(
            "compact-open-palette",
            crate::tr!("sidebar.search").into_owned(),
            IconName::Search,
        )
        .debug_selector(|| "compact-open-palette".into())
        .on_click(cx.listener(|this, _, window, cx| {
            this.on_toggle_palette(&TogglePalette, window, cx);
        }))
        .into_any_element()
    }

    fn render_hosts_page(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let back = self.back_control(cx);
        let hosts = self.hosts.clone();
        let body = hosts.update(cx, |panel, cx| panel.render_hosts(window, cx));
        v_flex()
            .size_full()
            .bg(crate::material::content_surface(cx))
            .debug_selector(|| "hosts-page".into())
            .child(nav_bar(
                back,
                crate::tr!("hosts.title").into_owned().into(),
                None,
                self.attachment
                    .as_ref()
                    .map(|_| self.palette_action(cx))
                    .into_iter()
                    .collect(),
                window,
                cx,
            ))
            .child(body)
            .into_any_element()
    }

    /// The pairing form as its own page, so Back returns to whatever pushed it
    /// — the Hosts list, or the Nearby row that prefilled the endpoint.
    fn render_pair_page(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let back = self.back_control(cx);
        let hosts = self.hosts.clone();
        let body = hosts.update(cx, |panel, cx| panel.render_pair(window, cx));
        v_flex()
            .size_full()
            .bg(crate::material::content_surface(cx))
            .child(nav_bar(
                back,
                crate::tr!("hosts.pair.page_title").into_owned().into(),
                None,
                vec![],
                window,
                cx,
            ))
            .child(body)
            .into_any_element()
    }

    /// Settings, as two pages of the same stack: the section list, then one
    /// section's detail. Both wear the same nav bar as every other page.
    fn render_settings_page(
        &mut self,
        destination: Destination,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let back = self.back_control(cx);
        let Some(settings) = self
            .attachment
            .as_ref()
            .map(|attachment| attachment.settings_page.clone())
        else {
            return div().into_any_element();
        };
        let detail = destination != Destination::Settings;
        let title = if destination == Destination::SettingsSection
            && self.destination(cx) == Destination::SettingsThreadRules
        {
            crate::tr!("settings.threads").into_owned().into()
        } else if detail {
            settings.read(cx).section_title(cx)
        } else {
            crate::tr!("settings.title").into_owned().into()
        };
        let body = settings.update(cx, |page, cx| {
            if detail {
                page.render_compact_section(destination, window, cx)
            } else {
                page.render_compact_list(cx)
            }
        });
        v_flex()
            .size_full()
            .bg(crate::material::content_surface(cx))
            .child(nav_bar(
                back,
                title,
                None,
                vec![self.palette_action(cx)],
                window,
                cx,
            ))
            .child(body)
            .into_any_element()
    }

    fn render_arrange_page(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let Some(sidebar) = self
            .attachment
            .as_ref()
            .map(|attachment| attachment.sidebar.clone())
        else {
            return div().into_any_element();
        };
        use crate::widgets::button::{Button, ButtonVariants as _};
        let done = div()
            // A control press must not arm the ancestor window drag region.
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(
                Button::new("arrange-done")
                    .ghost()
                    .label(crate::tr!("sidebar.arrange_done"))
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.back(window, cx);
                    })),
            )
            .into_any_element();
        let body = sidebar.update(cx, |sidebar, cx| sidebar.render_arrange(window, cx));
        v_flex()
            .debug_selector(|| "compact-arrange-page".into())
            .size_full()
            .bg(crate::material::content_surface(cx))
            .child(nav_bar(None, "".into(), None, vec![done], window, cx))
            .child(body)
            .into_any_element()
    }

    fn render_threads_page(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let back = self.back_control(cx);
        let Some(attachment) = &self.attachment else {
            return div().into_any_element();
        };
        let store = attachment.link.store.read(cx);
        // A remote machine's list is headed by the machine, with the link's
        // state where every other page puts its context. The local list is
        // just Threads; the way to the machines is the Back control.
        let (title, subtitle): (SharedString, Option<SharedString>) = match store.remote_host_name()
        {
            Some(_) => (
                store.machine_label().into(),
                Some(crate::remote::connection_label(&store.connection_state()).into()),
            ),
            None => (crate::tr!("mobile.threads").into_owned().into(), None),
        };
        let sidebar = attachment.sidebar.clone();
        let actions = vec![
            nav_icon_button(
                "compact-new-thread",
                compact_label("new_thread"),
                IconName::Plus,
            )
            .debug_selector(|| "compact-new-thread".into())
            .on_click(cx.listener(move |this, _, window, cx| {
                this.start_thread(window, cx);
            }))
            .into_any_element(),
            nav_icon_button(
                "compact-settings",
                compact_label("settings"),
                IconName::Settings,
            )
            .on_click(cx.listener(|this, _, _, cx| {
                this.window_state
                    .update(cx, |state, cx| state.open_settings(cx));
            }))
            .into_any_element(),
        ];
        v_flex()
            .debug_selector(|| "compact-threads-page".into())
            .size_full()
            .bg(crate::material::content_surface(cx))
            .child(nav_bar(
                back,
                title,
                subtitle.map(|subtitle| nav_subtitle(subtitle, cx)),
                actions,
                window,
                cx,
            ))
            .child(div().flex_1().min_h_0().child(sidebar))
            .into_any_element()
    }

    /// New thread: one project starts a draft directly, several go through the
    /// palette, which already owns "new thread in <project>" and can search.
    /// On a phone the palette opens without the keyboard: the projects are
    /// there to tap, and the search pill still raises it on demand.
    fn start_thread(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(attachment) = &self.attachment else {
            return;
        };
        let projects = attachment.link.store.read(cx).projects();
        let Some(project) = projects.first().cloned().filter(|_| projects.len() == 1) else {
            let mobile = crate::window_seam::is_mobile(cx);
            self.window_state.update(cx, |state, cx| {
                if mobile {
                    state.open_palette_without_keyboard(cx);
                } else {
                    state.open_palette(cx);
                }
            });
            return;
        };
        attachment.link.store.update(cx, |store, cx| {
            store.start_draft(project.id.clone(), project.root.clone(), cx)
        });
        self.open_thread(window, cx);
    }

    fn render_thread_page(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let back = self.back_control(cx);
        let Some(attachment) = &self.attachment else {
            return div().into_any_element();
        };
        let store = attachment.link.store.read(cx);
        let active = store.chat_active_session();
        let project = active
            .as_ref()
            .and_then(|(_, cwd, _)| {
                store
                    .projects()
                    .into_iter()
                    .find(|project| project.root == *cwd)
            })
            .map(|project| project.name);
        // Over a remote link the project shares its line with the link's state.
        let subtitle = if !store.scope().is_full() {
            Some(store.machine_label())
        } else {
            match (project, store.remote_host_name()) {
                (Some(project), Some(_)) => Some(format!(
                    "{project} · {}",
                    crate::remote::connection_label(&store.connection_state())
                )),
                (None, Some(_)) => Some(crate::remote::connection_label(&store.connection_state())),
                (project, None) => project,
            }
        };
        let title = active
            .map(|(title, _, draft)| {
                if draft {
                    compact_label("new_thread")
                } else {
                    title
                }
            })
            .unwrap_or_else(|| {
                compact_label(if self.pending_navigation_restore.is_some() {
                    "thread"
                } else {
                    "new_thread"
                })
            });
        let body = if self.pending_navigation_restore.is_some() {
            crate::material::loading_skeleton(cx)
        } else {
            attachment.chat.clone().into_any_element()
        };
        v_flex()
            .debug_selector(|| "compact-thread-page".into())
            .size_full()
            .bg(crate::material::content_surface(cx))
            .child(nav_bar(
                back,
                title.into(),
                subtitle.map(|subtitle| nav_subtitle(subtitle, cx)),
                vec![
                    self.palette_action(cx),
                    nav_icon_button(
                        "compact-panels",
                        crate::tr!("chat.panels").into_owned(),
                        IconName::PanelRight,
                    )
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.open_panels(cx);
                    }))
                    .into_any_element(),
                ],
                window,
                cx,
            ))
            .child(div().flex_1().min_h_0().child(body))
            .into_any_element()
    }

    /// The terminal, diff/plan and preview at full width. They are the same
    /// entities the wide layout puts in the split — a compact window has no
    /// room beside the timeline, not less of a product.
    fn render_panel_page(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let back = self.back_control(cx);
        let Some(attachment) = &self.attachment else {
            return div().into_any_element();
        };
        let store = attachment.link.store.read(cx);
        let panel = store.panel_state();
        let selected_panel = self
            .pending_navigation_restore
            .as_ref()
            .map(|pending| pending.panel.id());
        let terminal = selected_panel.map_or(self.panel_shows_terminal, |id| id == "terminal");
        let show_agents = store.active_session_id().is_some_and(|id| {
            !crate::agents_panel::Agents::of(store, &id).is_empty()
                || panel.right_tab == RightTab::Agents
                || selected_panel == Some("agents")
        });
        let show_preview = store.offers_preview();
        attachment.preview.update(cx, |preview, cx| {
            preview.set_compact_panel_selected(
                self.pending_navigation_restore.is_none()
                    && !terminal
                    && panel.right_tab == RightTab::Preview,
                cx,
            );
        });
        let shell = cx.entity();
        let segments = [
            (
                "terminal",
                crate::tr!("terminal.title").into_owned(),
                terminal,
            ),
            (
                "diff",
                crate::tr!("diff.title").into_owned(),
                selected_panel.map_or(!terminal && panel.right_tab == RightTab::Diff, |id| {
                    id == "diff"
                }),
            ),
            (
                "plan",
                crate::tr!("tasks.tab").into_owned(),
                selected_panel.map_or(!terminal && panel.right_tab == RightTab::Plan, |id| {
                    id == "plan"
                }),
            ),
            (
                "agents",
                crate::tr!("agents.title").into_owned(),
                selected_panel.map_or(!terminal && panel.right_tab == RightTab::Agents, |id| {
                    id == "agents"
                }),
            ),
            (
                "pr",
                crate::tr!("pull_requests.title").into_owned(),
                selected_panel.map_or(
                    !terminal && panel.right_tab == RightTab::PullRequests,
                    |id| id == "pr",
                ),
            ),
            (
                "preview",
                crate::tr!("preview.title").into_owned(),
                selected_panel.map_or(!terminal && panel.right_tab == RightTab::Preview, |id| {
                    id == "preview"
                }),
            ),
        ]
        .into_iter()
        .filter(|(id, _, _)| (*id != "preview" || show_preview) && (*id != "agents" || show_agents))
        .map(|(id, label, selected)| {
            let shell = shell.clone();
            TabItem::new(SharedString::from(format!("compact-panel-{id}")), label)
                .selected(selected)
                .on_click(move |_, _, cx| {
                    shell.update(cx, |shell, cx| shell.show_panel(id, cx));
                })
        })
        .collect::<Vec<_>>();
        let segments = TabBar::new("compact-panel-track")
            .segmented()
            .compact(true)
            .items(segments);

        let body: AnyElement = if self.pending_navigation_restore.is_some() {
            crate::material::loading_skeleton(cx)
        } else if terminal {
            attachment
                .chat
                .read(cx)
                .terminal_drawer()
                .into_any_element()
        } else if panel.right_tab == RightTab::Preview {
            attachment.preview.clone().into_any_element()
        } else {
            attachment.diff.clone().into_any_element()
        };
        // The drawer sizes its grid from the space it is actually given.
        if terminal {
            let size = window.viewport_size();
            let drawer = attachment.chat.read(cx).terminal_drawer();
            let (width, height) = (f32::from(size.width), f32::from(size.height) - 110.);
            if !drawer.read(cx).is_size(width, height) {
                drawer.update(cx, |drawer, cx| drawer.resize(width, height, cx));
            }
        }

        v_flex()
            .size_full()
            .bg(crate::material::content_surface(cx))
            .child(nav_bar(
                back,
                crate::tr!("chat.panels").into_owned().into(),
                None,
                vec![],
                window,
                cx,
            ))
            .child(
                div()
                    .flex_none()
                    .px(px(crate::material::COMPACT_PAGE_INSET))
                    .py(cx.theme().tokens.spacing.sm)
                    .child(segments),
            )
            .child(div().flex_1().min_h_0().child(body))
            .into_any_element()
    }

    fn render_pull_request_page(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let back = self.back_control(cx);
        let Some(attachment) = &self.attachment else {
            return div().into_any_element();
        };
        let view = attachment.chat.read(cx).pull_request_view(cx);
        let session = attachment.link.store.read(cx).active_session_id();
        let key = session.as_deref().and_then(|session| {
            attachment
                .link
                .store
                .read(cx)
                .open_pull_request(session)
                .cloned()
        });
        if let (Some(session), Some(key)) = (session, key) {
            view.update(cx, |view, cx| view.show(session, key, cx));
        } else if self.destination(cx) == Destination::PullRequest {
            // The thread no longer shows the pull request: back to the thread.
            let window_state = self.window_state.clone();
            window.defer(cx, move |_, cx| {
                window_state.update(cx, |state, cx| state.back(cx));
            });
        }
        let (title, actions) = view.update(cx, |view, cx| (view.nav_title(), view.nav_actions(cx)));
        v_flex()
            .size_full()
            .bg(crate::material::content_surface(cx))
            .child(nav_bar(back, title, None, actions, window, cx))
            .child(div().flex_1().min_h_0().child(view))
            .into_any_element()
    }

    /// Push the panel destination with something actually on it.
    fn open_panels(&mut self, cx: &mut Context<Self>) {
        let showing = self.attachment.as_ref().is_some_and(|attachment| {
            let panel = attachment.link.store.read(cx).panel_state();
            if self.panel_shows_terminal {
                panel.terminal_open
            } else {
                panel.right_panel_open
            }
        });
        if !showing {
            self.show_panel(
                if self.panel_shows_terminal {
                    "terminal"
                } else {
                    "diff"
                },
                cx,
            );
        }
        self.go(Destination::Panel, cx);
    }

    fn show_panel(&mut self, id: &'static str, cx: &mut Context<Self>) {
        if let Some(pending) = &mut self.pending_navigation_restore {
            pending.panel = match id {
                "terminal" => NavigationPanel::Terminal,
                "plan" => NavigationPanel::Plan,
                "agents" => NavigationPanel::Agents,
                "pr" => NavigationPanel::PullRequests,
                "preview" => NavigationPanel::Preview,
                _ => NavigationPanel::Diff,
            };
            pending.panel_open = true;
        }
        let Some(attachment) = &self.attachment else {
            return;
        };
        let store = attachment.link.store.clone();
        let panel = store.read(cx).panel_state();
        self.panel_shows_terminal = id == "terminal";
        store.update(cx, |store, cx| match id {
            "terminal" => {
                if !panel.terminal_open {
                    store.toggle_terminal_panel(cx);
                }
            }
            tab => {
                let tab = match tab {
                    "plan" => RightTab::Plan,
                    "agents" => RightTab::Agents,
                    "pr" => RightTab::PullRequests,
                    "preview" => RightTab::Preview,
                    _ => RightTab::Diff,
                };
                if !(panel.right_panel_open && panel.right_tab == tab) {
                    match tab {
                        RightTab::Plan => store.toggle_plan_panel(cx),
                        RightTab::Agents => store.toggle_agents_panel(cx),
                        RightTab::PullRequests => store.toggle_pull_requests_panel(cx),
                        RightTab::Preview => store.toggle_preview_panel(cx),
                        _ => store.toggle_diff_panel(cx),
                    }
                }
            }
        });
        self.schedule_navigation_save(cx);
        cx.notify();
    }

    fn begin_edge_back(
        &mut self,
        origin: gpui::Point<Pixels>,
        width: Pixels,
        window: &Window,
        cx: &mut Context<Self>,
    ) {
        if self
            .page_back
            .as_ref()
            .is_some_and(|page| page.target.is_some())
        {
            return;
        }
        let sheet = crate::overlay::active_sheet(window, cx);
        self.page_back = if sheet.is_none()
            && self.compact(cx)
            && !(self.destination(cx) == Destination::Thread && self.agent_lead(cx).is_some())
            && !crate::window_seam::keyboard_covers_window(window)
        {
            let mut views = self.nav.read(cx).views().cloned().collect::<Vec<_>>();
            views.pop().and_then(|current| {
                views.pop().map(|previous| PageBack {
                    previous,
                    current,
                    progress: 0.,
                    target: None,
                })
            })
        } else {
            None
        };
        self.back_swipe = Some(BackSwipe {
            origin,
            progress: crate::edge_back::BackProgress {
                width,
                ..Default::default()
            },
            sheet,
            last_moved_at: cx.background_executor().now(),
            finished: false,
        });
    }

    fn move_edge_back(&mut self, phase: TouchPhase, window: &mut Window, cx: &mut Context<Self>) {
        let Some(swipe) = &mut self.back_swipe else {
            return;
        };
        if swipe.finished {
            return;
        }
        let ended = matches!(phase, TouchPhase::Ended | TouchPhase::Cancelled);
        let commit = phase == TouchPhase::Ended && swipe.progress.commits();
        let progress = swipe.progress.progress();
        if let Some(sheet) = &swipe.sheet {
            let close = sheet.update(cx, |sheet, cx| {
                sheet.dragging = !ended;
                sheet.progress = if ended {
                    if commit { 1. } else { 0. }
                } else {
                    progress
                };
                App::notify(cx, sheet.parent);
                sheet.close.clone()
            });
            if commit {
                close(window, cx);
            }
        } else if let Some(page) = &mut self.page_back {
            page.progress = progress;
            if ended {
                page.target = Some(if commit { 1. } else { 0. });
            }
        } else if commit {
            self.back(window, cx);
        }
        if ended && let Some(swipe) = &mut self.back_swipe {
            swipe.finished = true;
        }
        cx.notify();
    }

    // gpui-base has no edge recognizer; observe GPUI's original pan contact so
    // ordinary taps still reach the Back control. UIKit supplies this on iOS.
    fn swipe_back_stack(
        &self,
        stack: impl IntoElement,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let shell = cx.entity().downgrade();
        // Edge capture must run before ScrollBounce can consume the release packet.
        div().relative().size_full().children([
            canvas(
                |bounds, window, _| window.insert_hitbox(bounds, HitboxBehavior::Normal),
                move |bounds, hitbox, window, _| {
                    // Claiming TouchDrag would swallow edge taps. GPUI keeps
                    // a pan's original contact in ScrollWheelEvent.position.
                    window.on_mouse_event(
                        move |event: &gpui::ScrollWheelEvent, phase, window, cx| {
                            if !phase.capture() {
                                return;
                            }
                            let gpui::ScrollDelta::Pixels(delta) = event.delta else {
                                return;
                            };
                            let Some(shell) = shell.upgrade() else { return };
                            shell.update(cx, |shell, cx| {
                                if delta.y != px(0.) {
                                    shell.move_edge_back(TouchPhase::Cancelled, window, cx);
                                    return;
                                }
                                if event.touch_phase == TouchPhase::Started {
                                    shell.back_swipe = None;
                                    if (crate::overlay::active_sheet(window, cx).is_none()
                                        && (!hitbox.should_handle_scroll(window)
                                            || gpui_base::GlobalState::is_in_deferred_context(cx)
                                            || shell.window_state.read(cx).palette_open
                                            || !shell.compact(cx)
                                            || shell.nav.read(cx).depth() <= 1))
                                        || !bounds.contains(&event.position)
                                        || event.position.x - bounds.left() > px(20.)
                                    {
                                        return;
                                    }
                                    shell.begin_edge_back(
                                        event.position,
                                        bounds.size.width,
                                        window,
                                        cx,
                                    );
                                }
                                let Some(swipe) = shell.back_swipe.as_mut() else {
                                    return;
                                };
                                if event.position != swipe.origin {
                                    return;
                                }
                                cx.stop_propagation();
                                if swipe.finished {
                                    return;
                                }
                                let now = cx.background_executor().now();
                                swipe.progress.travel += delta.x;
                                if event.touch_phase == TouchPhase::Moved && delta.x != px(0.) {
                                    let elapsed =
                                        now.duration_since(swipe.last_moved_at).as_secs_f32();
                                    swipe.progress.velocity =
                                        f32::from(delta.x) / elapsed.max(0.001);
                                    swipe.last_moved_at = now;
                                }
                                if !matches!(
                                    event.touch_phase,
                                    TouchPhase::Ended | TouchPhase::Cancelled
                                ) {
                                    shell.move_edge_back(event.touch_phase, window, cx);
                                }
                                let Some(swipe) = shell.back_swipe.as_mut() else {
                                    return;
                                };
                                if matches!(
                                    event.touch_phase,
                                    TouchPhase::Ended | TouchPhase::Cancelled
                                ) {
                                    if now.duration_since(swipe.last_moved_at)
                                        > Duration::from_millis(100)
                                    {
                                        swipe.progress.velocity = 0.;
                                    }
                                    let origin = swipe.origin;
                                    shell.move_edge_back(event.touch_phase, window, cx);
                                    if event.touch_phase == TouchPhase::Ended {
                                        crate::edge_back::catch_fling(origin, window, cx);
                                    }
                                }
                            });
                        },
                    );
                },
            )
            .absolute()
            .inset_0()
            .into_any_element(),
            stack.into_any_element(),
        ])
    }

    fn render_compact(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        if self.nav.read(cx).is_empty() {
            self.sync_nav(NavMotion::Immediate, cx);
        }
        let width = window.viewport_size().width;
        // gpui-base exposes NavPage::progress read-only and mounts the
        // previous entry only after pop. Bind interactive progress here, at the
        // navigation owner, until base supplies that API. Retain the same views;
        // compact ChatView excludes the terminal, so Thread and Panel never
        // mount the composer or terminal twice.
        if self.page_back.is_none() {
            crate::edge_back::settle("page-edge-back", 0., true, window, cx);
        }
        let interactive = self.page_back.as_ref().map(|page| {
            let progress = crate::edge_back::settle(
                "page-edge-back",
                page.target.unwrap_or(page.progress),
                page.target.is_none(),
                window,
                cx,
            );
            (
                page.previous.clone(),
                page.current.clone(),
                progress,
                page.target,
            )
        });
        let interactive = interactive.and_then(|(previous, current, progress, target)| {
            if target == Some(progress) {
                self.page_back = None;
                if target == Some(1.) {
                    self.back(window, cx);
                    self.sync_nav(NavMotion::Immediate, cx);
                }
                None
            } else {
                Some((previous, current, progress))
            }
        });
        // 200ms lateral push/pop. GPUI has no paint transform, so each page is
        // offset by its own insets; setting `left` and `right` in opposite
        // directions slides it without resizing it.
        let stack = if let Some((previous, current, progress)) = interactive {
            let offset = crate::edge_back::BackProgress::offset(progress, width);
            let underneath = (offset - width) * 0.25;
            div()
                .relative()
                .size_full()
                .overflow_hidden()
                .child(
                    div()
                        .absolute()
                        .top_0()
                        .bottom_0()
                        .left(underneath)
                        .right(-underneath)
                        .debug_selector(|| "back-previous-page".into())
                        .child(previous)
                        .child(
                            div()
                                .absolute()
                                .inset_0()
                                .bg(gpui::black().opacity(0.15 * (1. - progress))),
                        ),
                )
                .child(
                    div()
                        .absolute()
                        .top_0()
                        .bottom_0()
                        .left(offset)
                        .right(-offset)
                        .debug_selector(|| "back-current-page".into())
                        .child(current),
                )
                .child(div().absolute().inset_0().occlude())
                .into_any_element()
        } else {
            NavStack::new(&self.nav)
                .size_full()
                .overflow_hidden()
                .transition(Transition::new(NAV_TRANSITION))
                .item(move |page, _, _| {
                    let rest = 1. - page.progress();
                    let (offset, opacity) = match (page.phase(), page.operation()) {
                        (PresencePhase::Entering, Some(NavOperation::Pop)) => {
                            (width * -0.25 * rest, 1.)
                        }
                        (PresencePhase::Entering, Some(_)) => (width * rest, page.progress()),
                        (PresencePhase::Exiting, Some(NavOperation::Pop)) => {
                            (width * page.progress(), 1.)
                        }
                        (PresencePhase::Exiting, Some(_)) => (width * -0.25 * page.progress(), 1.),
                        _ => (px(0.), 1.),
                    };
                    page.left(offset)
                        .right(px(0.) - offset)
                        .opacity(opacity)
                        .into_any_element()
                })
                .into_any_element()
        };
        v_flex()
            .id("app-shell")
            .relative()
            .size_full()
            .bg(crate::material::content_surface(cx))
            .text_color(cx.theme().foreground)
            .when(cfg!(target_os = "android"), |mut element| {
                // Android's cosmic-text fallback has no script preferences. These
                // system sans families keep Han text out of arbitrary serif faces.
                element.text_style().font_fallbacks = Some(gpui::FontFallbacks::from_fonts(vec![
                    "Noto Sans CJK SC".into(),
                    "Noto Sans SC".into(),
                    "Source Han Sans SC".into(),
                ]));
                element
            })
            .text_token(&cx.theme().tokens.typography.md)
            .on_action(cx.listener(Self::on_toggle_palette))
            .on_action(cx.listener(Self::on_open_source_control))
            .key_context("TcodeShell")
            .on_action(cx.listener(Self::on_thread_undo))
            .on_action(cx.listener(Self::on_open_thread_anchor))
            // Every compact page, settings included, is one entry of the same
            // stack: one nav bar, one Back, one transition.
            .child(stack)
            .map(|el| {
                el.children(
                    self.attachment
                        .as_ref()
                        .map(|attachment| attachment.palette.clone()),
                )
            })
            .into_any_element()
    }

    /// Constrain the shell to the window's safe content rectangle. Backgrounds
    /// paint edge to edge — the surface below is the window's — and interactive
    /// content is inset exactly once, at both widths. A window the system does
    /// not occlude gets no wrapper at all.
    fn within_seam(&self, body: AnyElement, window: &Window, cx: &mut Context<Self>) -> AnyElement {
        let seam = crate::window_seam::content_insets(window);
        if seam == gpui::Edges::default() {
            return body;
        }
        div()
            .size_full()
            .bg(crate::material::opaque_canvas(cx))
            .pt(seam.top)
            .pb(seam.bottom)
            .pl(seam.left)
            .pr(seam.right)
            .child(body)
            .into_any_element()
    }
}

// ---------------------------------------------------------------------------
// Wide layout
// ---------------------------------------------------------------------------

impl AppShell {
    /// The hosts surface itself, without chrome: the same rows and the same
    /// pairing form at both widths. Compact pushes Pair onto the navigation
    /// stack; wide shows it in place, inside the Hosts route.
    fn hosts_body(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let pairing = self.destination(cx) == Destination::Pair;
        let hosts = self.hosts.clone();
        hosts.update(cx, |panel, cx| {
            if pairing {
                panel.render_pair(window, cx)
            } else {
                panel.render_hosts(window, cx)
            }
        })
    }

    /// The wide Hosts route's top strip: the page title and (on Windows) the
    /// caption cluster this route's content column owns. The sidebar switches
    /// wide routes, so this strip deliberately has no Back control.
    fn render_hosts_header(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let hosts_caption = window_caption::hosts_caption_for_state(
            window_caption::CaptionSurface::Hosts,
            self.window_state.read(cx).route(),
            false,
            RightTab::Diff,
        );
        let pairing = self.destination(cx) == Destination::Pair;
        let title: SharedString = if pairing {
            crate::tr!("hosts.pair.page_title").into_owned().into()
        } else {
            crate::tr!("hosts.title").into_owned().into()
        };
        // Machines is a root the sidebar switches to; Pair is pushed over it,
        // so only Pair has somewhere to go back to.
        let back = pairing.then(|| {
            back_button("hosts-back", Destination::Hosts.back_label())
                .debug_selector(|| "hosts-back".into())
                .on_click(cx.listener(|this, _, _, cx| {
                    this.window_state
                        .update(cx, |state, cx| state.close_pair(cx));
                }))
        });
        window_drag_area(
            "hosts-header-drag",
            crate::settings_rows::content_header(
                crate::material::bar("hosts-header", crate::material::Bar::Strip, false, cx),
                cx,
            )
            .when(hosts_caption, |strip| strip.relative()),
            window,
            cx,
        )
        .child(
            crate::settings_rows::content_header_column()
                .items_center()
                .gap(cx.theme().tokens.spacing.xs)
                .when(hosts_caption, |column| {
                    column.pr(px(window_caption::CAPTION_CLUSTER_WIDTH))
                })
                .children(back)
                .child(window_caption::drag_region(crate::material::bar_title(
                    title, cx,
                ))),
        )
        .children(hosts_caption.then(|| {
            div()
                .absolute()
                .top_0()
                .right_0()
                .h_full()
                .child(window_caption::caption_controls(window, cx))
        }))
        .into_any_element()
    }

    fn render_wide(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let route = self.window_state.read(cx).route();
        let palette_open = self.window_state.read(cx).palette_open;
        let fullscreen = window.is_fullscreen();
        if self.attachment.is_none() {
            // No host: the hosts page is the whole window, and the only page
            // there is. A tablet in landscape lands here before it attaches
            // and a desktop after it disconnects; neither has a sidebar to
            // switch routes with, so the page wears the same nav bar as its
            // compact self — the title, and on Pair the Back that returns to
            // Machines. Whatever the history says, nothing else is shown
            // until `attach` enters a workspace.
            let page = if self.destination(cx) == Destination::Pair {
                self.render_pair_page(window, cx)
            } else {
                self.render_hosts_page(window, cx)
            };
            return div()
                .id("app-shell")
                .size_full()
                .bg(crate::material::opaque_canvas(cx))
                .text_color(cx.theme().foreground)
                .child(v_flex().id("hosts").size_full().child(page))
                .into_any_element();
        }
        let Some(attachment) = &mut self.attachment else {
            unreachable!("checked above");
        };
        let collapsed = self.window_state.read(cx).sidebar_collapsed;
        // The overlay is workspace-only transient state. Clear it synchronously
        // on route/expanded transitions rather than waiting for pointer input.
        if !collapsed || route != Route::Chat {
            attachment.sidebar_overlay_visible = false;
        }
        let panel = attachment.link.store.read(cx).panel_state();
        let diff_open = panel.right_panel_open;
        let right_tab = panel.right_tab;
        let diff_expanded = panel.right_panel_expanded;
        // "Expanded" (full-width) is a diff-only affordance; the preview tab
        // always shares the split so the webview keeps a stable size.
        let diff_expanded = diff_expanded && right_tab != RightTab::Preview;

        // A native WebView is not composited into GPUI and survives removal of
        // its layout node. Synchronize it before the settings early-return or a
        // right-panel tab/close transition can unmount PreviewPanel.
        attachment
            .preview
            .update(cx, |preview, cx| preview.sync_visibility(cx));

        // Settings is a window of its own: opening it switches the whole
        // window, so the settings rail — not the workspace sidebar — is the
        // left column. Root owns the translucent canvas across both routes;
        // Settings paints its own rail and content surfaces over it.
        if route == Route::Settings {
            return div()
                .id("app-shell")
                .size_full()
                // Fullscreen flattens the canvas under the paper (see the
                // workspace root below).
                .when(fullscreen, |this| {
                    this.bg(crate::material::opaque_canvas(cx))
                })
                .text_color(cx.theme().foreground)
                .on_action(cx.listener(Self::on_toggle_palette))
                .on_action(cx.listener(Self::on_open_source_control))
                .key_context("TcodeShell")
                .on_action(cx.listener(Self::on_thread_undo))
                .on_action(cx.listener(Self::on_open_thread_anchor))
                .child(
                    div()
                        .id("workspace")
                        .flex_1()
                        .size_full()
                        .min_h_0()
                        .overflow_hidden()
                        .debug_selector(|| "settings-route".into())
                        .child(attachment.settings_page.clone()),
                )
                .into_any_element();
        }

        // Hosts is a product surface, not a settings page: the sidebar stays,
        // because the feature area that leads here is meant to be persistent.
        if route == Route::Hosts {
            let sidebar = attachment.sidebar.clone();
            let sidebar_width = attachment.sidebar_width.get();
            let body = self.hosts_body(window, cx);
            let header = self.render_hosts_header(window, cx);
            return div()
                .id("app-shell")
                .size_full()
                .when(fullscreen, |this| {
                    this.bg(crate::material::opaque_canvas(cx))
                })
                .text_color(cx.theme().foreground)
                .on_action(cx.listener(Self::on_toggle_palette))
                .on_action(cx.listener(Self::on_open_source_control))
                .key_context("TcodeShell")
                .on_action(cx.listener(Self::on_thread_undo))
                .on_action(cx.listener(Self::on_open_thread_anchor))
                .child(
                    h_flex()
                        .id("workspace")
                        .size_full()
                        .min_h_0()
                        .overflow_hidden()
                        .when(!collapsed, |row| {
                            row.child(div().flex_none().w(sidebar_width).h_full().child(sidebar))
                        })
                        .child(
                            v_flex()
                                .flex_1()
                                .min_w_0()
                                .h_full()
                                .debug_selector(|| "hosts-route".into())
                                .bg(crate::material::content_surface(cx))
                                .shadow_sm()
                                .child(header)
                                .child(body),
                        ),
                )
                .into_any_element();
        }

        // Which entity fills the right panel: the Preview tab shows the embedded
        // browser; Diff/Plan share the DiffPanel container.
        let right_panel: AnyElement = if right_tab == RightTab::Preview {
            attachment.preview.clone().into_any_element()
        } else {
            attachment.diff.clone().into_any_element()
        };
        let sidebar_layout_width = if collapsed {
            px(0.)
        } else {
            attachment.sidebar_width.get()
        };

        // Sidebar | chat | right panel live in ONE resizable group. Nesting a
        // second group inside the chat panel does not shrink the chat: it keeps
        // its full width and the right panel is painted over it, clipping the
        // timeline and the composer mid-word. A flat group makes the chat a real
        // flex sibling of the right panel, so it reflows instead of clipping.
        let chat_visible = !(diff_open && diff_expanded);
        // The right panel is the last of three panels (sidebar · chat · right),
        // and the sidebar is only a panel when it is expanded.
        let right_ix = if collapsed { 1 } else { 2 };
        // Panel state is indexed by position; the two group shapes need their
        // own state so the chat's size never becomes the sidebar's size.
        let split = if collapsed {
            attachment.collapsed_split.clone()
        } else {
            attachment.split.clone()
        };

        let viewport_width = window.viewport_size().width;
        let layout_changed = self.last_split_layout != Some((viewport_width, collapsed));
        if layout_changed {
            self.last_split_layout = Some((viewport_width, collapsed));
            attachment.right_restore_pending = true;
        }

        // Too narrow for the chat and the panel side by side: the panel floats
        // over the chat's right edge and the chat keeps the whole column
        // underneath, rather than either being squeezed below its minimum.
        let beside_sidebar = viewport_width - sidebar_layout_width;
        let chat_min = px(CHAT_MIN_WIDTH
            + crate::chat::header_extra_width(
                attachment.link.store.read(cx),
                collapsed,
                fullscreen,
                false,
            ));
        let right_floats =
            diff_open && chat_visible && beside_sidebar < chat_min + px(RIGHT_PANEL_MIN_WIDTH);
        attachment.floating_focus.update(cx, |focus, cx| {
            focus.sync_open(right_floats, window, cx);
        });
        let floating_width = attachment
            .right_width
            .get()
            .max(px(RIGHT_PANEL_MIN_WIDTH))
            .min(beside_sidebar);
        if !right_floats || layout_changed {
            attachment.floating_restore_pending = true;
        }
        if right_floats && attachment.floating_restore_pending {
            let restored =
                attachment
                    .floating_split
                    .update(cx, |state, cx| match state.sizes().get(1) {
                        Some(width) if (*width - floating_width).abs() > px(0.5) => {
                            state.resize_panel(1, floating_width, window, cx);
                            false
                        }
                        Some(_) => true,
                        None => false,
                    });
            if restored && !layout_changed {
                attachment.floating_restore_pending = false;
            }
        }
        let right_docked = diff_open && !right_floats;
        let (docked_panel, floating_panel) = if right_floats {
            (None, Some(right_panel))
        } else {
            (Some(right_panel), None)
        };

        // Give the docked right panel its width, as much of it as leaves the
        // chat its minimum, once the group knows about the panel (its count is
        // synced while the group renders, so this lands on the frame after it
        // docks — the group notifies, so that frame comes).
        if !(right_docked && chat_visible) {
            attachment.right_restore_pending = true;
        } else if attachment.right_restore_pending {
            let width = attachment
                .right_width
                .get()
                .min(beside_sidebar - chat_min)
                .max(px(RIGHT_PANEL_MIN_WIDTH));
            let restored = split.update(cx, |state, cx| match state.sizes().get(right_ix) {
                Some(size) if (f32::from(*size) - f32::from(width)).abs() > 0.5 => {
                    state.resize_panel(right_ix, width, window, cx);
                    false
                }
                Some(_) => true,
                None => false,
            });
            // A match on the frame the layout changed is the old layout, not a
            // settled one.
            if restored && !layout_changed {
                attachment.right_restore_pending = false;
            }
        }

        // Chat and right-panel reading surfaces sit above the translucent canvas.
        let chat_panel = resizable_panel()
            .visible(chat_visible)
            .when(right_docked, |panel| {
                panel.size_range(chat_min..Pixels::MAX)
            })
            .child(
                v_flex()
                    .size_full()
                    .bg(crate::material::content_surface(cx))
                    .shadow_sm()
                    .child(
                        div().flex_1().min_h_0().child(
                            self.attachment
                                .as_ref()
                                .expect("attachment checked above")
                                .chat
                                .clone(),
                        ),
                    ),
            );
        let attachment = self.attachment.as_ref().expect("attachment checked above");
        // Beside the chat, `flex_none`: whatever the group's sizes leave over or
        // lack goes to the chat, so the panel is exactly the width restored
        // above. Expanded, there is no chat, so the panel grows into it all.
        let right = resizable_panel()
            .when(chat_visible, |panel| panel.flex_none())
            .visible(right_docked)
            .size(px(RIGHT_PANEL_WIDTH))
            .size_range(
                px(RIGHT_PANEL_MIN_WIDTH)..if chat_visible { px(1400.) } else { Pixels::MAX },
            )
            .child(
                div()
                    .debug_selector(|| "right-panel".into())
                    .size_full()
                    .bg(crate::material::content_surface(cx))
                    .shadow_sm()
                    .children(docked_panel),
            );
        // gpui-base has no API to disable one handle in a group; cover the
        // expanded panel's handle so dragging it cannot change its restored width.
        let handle_cover = (!chat_visible).then(|| {
            div()
                .absolute()
                .top_0()
                .left(sidebar_layout_width - px(5.))
                .w(px(10.))
                .h_full()
                .occlude()
        });
        let floating_right = floating_panel.map(|panel| {
            let focus = attachment.floating_focus.read(cx).focus_handle(cx);
            let remembered = attachment.right_width.clone();
            div()
                .absolute()
                .top_0()
                .right_0()
                .h_full()
                .w(beside_sidebar)
                .child(
                    h_resizable("floating-right-panels")
                        .with_state(&attachment.floating_split)
                        .on_resize(move |state, _, cx| {
                            if let Some(width) = state.read(cx).sizes().get(1) {
                                remembered.set(*width);
                            }
                        })
                        .child(resizable_panel().size_range(px(0.)..Pixels::MAX))
                        .child(
                            resizable_panel()
                                .flex_none()
                                .size(floating_width)
                                .size_range(
                                    px(RIGHT_PANEL_MIN_WIDTH).min(beside_sidebar)..beside_sidebar,
                                )
                                .child(
                                    crate::material::floating_panel(
                                        div().id("right-panel-overlay"),
                                        crate::material::Side::Right,
                                        cx,
                                    )
                                    .debug_selector(|| "right-panel-overlay".into())
                                    .size_full()
                                    .role(Role::Dialog)
                                    .tab_group()
                                    .track_focus(&focus)
                                    .key_context("Popover")
                                    .on_action(window.listener_for(
                                        &attachment.floating_focus,
                                        gpui_base::PopoverState::on_action_cancel,
                                    ))
                                    .occlude()
                                    .child(panel),
                                ),
                        ),
                )
        });

        let remembered_right = attachment.right_width.clone();
        let remembered_sidebar = attachment.sidebar_width.clone();
        let sidebar = attachment.sidebar.clone();
        let overlay_width = attachment.sidebar_width.get();
        let overlay_visible = attachment.sidebar_overlay_visible;
        let palette = attachment.palette.clone();
        let group = move |id: &'static str| {
            h_resizable(id)
                .with_state(&split)
                .on_resize(move |state, _, cx| {
                    // The group does not say which handle was dragged. A drag
                    // that moved the sidebar was the sidebar's, and whatever it
                    // squeezed the panel to is not a width the user chose.
                    let sizes = state.read(cx).sizes();
                    let sidebar = (!collapsed).then(|| sizes.first()).flatten();
                    match sidebar {
                        Some(size) if (f32::from(*size - remembered_sidebar.get())).abs() > 0.5 => {
                            remembered_sidebar.set(*size);
                        }
                        _ => {
                            if let Some(size) = sizes.get(right_ix) {
                                remembered_right.set(*size);
                            }
                        }
                    }
                })
        };

        let workspace: AnyElement = if collapsed {
            // Zero layout width: the chat/right group owns the whole workspace
            // and runs to the window's left edge. The trigger and overlay are
            // independent absolute siblings, so neither reflows the columns.
            //
            // A popover keeps the overlay alive through its occluded-hover
            // false transition (see next_sidebar_overlay_visibility). When the
            // popover dismisses with the pointer already outside the overlay,
            // no hover event follows — reap the stale overlay here instead
            // (dismissal refreshes the window, so this pass always runs).
            let overlay_visible = if overlay_visible
                && !gpui_base::GlobalState::is_in_deferred_context(cx)
                && window.mouse_position().x > overlay_width
            {
                if let Some(attachment) = &mut self.attachment {
                    attachment.sidebar_overlay_visible = false;
                }
                false
            } else {
                overlay_visible
            };
            div()
                .relative()
                .size_full()
                .child(
                    group("chat-diff-panels")
                        .child(chat_panel)
                        .when(right_docked, |group| group.child(right)),
                )
                .children(handle_cover)
                .children(floating_right)
                // This fixed transparent strip only opens the overlay. Its
                // inevitable false transition when the overlay occludes it is
                // deliberately ignored by the state machine.
                .child(
                    div()
                        .id("sidebar-hover-trigger")
                        .absolute()
                        .left_0()
                        .top_0()
                        .h_full()
                        .w(px(SIDEBAR_HOVER_EDGE))
                        .occlude()
                        .on_hover(cx.listener(|this, hovered: &bool, _, cx| {
                            this.update_sidebar_overlay(
                                SidebarHoverTransition::Trigger(*hovered),
                                cx,
                            );
                        })),
                )
                .when(overlay_visible, |this| {
                    this.child(
                        crate::material::floating_panel(
                            div().id("sidebar-hover-overlay"),
                            crate::material::Side::Left,
                            cx,
                        )
                        .absolute()
                        .left_0()
                        .top_0()
                        .h_full()
                        .w(overlay_width)
                        // The blocker and hover listener share this hitbox:
                        // the overlay owns its full visible lifetime.
                        .occlude()
                        .on_hover(cx.listener(|this, hovered: &bool, _, cx| {
                            this.update_sidebar_overlay(
                                SidebarHoverTransition::Overlay(*hovered),
                                cx,
                            );
                        }))
                        .child(sidebar),
                    )
                })
                .into_any_element()
        } else {
            div()
                .relative()
                .size_full()
                .child(
                    group("workspace-panels")
                        .child(
                            resizable_panel()
                                .fixed()
                                .size(overlay_width)
                                .size_range(px(220.)..px(380.))
                                .child(sidebar),
                        )
                        .child(chat_panel)
                        .when(right_docked, |group| group.child(right)),
                )
                .children(handle_cover)
                .children(floating_right)
                .into_any_element()
        };

        // No separate titlebar: the sidebar and chat columns run to the window
        // top (the native traffic lights overlay the sidebar's top-left).
        div()
            .id("app-shell")
            .size_full()
            // Fullscreen only: a fullscreen Space has nothing but black behind
            // the vibrancy material, which muddies the translucent canvas —
            // cover it with its opaque base. Windowed, paint nothing here:
            // Root owns the translucent canvas.
            .when(fullscreen, |this| {
                this.bg(crate::material::opaque_canvas(cx))
            })
            .text_color(cx.theme().foreground)
            .on_action(cx.listener(Self::on_toggle_palette))
            .on_action(cx.listener(Self::on_open_source_control))
            .key_context("TcodeShell")
            .on_action(cx.listener(Self::on_thread_undo))
            .on_action(cx.listener(Self::on_open_thread_anchor))
            .child(
                div()
                    .id("workspace")
                    .flex_1()
                    .size_full()
                    .min_h_0()
                    .overflow_hidden()
                    .child(workspace),
            )
            .when(palette_open, |this| this.child(palette))
            .into_any_element()
    }

    fn update_sidebar_overlay(
        &mut self,
        transition: SidebarHoverTransition,
        cx: &mut Context<Self>,
    ) {
        let (collapsed, route) = {
            let state = self.window_state.read(cx);
            (state.sidebar_collapsed, state.route())
        };
        let popover_open = gpui_base::GlobalState::is_in_deferred_context(cx);
        let Some(attachment) = &mut self.attachment else {
            return;
        };
        let visible = next_sidebar_overlay_visibility(
            attachment.sidebar_overlay_visible,
            transition,
            collapsed,
            route,
            popover_open,
        );
        if attachment.sidebar_overlay_visible != visible {
            attachment.sidebar_overlay_visible = visible;
            cx.notify();
        }
    }
}

impl Render for AppShell {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // The window may have been resized without a bounds notification (a
        // first frame, or an inset change that narrowed the content box).
        self.sync_layout(window, cx);
        if let Some(attachment) = &self.attachment {
            let title = attachment.link.store.read(cx).shell_window_title();
            if title != self.window_title {
                window.set_window_title(&title);
                self.window_title = title;
            }
        }
        if (self.compact(cx) || self.window_state.read(cx).route() != Route::Chat)
            && let Some(attachment) = &self.attachment
        {
            attachment
                .floating_focus
                .update(cx, |focus, cx| focus.sync_open(false, window, cx));
        }
        // The palette takes focus on the frame it opens, at either width.
        let (palette_open, focus_query) = {
            let state = self.window_state.read(cx);
            (state.palette_open, state.palette_focuses_query)
        };
        if palette_open
            && !self.palette_was_open
            && let Some(attachment) = &self.attachment
        {
            attachment
                .palette
                .update(cx, |p, cx| p.open(focus_query, window, cx));
        }
        self.palette_was_open = palette_open;
        let body = if self.compact(cx) {
            self.render_compact(window, cx)
        } else {
            self.render_wide(window, cx)
        };
        let body = if crate::window_seam::is_mobile(cx) && !cfg!(target_os = "ios") {
            self.swipe_back_stack(body, cx).into_any_element()
        } else {
            body
        };
        let dismissal = self
            .attachment
            .as_ref()
            .map(|attachment| attachment.palette.read(cx).outside_dismissal.clone());
        div()
            .size_full()
            // gpui-base transfers focus between controls, but leaves it
            // intact on empty backgrounds. Mobile users need that tap to
            // dismiss the software keyboard too.
            .on_mouse_down(MouseButton::Left, |_, window, cx| {
                if crate::window_seam::is_mobile(cx) && !window.default_prevented() {
                    window.blur(cx);
                }
            })
            // Both layouts must select UI text explicitly: GPUI's default
            // fallback stack prefers Lilex once the terminal font is loaded.
            .font_family(cx.theme().font_family.clone())
            .children(dismissal.map(|dismissal| dismissal.release_listener().into_any_element()))
            .child(self.within_seam(body, window, cx))
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use gpui::{TestAppContext, VisualTestContext, size};
    use tcode_client::host::Transport;
    use tcode_protocol::{
        ClientPayload, Command, EventEnvelope, HostMessage, IndexSnapshot, ServerEvent, Topic,
        decode_client_line, encode_line,
    };

    use super::*;

    /// One shell over a transport the test holds both ends of, so what the
    /// client says to its host is observable and what the transport says
    /// about the link can be dictated.
    struct MountedShell {
        outgoing: async_channel::Receiver<String>,
        observed: RefCell<Vec<tcode_protocol::ClientMessage>>,
        incoming: async_channel::Sender<String>,
        states: async_channel::Sender<tcode_client::ConnectionState>,
    }

    struct ReturningClient {
        saved: tcode_client::pairing::PairedHost,
        /// One per attachment the test expects, in order.
        transports: RefCell<Vec<Transport>>,
        preferences: RefCell<tcode_client::host::ClientPreferences>,
        machine_exists: bool,
        saves: Cell<usize>,
        outbox: Option<std::sync::Arc<dyn tcode_client::outbox::Storage>>,
    }

    impl ClientHost for ReturningClient {
        fn outbox_storage(
            &self,
            _: &str,
        ) -> Option<std::sync::Arc<dyn tcode_client::outbox::Storage>> {
            self.outbox.clone()
        }
        fn device_name(&self) -> String {
            "returning phone".into()
        }

        fn device_id(&self) -> String {
            "returning-phone".into()
        }

        fn device_platform(&self) -> Option<String> {
            None
        }

        fn load_hosts(&self) -> Vec<tcode_client::pairing::PairedHost> {
            if self.machine_exists {
                vec![self.saved.clone()]
            } else {
                Vec::new()
            }
        }

        fn load_preferences(&self) -> tcode_client::host::ClientPreferences {
            self.preferences.borrow().clone()
        }

        fn save_preferences(&self, preferences: &tcode_client::host::ClientPreferences) {
            *self.preferences.borrow_mut() = preferences.clone();
            self.saves.set(self.saves.get() + 1);
        }

        fn save_hosts(&self, _: &[tcode_client::pairing::PairedHost]) {}

        fn last_host_id(&self) -> Option<String> {
            Some(self.saved.host_id.clone())
        }

        fn set_last_host_id(&self, host_id: Option<&str>) {
            assert_eq!(host_id, Some(self.saved.host_id.as_str()));
        }

        fn pair(
            &self,
            _: tcode_client::pairing::PairInvite,
            _: Option<std::net::IpAddr>,
        ) -> tcode_client::host::HostFuture<'_, Result<tcode_client::pairing::PairedHost, String>>
        {
            panic!("a cold start must reuse the saved pairing")
        }

        fn connect(&self, host: &tcode_client::pairing::PairedHost) -> Transport {
            assert_eq!(host.host_id, self.saved.host_id);
            let mut transports = self.transports.borrow_mut();
            assert!(!transports.is_empty(), "an unexpected connection");
            transports.remove(0)
        }
    }

    /// A returning phone: a mobile build at phone width, so the restored
    /// destination is a page of the compact stack.
    fn mount_restored<'a>(
        cx: &'a mut TestAppContext,
        history: &[&str],
        machine_exists: bool,
    ) -> (
        Entity<AppShell>,
        MountedShell,
        Rc<ReturningClient>,
        &'a mut VisualTestContext,
    ) {
        cx.update(|cx| crate::window_seam::override_mobile_for_test(cx, true));
        mount_restored_at_width(cx, history, machine_exists, 393., "plan")
    }

    fn mount_restored_at_width<'a>(
        cx: &'a mut TestAppContext,
        history: &[&str],
        machine_exists: bool,
        width: f32,
        panel: &str,
    ) -> (
        Entity<AppShell>,
        MountedShell,
        Rc<ReturningClient>,
        &'a mut VisualTestContext,
    ) {
        cx.update(crate::theme::init);
        let (to_host, outgoing) = async_channel::unbounded();
        let (incoming, from_host) = async_channel::unbounded();
        let (states, state) = async_channel::unbounded();
        let client = Rc::new(ReturningClient {
            saved: tcode_client::pairing::PairedHost {
                host_id: "last-host".into(),
                name: "Last machine".into(),
                traverse: Vec::new(),
                relay: None,
                addrs: vec!["127.0.0.1:47503".into()],
                last_connected_unix: Some(1),
                group_id: None,
                space_id: None,
                space_name: None,
            },
            transports: RefCell::new(vec![Transport {
                originals: None,
                to_host: to_host.into(),
                from_host,
                state,
                current_host: None,
            }]),
            preferences: RefCell::new(tcode_client::host::ClientPreferences {
                navigation: Some(serde_json::json!({
                    "history": history,
                    "host_id": "last-host",
                    "session_id": "thread-a",
                    "panel": panel
                })),
                ..Default::default()
            }),
            machine_exists,
            saves: Cell::new(0),
            outbox: None,
        });
        let setup_client = client.clone();
        let window = cx.open_window(size(px(width), px(852.)), move |window, cx| {
            let state = cx.new(|_| WindowState::new(false));
            AppShell::new(
                state,
                ShellSetup {
                    initial: crate::last_host_target(setup_client.as_ref()),
                    client_host: Some(setup_client),
                    restore_navigation: true,
                    ..Default::default()
                },
                window,
                cx,
            )
        });
        let shell = window.root(cx).unwrap();
        let cx = VisualTestContext::from_window(window.into(), cx).into_mut();
        if let Some(store) = shell.read_with(cx, |shell, _| shell.store()) {
            crate::store::tests::seed_full_scope(&store, &incoming, Vec::new(), cx);
        }
        draw(cx);
        (
            shell,
            MountedShell {
                outgoing,
                observed: RefCell::new(Vec::new()),
                incoming,
                states,
            },
            client,
            cx,
        )
    }

    /// Re-pairing with a machine ends on `switch_to` with that machine's id.
    /// The link it refused has stopped reconnecting, so it is rebuilt rather
    /// than kept saying the pairing was rejected; a pairing into one of its
    /// spaces is likewise a new link, not the full one it replaces.
    #[gpui::test]
    fn repairing_rebuilds_a_link_the_machine_rejected(cx: &mut TestAppContext) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        let (shell, host, client, cx) = mount_restored(cx, &["threads"], true);
        let fresh_transport = || {
            let (to_host, _outgoing) = async_channel::unbounded();
            let (_incoming, from_host) = async_channel::unbounded();
            let (_states, state) = async_channel::unbounded();
            Transport {
                originals: None,
                to_host: to_host.into(),
                from_host,
                state,
                current_host: None,
            }
        };
        let rejected = tcode_client::ConnectionState::Offline {
            reason: tcode_client::ConnectionFailure::AuthenticationRejected,
        };
        host.states.send_blocking(rejected.clone()).unwrap();
        cx.run_until_parked();
        assert_eq!(
            store_of(&shell, cx).read_with(cx, |store, _| store.connection_state()),
            rejected
        );
        let saved = client.saved.clone();
        client.transports.borrow_mut().push(fresh_transport());
        cx.update(|window, cx| {
            shell.update(cx, |shell, cx| {
                shell.switch_to(
                    AttachmentTarget::Remote(Box::new(saved.clone())),
                    window,
                    cx,
                )
            });
        });
        cx.run_until_parked();
        assert!(
            client.transports.borrow().is_empty(),
            "the refused link is replaced"
        );
        assert_ne!(
            store_of(&shell, cx).read_with(cx, |store, _| store.connection_state()),
            rejected
        );

        // The working link to the same machine is kept...
        cx.update(|window, cx| {
            shell.update(cx, |shell, cx| {
                shell.switch_to(
                    AttachmentTarget::Remote(Box::new(saved.clone())),
                    window,
                    cx,
                )
            });
        });
        cx.run_until_parked();

        // ...and joining one of its spaces is a different link.
        client.transports.borrow_mut().push(fresh_transport());
        let member = tcode_client::pairing::PairedHost {
            space_id: Some("space-1".into()),
            space_name: Some("Shared".into()),
            ..saved
        };
        cx.update(|window, cx| {
            shell.update(cx, |shell, cx| {
                shell.switch_to(AttachmentTarget::Remote(Box::new(member)), window, cx)
            });
        });
        cx.run_until_parked();
        assert!(
            client.transports.borrow().is_empty(),
            "a space membership replaces the full link"
        );
    }

    #[gpui::test]
    fn pending_outbox_overrides_desktop_launch_without_a_snapshot(cx: &mut TestAppContext) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        struct SavedOutbox;
        impl tcode_client::outbox::Storage for SavedOutbox {
            fn load(
                &self,
            ) -> Result<Vec<tcode_client::outbox::Entry>, tcode_protocol::ProtocolError>
            {
                Ok(vec![tcode_client::outbox::Entry {
                    key: "saved-send".into(),
                    command: Command::SendTurn {
                        session_id: "pending-thread".into(),
                        text: "survives relaunch".into(),
                        attachment_paths: Vec::new(),
                    },
                }])
            }
            fn save(
                &self,
                _: &[tcode_client::outbox::Entry],
            ) -> Result<(), tcode_protocol::ProtocolError> {
                Ok(())
            }
        }
        cx.update(crate::theme::init);
        cx.update(crate::markdown::init);
        let (to_host, _outgoing) = async_channel::unbounded();
        let (_incoming, from_host) = async_channel::unbounded();
        let (_state_tx, state) = async_channel::unbounded();
        let client = Rc::new(ReturningClient {
            saved: tcode_client::pairing::PairedHost {
                host_id: "pending-host".into(),
                name: "Pending host".into(),
                traverse: Vec::new(),
                relay: None,
                addrs: vec!["127.0.0.1:48442".into()],
                last_connected_unix: None,
                group_id: None,
                space_id: None,
                space_name: None,
            },
            transports: RefCell::new(vec![Transport {
                originals: None,
                to_host: to_host.into(),
                from_host,
                state,
                current_host: None,
            }]),
            preferences: RefCell::new(Default::default()),
            machine_exists: true,
            saves: Cell::new(0),
            outbox: Some(std::sync::Arc::new(SavedOutbox)),
        });
        let (shell, cx) = cx.add_window_view(|window, cx| {
            let state = cx.new(|_| WindowState::new(false));
            AppShell::new(
                state,
                ShellSetup {
                    initial: Some(AttachmentTarget::Local),
                    client_host: Some(client),
                    restore_navigation: false,
                    ..Default::default()
                },
                window,
                cx,
            )
        });
        cx.simulate_resize(size(px(1024.), px(700.)));
        draw(cx);
        let store = store_of(&shell, cx);
        store.read_with(cx, |store, _| {
            assert_eq!(store.remote_host_name(), Some("Pending host"));
            assert_eq!(store.active_session_id().as_deref(), Some("pending-thread"));
            assert!(!store.chat_loading());
            assert_eq!(store.delivery_messages()[0].1, "survives relaunch");
            assert!(store.sidebar_sessions().is_empty());
        });
        assert!(cx.debug_bounds("pending-delivery-bubble").is_some());
        assert!(cx.debug_bounds("pending-thread-row").is_some());
        as_mobile(cx);
        cx.simulate_resize(size(px(393.), px(852.)));
        let navigation = shell.read_with(cx, |shell, _| shell.window_state());
        navigation.update(cx, |state, cx| state.enter_workspace(cx));
        draw(cx);
        shell.read_with(cx, |shell, cx| {
            assert!(shell.compact(cx));
            assert_eq!(shell.destination(cx), Destination::Threads);
        });
        cx.update(|window, cx| {
            crate::settings_page::apply_theme(tcode_core::settings::ThemeMode::Dark, window, cx)
        });
        draw(cx);
        assert!(
            cx.debug_bounds("pending-thread-row").is_some(),
            "compact Threads exposes the outbox without a cached index"
        );
        navigation.update(cx, |state, cx| state.open_thread(cx));
        draw(cx);
        assert!(cx.debug_bounds("pending-delivery-bubble").is_some());
    }

    fn await_restore_update(
        shell: &Entity<AppShell>,
        cx: &mut VisualTestContext,
        ready: impl Fn(&WorkspaceStore) -> bool,
    ) {
        let store = store_of(shell, cx);
        for _ in 0..500 {
            store.update(cx, |store, cx| store.drain_host_events_for_test(cx));
            draw(cx);
            if store.read_with(cx, |store, _| ready(store)) {
                return;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        panic!("restore host update never arrived");
    }

    fn restore_index(
        shell: &Entity<AppShell>,
        host: &MountedShell,
        present: bool,
        cx: &mut VisualTestContext,
    ) {
        while let Ok(line) = host.outgoing.try_recv() {
            host.observed
                .borrow_mut()
                .push(decode_client_line(&line).unwrap());
        }
        let scope_request = host
            .observed
            .borrow()
            .iter()
            .rev()
            .find_map(|message| {
                matches!(
                    message.payload,
                    ClientPayload::Subscribe(tcode_protocol::Subscription {
                        topic: Topic::Scope,
                        ..
                    })
                )
                .then_some(message.id)
            })
            .expect("Scope subscription");
        crate::store::tests::seed_full_scope_reply(
            &store_of(shell, cx),
            &host.incoming,
            scope_request,
            Vec::new(),
            cx,
        );
        let mut meta = tcode_core::project::SessionMeta::new(
            agent::ProviderKind::Codex,
            "/project".into(),
            None,
        );
        meta.id = "thread-a".into();
        host.incoming
            .try_send(
                encode_line(&HostMessage::Event(EventEnvelope {
                    request_id: None,
                    topic: Topic::Index,
                    event: ServerEvent::IndexSnapshot(IndexSnapshot {
                        summary: Default::default(),
                        sessions: if present { vec![meta] } else { Vec::new() },
                        projects: Vec::new(),
                    }),
                }))
                .unwrap(),
            )
            .unwrap();
        await_restore_update(shell, cx, WorkspaceStore::index_hydrated);
    }

    fn restore_status(shell: &Entity<AppShell>, host: &MountedShell, cx: &mut VisualTestContext) {
        host.incoming
            .try_send(
                encode_line(&HostMessage::Event(EventEnvelope {
                    request_id: None,
                    topic: Topic::SessionStatus {
                        session_id: "thread-a".into(),
                    },
                    event: ServerEvent::SessionStatusReplaced(Box::new(session_status(
                        "thread-a",
                        std::path::Path::new("/project"),
                    ))),
                }))
                .unwrap(),
            )
            .unwrap();
        await_restore_update(shell, cx, |store| store.chat_active_session().is_some());
    }

    /// The rest of the baseline a restored thread waits for, after
    /// [`restore_index`] and [`restore_status`].
    fn restore_settings_and_events(
        shell: &Entity<AppShell>,
        host: &MountedShell,
        cx: &mut VisualTestContext,
    ) {
        // The host's language is applied with its settings; the assertions
        // read English.
        let settings = tcode_core::settings::Settings {
            language: Some(crate::LANGUAGE_ENGLISH.into()),
            ..Default::default()
        };
        for (topic, event) in [
            (Topic::Settings, ServerEvent::SettingsSnapshot(settings)),
            (
                Topic::SessionPlan {
                    session_id: "thread-a".into(),
                },
                ServerEvent::SessionPlanReplaced(tcode_protocol::SessionPlan {
                    session_id: "thread-a".into(),
                    steps: Vec::new(),
                }),
            ),
            (
                Topic::SessionEvents {
                    session_id: "thread-a".into(),
                },
                ServerEvent::SessionSnapshot {
                    from: 0,
                    end: 0,
                    records: Vec::new(),
                    total: 0,
                    total_turns: 0,
                    truncated: false,
                },
            ),
        ] {
            host.incoming
                .try_send(
                    encode_line(&HostMessage::Event(EventEnvelope {
                        request_id: None,
                        topic,
                        event,
                    }))
                    .unwrap(),
                )
                .unwrap();
        }
        await_restore_update(shell, cx, WorkspaceStore::baseline_ready);
    }

    /// A phone that goes back to the thread list before the thread's
    /// conversation arrives keeps the thread selected, so the conversation
    /// still loads behind the list. That is not a read; showing the thread
    /// page again is.
    #[gpui::test]
    fn a_conversation_that_loads_behind_the_thread_list_is_not_read(cx: &mut TestAppContext) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        let (shell, host, _, cx) = mount_restored(cx, &["hosts", "threads", "thread"], true);
        let reads = |host: &MountedShell| {
            sent(host)
                .into_iter()
                .filter_map(|payload| match payload {
                    ClientPayload::Command(Command::MarkSessionRead { session_id, .. }) => {
                        Some(session_id)
                    }
                    _ => None,
                })
                .collect::<Vec<_>>()
        };
        host.states
            .try_send(tcode_client::ConnectionState::Syncing { path: None })
            .unwrap();
        draw(cx);
        assert!(cx.debug_bounds("compact-thread-page").is_some());
        assert!(cx.update(|window, cx| shell.update(cx, |shell, cx| shell.back(window, cx))));
        draw(cx);

        restore_index(&shell, &host, true, cx);
        restore_status(&shell, &host, cx);
        restore_settings_and_events(&shell, &host, cx);
        assert_eq!(reads(&host), Vec::<String>::new());

        shell.update(cx, |shell, cx| shell.go(Destination::Thread, cx));
        draw(cx);
        assert_eq!(reads(&host), ["thread-a"]);
    }

    /// The phone's direct path dying is reported by the transport as a new
    /// value of the state it is in naming the relay; that alone, with no
    /// host line and no touch, changes what the nav bar says about the
    /// link. A path known while the baseline still loads is already shown
    /// as the connected path: syncing is what the skeletons say, not the
    /// subtitle.
    #[gpui::test]
    fn a_path_change_alone_updates_the_connection_subtitle(cx: &mut TestAppContext) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        let (shell, host, _, cx) = mount_restored(cx, &["hosts", "threads"], true);
        let store = store_of(&shell, cx);
        let subtitle = |cx: &mut VisualTestContext| {
            draw(cx);
            store.read_with(cx, |store, _| {
                crate::remote::connection_label(&store.connection_state())
            })
        };
        let lan = tcode_protocol::PathInfo {
            direct: true,
            relay: None,
            lan: true,
            probing_direct: false,
        };
        assert_eq!(subtitle(cx), "Connected", "the baseline is not in yet");
        host.states
            .try_send(tcode_client::ConnectionState::Syncing {
                path: Some(lan.clone()),
            })
            .unwrap();
        assert_eq!(subtitle(cx), "LAN · Connected");
        restore_index(&shell, &host, true, cx);
        restore_status(&shell, &host, cx);
        restore_settings_and_events(&shell, &host, cx);
        assert_eq!(subtitle(cx), "LAN · Connected", "the baseline is in");
        host.states
            .try_send(tcode_client::ConnectionState::Connected { path: Some(lan) })
            .unwrap();
        assert_eq!(subtitle(cx), "LAN · Connected");
        host.states
            .try_send(tcode_client::ConnectionState::Connected {
                path: Some(tcode_protocol::PathInfo {
                    direct: false,
                    relay: Some("https://aps1-1.relay.n0.iroh.link/".into()),
                    lan: false,
                    probing_direct: false,
                }),
            })
            .unwrap();
        assert_eq!(subtitle(cx), "Relay (aps1-1) · Connected");
        assert!(
            cx.debug_bounds("compact-threads-page").is_some(),
            "the Threads page is what shows it"
        );
    }

    #[gpui::test]
    fn cold_start_restores_thread_and_selects_it_once(cx: &mut TestAppContext) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        cx.update(|cx| crate::window_seam::override_mobile_for_test(cx, true));
        for width in [393., 1024.] {
            let (shell, host, _, cx) =
                mount_restored_at_width(cx, &["hosts", "threads", "thread"], true, width, "plan");
            assert!(
                cx.debug_bounds("baseline-loading").is_some(),
                "the restored destination shows a skeleton before its Index baseline"
            );
            shell.read_with(cx, |shell, cx| {
                assert_eq!(
                    shell.window_state.read(cx).history(),
                    [
                        Destination::Hosts,
                        Destination::Threads,
                        Destination::Thread
                    ]
                );
                assert_eq!(
                    shell
                        .store()
                        .unwrap()
                        .read(cx)
                        .active_session_id()
                        .as_deref(),
                    Some("thread-a")
                );
            });
            restore_index(&shell, &host, true, cx);
            restore_index(&shell, &host, true, cx);
            restore_status(&shell, &host, cx);
            draw(cx);
            cx.update(|window, cx| {
                let composer = shell
                    .read(cx)
                    .attachment
                    .as_ref()
                    .unwrap()
                    .chat
                    .read(cx)
                    .composer();
                assert!(!composer.read(cx).input_focus_handle(cx).is_focused(window));
            });
            let subscriptions = sent(&host).into_iter().filter(|payload| matches!(payload,
                ClientPayload::Subscribe(subscription) if subscription.topic == Topic::SessionEvents { session_id: "thread-a".into() }
            )).count();
            assert_eq!(
                subscriptions, 1,
                "baseline reconciliation must not select again"
            );
            for (topic, event) in [
                (
                    Topic::Settings,
                    ServerEvent::SettingsSnapshot(Default::default()),
                ),
                (
                    Topic::SessionEvents {
                        session_id: "thread-a".into(),
                    },
                    ServerEvent::SessionSnapshot {
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
                ),
            ] {
                host.incoming
                    .try_send(
                        encode_line(&HostMessage::Event(EventEnvelope {
                            request_id: None,
                            topic,
                            event,
                        }))
                        .unwrap(),
                    )
                    .unwrap();
            }
            await_restore_update(&shell, cx, |store| !store.chat_loading());
            draw(cx);
            cx.update(|window, cx| {
                window.simulate_next_frame(cx);
            });
            cx.run_until_parked();
            assert!(
                sent(&host).into_iter().any(|payload| matches!(
                    payload,
                    ClientPayload::Query(tcode_protocol::Query::SessionHistoryPage {
                        before: 1800,
                        limit: 200,
                        ..
                    })
                )),
                "restored short history must prefetch before any scroll"
            );
            shell.read_with(cx, |shell, cx| {
                assert_eq!(
                    shell.window_state.read(cx).history().last(),
                    Some(&Destination::Thread)
                )
            });
            let focus = shell.read_with(cx, |shell, cx| {
                shell
                    .attachment
                    .as_ref()
                    .unwrap()
                    .chat
                    .read(cx)
                    .composer()
                    .read(cx)
                    .input_focus_handle(cx)
            });
            for rotated_width in [width, 393., 1024.] {
                resize(cx, rotated_width);
                cx.update(|window, _| assert!(!focus.is_focused(window), "cold restore at {width}px must not open the keyboard after hydration or rotation"));
            }
        }
    }

    #[gpui::test]
    fn cold_start_discards_transient_settings_and_pair_pages(cx: &mut TestAppContext) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        for (saved, expected) in [
            (
                vec!["hosts", "threads", "thread", "settings", "settings_section"],
                vec![
                    Destination::Hosts,
                    Destination::Threads,
                    Destination::Thread,
                ],
            ),
            (
                vec!["hosts", "threads", "hosts", "pair"],
                vec![Destination::Hosts, Destination::Threads, Destination::Hosts],
            ),
        ] {
            let (shell, _, _, cx) = mount_restored(cx, &saved, true);
            shell.read_with(cx, |shell, cx| {
                assert_eq!(shell.window_state.read(cx).history(), expected);
            });
        }
    }

    #[gpui::test]
    fn cold_start_discards_a_missing_session_after_index_in_either_delivery_order(
        cx: &mut TestAppContext,
    ) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        for status_first in [false, true] {
            let (shell, host, client, cx) =
                mount_restored(cx, &["hosts", "threads", "thread", "panel"], true);
            assert!(
                cx.debug_bounds("baseline-loading").is_some(),
                "the restored Panel also waits behind a skeleton"
            );
            shell.read_with(cx, |shell, cx| {
                assert_eq!(
                    shell.window_state.read(cx).history().last(),
                    Some(&Destination::Panel)
                )
            });
            if status_first {
                restore_status(&shell, &host, cx);
            }
            restore_index(&shell, &host, false, cx);
            shell.read_with(cx, |shell, cx| {
                assert_eq!(
                    shell.window_state.read(cx).history(),
                    [Destination::Hosts, Destination::Threads]
                );
                assert_eq!(shell.store().unwrap().read(cx).active_session_id(), None);
            });
            cx.executor().advance_clock(Duration::from_millis(151));
            draw(cx);
            let preferences = client.load_preferences();
            assert_eq!(
                preferences.navigation.unwrap()["history"],
                serde_json::json!(["hosts", "threads"])
            );
        }
    }

    #[gpui::test]
    fn cold_start_restores_the_selected_panel_after_session_status(cx: &mut TestAppContext) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        cx.update(|cx| crate::window_seam::override_mobile_for_test(cx, true));
        for (saved_panel, expected_tab) in [
            ("plan", RightTab::Plan),
            ("agents", RightTab::Agents),
            ("pull_requests", RightTab::PullRequests),
        ] {
            let (shell, host, client, cx) = mount_restored_at_width(
                cx,
                &["hosts", "threads", "thread", "panel"],
                true,
                393.,
                saved_panel,
            );
            restore_index(&shell, &host, true, cx);
            restore_status(&shell, &host, cx);
            assert!(
                cx.debug_bounds("baseline-loading").is_some(),
                "status alone is not the conversation baseline"
            );
            let settings = tcode_core::settings::Settings {
                language: Some(crate::LANGUAGE_ENGLISH.into()),
                ..Default::default()
            };
            for (topic, event) in [
                (Topic::Settings, ServerEvent::SettingsSnapshot(settings)),
                (
                    Topic::SessionEvents {
                        session_id: "thread-a".into(),
                    },
                    ServerEvent::SessionSnapshot {
                        from: 0,
                        end: 0,
                        records: Vec::new(),
                        total: 0,
                        total_turns: 0,
                        truncated: false,
                    },
                ),
            ] {
                host.incoming
                    .try_send(
                        encode_line(&HostMessage::Event(EventEnvelope {
                            request_id: None,
                            topic,
                            event,
                        }))
                        .unwrap(),
                    )
                    .unwrap();
            }
            await_restore_update(&shell, cx, |store| !store.chat_loading());
            shell.read_with(cx, |shell, cx| {
                assert!(shell.pending_navigation_restore.is_none());
                assert_eq!(
                    shell.window_state.read(cx).history().last(),
                    Some(&Destination::Panel)
                );
                assert_eq!(
                    shell.store().unwrap().read(cx).panel_state().right_tab,
                    expected_tab
                );
            });
            // A preference changed during the debounce must survive the checkpoint.
            client.preferences.borrow_mut().language = Some("zh-CN".into());
            cx.executor().advance_clock(Duration::from_millis(151));
            draw(cx);
            assert_eq!(client.load_preferences().language.as_deref(), Some("zh-CN"));
            assert_eq!(
                client.load_preferences().navigation.unwrap()["panel"],
                saved_panel
            );
        }
    }

    #[gpui::test]
    fn navigation_checkpoint_debounces_selection_and_history_without_losing_preferences(
        cx: &mut TestAppContext,
    ) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        let (shell, _, client, cx) = mount_restored(cx, &["hosts", "threads", "thread"], true);
        let store = store_of(&shell, cx);
        // An OpenThread intent on the existing page must checkpoint without
        // waiting for a host response or a changed history.
        store.update(cx, |store, _| store.select_session("thread-b".into()));
        shell
            .read_with(cx, |shell, _| shell.window_state())
            .update(cx, |state, cx| state.open_thread(cx));
        draw(cx);
        assert_eq!(client.saves.get(), 0);
        cx.executor().advance_clock(Duration::from_millis(151));
        draw(cx);
        assert_eq!(client.saves.get(), 1);
        let same_page = client.load_preferences().navigation.unwrap();
        assert_eq!(same_page["session_id"], "thread-b");
        assert_eq!(
            same_page["history"],
            serde_json::json!(["hosts", "threads", "thread"])
        );

        store.update(cx, |store, cx| {
            store.select_session("thread-c".into());
            cx.notify();
        });
        shell
            .read_with(cx, |shell, _| shell.window_state())
            .update(cx, |state, cx| state.go(Destination::Settings, cx));
        draw(cx);
        assert_eq!(client.saves.get(), 1);
        cx.executor().advance_clock(Duration::from_millis(151));
        draw(cx);
        assert_eq!(client.saves.get(), 2);
        let navigation = client.load_preferences().navigation.unwrap();
        assert_eq!(navigation["session_id"], "thread-c");
        assert_eq!(
            navigation["history"],
            serde_json::json!(["hosts", "threads", "thread", "settings"])
        );
        // The store cached preferences before this checkpoint. A theme edit
        // must retain the shell's newer history and selection.
        store.update(cx, |store, _| {
            store.set_client_theme(Some(tcode_core::settings::ThemeMode::Light))
        });
        assert_eq!(client.load_preferences().navigation, Some(navigation));
        assert_eq!(
            client.load_preferences().appearance.as_deref(),
            Some("light")
        );
    }

    #[gpui::test]
    fn cold_start_with_a_missing_machine_restores_hosts(cx: &mut TestAppContext) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        let (shell, host, _, cx) = mount_restored(cx, &["hosts", "threads", "thread"], false);
        shell.read_with(cx, |shell, cx| {
            assert_eq!(shell.window_state.read(cx).history(), [Destination::Hosts]);
            assert!(shell.store().is_none());
        });
        assert!(sent(&host).is_empty());
    }

    /// A tablet in landscape with nothing to attach to is the hosts page and
    /// nothing else: the wide layout has no local host and no sidebar to
    /// switch routes with, so the page is the whole window until the window
    /// attaches somewhere, Pair returns to it, and disconnecting comes back
    /// to it.
    #[gpui::test]
    fn a_wide_mobile_window_stays_on_hosts_until_it_attaches(cx: &mut TestAppContext) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        cx.update(|cx| crate::window_seam::override_mobile_for_test(cx, true));
        let (shell, _host, client, cx) =
            mount_restored_at_width(cx, &["hosts"], false, 1024., "plan");
        let assert_locked_on_hosts = |cx: &mut VisualTestContext| {
            draw(cx);
            assert!(
                cx.debug_bounds("hosts-page").is_some(),
                "the hosts page is the window"
            );
            assert!(
                cx.debug_bounds("hosts-route").is_none(),
                "no workspace column"
            );
            assert!(
                cx.debug_bounds("sidebar-feature-hosts").is_none(),
                "no sidebar"
            );
            assert!(
                cx.debug_bounds("compact-back").is_none(),
                "nothing to go back to"
            );
        };
        shell.read_with(cx, |shell, cx| {
            assert!(!shell.compact(cx), "1024px on a mobile build is wide");
            assert!(shell.store().is_none());
            assert_eq!(shell.window_state.read(cx).history(), [Destination::Hosts]);
        });
        assert_locked_on_hosts(cx);
        let state = shell.read_with(cx, |shell, _| shell.window_state());
        // Nothing leads away: Back is the root, a thread intent has no thread,
        // and a stale destination still shows the hosts page.
        cx.update(|window, cx| {
            shell.update(cx, |shell, cx| assert!(!shell.back(window, cx)));
        });
        state.update(cx, |state, cx| state.open_thread(cx));
        assert_locked_on_hosts(cx);
        state.update(cx, |state, cx| state.go(Destination::Threads, cx));
        draw(cx);
        assert!(cx.debug_bounds("hosts-page").is_some());
        assert!(cx.debug_bounds("compact-threads-page").is_none());
        state.update(cx, |state, cx| state.leave_workspace(cx));
        // Pair is the one page over Hosts, and its Back returns to Hosts.
        state.update(cx, |state, cx| state.go(Destination::Pair, cx));
        draw(cx);
        let back = cx
            .debug_bounds("compact-back")
            .expect("Pair returns to Machines");
        cx.simulate_click(back.center(), gpui::Modifiers::default());
        assert_locked_on_hosts(cx);

        // Attaching leaves the page for the new host's workspace...
        let saved = client.saved.clone();
        cx.update(|window, cx| {
            shell.update(cx, |shell, cx| {
                shell.switch_to(AttachmentTarget::Remote(Box::new(saved)), window, cx)
            });
        });
        draw(cx);
        shell.read_with(cx, |shell, cx| {
            assert!(shell.store().is_some());
            assert_eq!(shell.window_state.read(cx).route(), Route::Chat);
        });
        assert!(cx.debug_bounds("hosts-page").is_none());
        assert!(
            cx.debug_bounds("sidebar-feature-hosts").is_some(),
            "the wide workspace"
        );

        // ...and disconnecting returns to it.
        shell.update(cx, |shell, cx| shell.detach(cx));
        shell.read_with(cx, |shell, cx| {
            assert!(shell.store().is_none());
            assert_eq!(shell.window_state.read(cx).history(), [Destination::Hosts]);
        });
        assert_locked_on_hosts(cx);
    }

    /// Navigation never raises a software keyboard: on a phone every route
    /// leaves the composer unfocused, and a desktop window opening a thread
    /// focuses it.
    #[gpui::test]
    fn navigation_dismisses_mobile_keyboards_but_preserves_explicit_editing_focus(
        cx: &mut TestAppContext,
    ) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        let (shell, _host, cx) = mount(cx);
        as_mobile(cx);
        resize(cx, 393.);
        let store = store_of(&shell, cx);
        let composer = shell.read_with(cx, |shell, cx| {
            shell.attachment.as_ref().unwrap().chat.read(cx).composer()
        });
        let focus = composer.read_with(cx, |composer, cx| composer.input_focus_handle(cx));
        for width in [1024., 393., 1024.] {
            resize(cx, width);
            for id in ["thread-1", "thread-2"] {
                cx.update(|window, cx| {
                    if id == "thread-2" {
                        focus.focus(window, cx);
                    }
                    store.update(cx, |store, _| store.select_session(id.into()));
                    let state = shell.read(cx).window_state();
                    state.update(cx, |_, cx| cx.emit(OpenThread));
                });
                draw(cx);
                cx.update(|window, _| assert!(!focus.is_focused(window)));
            }
            if shell.read_with(cx, |shell, cx| shell.compact(cx)) {
                cx.update(|window, cx| {
                    focus.focus(window, cx);
                    shell.update(cx, |shell, cx| assert!(shell.back(window, cx)));
                });
            }
            draw(cx);
            cx.update(|window, cx| {
                assert!(!focus.is_focused(window));
                shell.update(cx, |shell, cx| shell.open_thread(window, cx));
            });
            draw(cx);
            cx.update(|window, _| assert!(!focus.is_focused(window)));
            for destination in [
                Destination::Panel,
                Destination::Hosts,
                Destination::Settings,
            ] {
                cx.update(|window, cx| {
                    focus.focus(window, cx);
                    shell.update(cx, |shell, cx| shell.go(destination, cx));
                });
                draw(cx);
                cx.update(|window, cx| {
                    assert!(!focus.is_focused(window));
                    shell.update(cx, |shell, cx| shell.open_thread(window, cx));
                });
                draw(cx);
                cx.update(|window, _| assert!(!focus.is_focused(window)));
            }
        }
        cx.update(|window, cx| focus.focus(window, cx));
        for width in [393., 1024., 393.] {
            resize(cx, width);
            draw(cx);
            cx.update(|window, _| assert!(focus.is_focused(window)));
        }
        cx.update(|_, cx| crate::window_seam::override_mobile_for_test(cx, false));
        resize(cx, 1024.);
        cx.update(|window, cx| shell.update(cx, |shell, cx| shell.open_thread(window, cx)));
        draw(cx);
        cx.update(|window, _| assert!(focus.is_focused(window)));
    }

    #[gpui::test]
    fn background_provider_notices_preserve_composer_editing(cx: &mut TestAppContext) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        let root = crate::store::tests::scratch_root("background-notification-focus");
        let disk = tcode_services::store::SessionStore::open_at(root.clone()).unwrap();
        for id in ["thread-a", "thread-b"] {
            let mut meta = tcode_core::project::SessionMeta::new(
                agent::ProviderKind::Codex,
                root.clone(),
                None,
            );
            meta.id = id.into();
            disk.upsert_meta(&meta).unwrap();
        }
        let host =
            tcode_runtime::pipe::spawn_host(disk, tcode_runtime::pipe::HostServices::default())
                .unwrap();
        let mut driver = crate::store::tests::LocalHostDriver::new(&host);
        let (shell, _transport, cx) = mount(cx);
        as_mobile(cx);
        resize(cx, 393.);
        let store = store_of(&shell, cx);
        store.update(cx, |store, cx| {
            *store = WorkspaceStore::new_attached(
                driver.link.clone(),
                crate::store::WorkspaceAttachment::Local,
                None,
                None,
                false,
                cx,
            );
            store.select_session("thread-a".into());
        });
        driver.wait_until(cx, &store, "selected conversation", |cx| {
            store.read_with(cx, |store, _| {
                store.baseline_ready() && !store.threads_loading() && !store.chat_loading()
            })
        });
        cx.update(|window, cx| shell.update(cx, |shell, cx| shell.open_thread(window, cx)));
        draw(cx);
        let composer = shell.read_with(cx, |shell, cx| {
            shell.attachment.as_ref().unwrap().chat.read(cx).composer()
        });
        let focus = composer.read_with(cx, |composer, cx| composer.input_focus_handle(cx));
        let card = cx.debug_bounds("composer-card").unwrap();
        cx.simulate_click(card.center(), gpui::Modifiers::default());
        cx.simulate_keystrokes("draft");
        cx.simulate_keystrokes("left");
        draw(cx);
        cx.update(|window, _| assert!(focus.is_focused(window)));
        assert_eq!(
            composer.read_with(cx, |composer, cx| composer.draft(cx)),
            "draft"
        );

        for event in [
            agent::AgentEvent::TurnRetry {
                retry: Some(agent::TurnRetry {
                    turn_id: "background-turn".into(),
                    retry_at: Some(u64::MAX),
                }),
            },
            agent::AgentEvent::Warning {
                message: "Background status changed".into(),
            },
            agent::AgentEvent::Error {
                message: "Reconnecting…".into(),
                fatal: false,
            },
        ] {
            driver
                .await_with_events(
                    cx,
                    &store,
                    host.update_state_for_test(move |state, cx| {
                        state.provider_event_for_test("thread-b", event, cx);
                    }),
                )
                .unwrap();
            driver.step(cx, &store);
            draw(cx);
            cx.update(|window, _| {
                assert!(
                    focus.is_focused(window),
                    "background provider event stole composer focus"
                );
            });
            assert_eq!(
                composer.read_with(cx, |composer, cx| composer.draft(cx)),
                "draft"
            );
        }
        for width in [1024., 393.] {
            resize(cx, width);
            cx.update(|window, _| {
                assert!(focus.is_focused(window), "rotation keeps editing focus")
            });
        }
        let toast = cx
            .debug_bounds("compact-toast")
            .expect("the notice remains visible");
        cx.simulate_click(toast.center(), gpui::Modifiers::default());
        draw(cx);
        cx.update(|window, _| {
            assert!(
                !focus.is_focused(window),
                "tapping the error opens its details"
            )
        });
        cx.update(|window, cx| window.close_dialog(cx));
        draw(cx);
        cx.update(|window, _| {
            assert!(
                focus.is_focused(window),
                "closing details returns to editing"
            )
        });
        cx.simulate_keystrokes("x");
        assert_eq!(
            composer.read_with(cx, |composer, cx| composer.draft(cx)),
            "drafxt"
        );
        driver.shutdown(cx, &store, &host);
        let _ = std::fs::remove_dir_all(root);
    }

    #[gpui::test]
    fn keyboard_cover_changes_keep_editor_focus_until_explicit_dismissal(cx: &mut TestAppContext) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        cx.update(crate::theme::init);
        cx.update(|cx| crate::window_seam::override_mobile_for_test(cx, true));
        let (shell, host, _, cx) =
            mount_restored_at_width(cx, &["hosts", "threads", "thread"], true, 393., "plan");
        restore_index(&shell, &host, true, cx);
        restore_status(&shell, &host, cx);
        for (topic, event) in [
            (
                Topic::Settings,
                ServerEvent::SettingsSnapshot(Default::default()),
            ),
            (
                Topic::SessionEvents {
                    session_id: "thread-a".into(),
                },
                ServerEvent::SessionSnapshot {
                    total: 0,
                    total_turns: 0,
                    truncated: false,
                    from: 0,
                    end: 0,
                    records: vec![],
                },
            ),
        ] {
            host.incoming
                .try_send(
                    encode_line(&HostMessage::Event(EventEnvelope {
                        request_id: None,
                        topic,
                        event,
                    }))
                    .unwrap(),
                )
                .unwrap();
        }
        await_restore_update(&shell, cx, |store| !store.chat_loading());
        cx.executor().advance_clock(Duration::from_millis(250));
        draw(cx);
        let composer = shell.read_with(cx, |shell, cx| {
            shell.attachment.as_ref().unwrap().chat.read(cx).composer()
        });
        let focus = composer.read_with(cx, |composer, cx| composer.input_focus_handle(cx));
        cx.update(|window, cx| focus.focus(window, cx));
        draw(cx);
        crate::window_seam::occlude_for_test(
            cx,
            gpui::Edges {
                bottom: px(400.),
                ..Default::default()
            },
        );
        draw(cx);
        crate::window_seam::occlude_for_test(cx, gpui::Edges::default());
        draw(cx);
        cx.update(|window, _| {
            assert!(
                focus.is_focused(window),
                "hardware input hides the software keyboard without ending editing"
            )
        });
        cx.simulate_input("keyboard draft");
        assert_eq!(
            composer.read_with(cx, |composer, cx| composer.draft(cx)),
            "keyboard draft"
        );
        let card = cx
            .debug_bounds("composer-card")
            .expect("composer on the thread page");
        cx.simulate_click(
            gpui::point(card.center().x, card.top() - px(40.)),
            gpui::Modifiers::default(),
        );
        draw(cx);
        cx.update(|window, _| {
            assert!(
                !focus.is_focused(window),
                "tap blank timeline dismisses editing"
            )
        });
        cx.simulate_click(card.center(), gpui::Modifiers::default());
        draw(cx);
        cx.update(|window, _| assert!(focus.is_focused(window), "tap composer resumes editing"));
    }

    /// A tablet that rotates through phone widths and back: the read-only
    /// child composer stays visible and inert at every width, and the parent
    /// draft is intact when its thread is reopened.
    #[gpui::test]
    fn native_subagent_composer_is_visible_inert_and_restores_parent_draft(
        cx: &mut TestAppContext,
    ) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        cx.update(crate::theme::init);
        cx.update(|cx| crate::window_seam::override_mobile_for_test(cx, true));
        let (shell, host, _, cx) =
            mount_restored_at_width(cx, &["hosts", "threads", "thread"], true, 1200., "plan");
        restore_index(&shell, &host, true, cx);
        restore_status(&shell, &host, cx);
        let store = store_of(&shell, cx);
        let composer = shell.read_with(cx, |shell, cx| {
            shell.attachment.as_ref().unwrap().chat.read(cx).composer()
        });
        let deliver = |session: &str, readonly: bool, cx: &mut VisualTestContext| {
            let mut meta = tcode_core::project::SessionMeta::new(
                agent::ProviderKind::Codex,
                "/project".into(),
                Some("child-model".into()),
            );
            meta.id = session.into();
            meta.native_subagent = readonly.then(|| "spawn-1".into());
            let mut status = session_status(session, std::path::Path::new("/project"));
            status.requested_model = meta.model.clone();
            status.activity.turn_running = readonly;
            status.conversation_read_only = readonly;
            status.provider_option_selections = vec![agent::OptionSelection {
                id: "reasoningEffort".into(),
                value: serde_json::json!("high"),
            }];
            store.update(cx, |store, _| store.select_session(session.into()));
            for (topic, event) in [
                (
                    Topic::Settings,
                    ServerEvent::SettingsSnapshot(Default::default()),
                ),
                (Topic::Index, ServerEvent::IndexUpsertSession(meta)),
                (
                    Topic::SessionStatus {
                        session_id: session.into(),
                    },
                    ServerEvent::SessionStatusReplaced(Box::new(status)),
                ),
                (
                    Topic::SessionPlan {
                        session_id: session.into(),
                    },
                    ServerEvent::SessionPlanReplaced(tcode_protocol::SessionPlan {
                        session_id: session.into(),
                        steps: Vec::new(),
                    }),
                ),
                (
                    Topic::SessionEvents {
                        session_id: session.into(),
                    },
                    ServerEvent::SessionSnapshot {
                        total: 0,
                        total_turns: 0,
                        truncated: false,
                        from: 0,
                        end: 0,
                        records: vec![],
                    },
                ),
            ] {
                host.incoming
                    .try_send(
                        encode_line(&HostMessage::Event(EventEnvelope {
                            request_id: None,
                            topic,
                            event,
                        }))
                        .unwrap(),
                    )
                    .unwrap();
            }
            await_restore_update(&shell, cx, |store| !store.chat_loading());
        };
        deliver("thread-a", false, cx);
        cx.update(|window, cx| {
            composer.update(cx, |composer, cx| {
                composer.set_draft("parent draft", window, cx);
            })
        });
        draw(cx);
        deliver("child", true, cx);
        for width in [1200., 760., 393.] {
            resize(cx, width);
            shell.update(cx, |shell, cx| shell.go(Destination::Thread, cx));
            cx.executor().advance_clock(Duration::from_millis(250));
            draw(cx);
            sent(&host);
            assert!(cx.debug_bounds("composer-card").is_some());
            for selector in [
                "model-picker",
                "traits-chip",
                "permission-chip",
                "send-message",
            ] {
                let bounds = cx.debug_bounds(selector).expect(selector);
                assert!(bounds.size.width > px(0.) && bounds.size.height > px(0.));
                cx.simulate_click(bounds.center(), gpui::Modifiers::default());
                draw(cx);
            }
            let card = cx.debug_bounds("composer-card").unwrap();
            cx.simulate_click(
                card.origin + gpui::point(px(20.), px(20.)),
                gpui::Modifiers::default(),
            );
            cx.simulate_keystrokes("x shift-tab enter");
            draw(cx);
            assert_eq!(
                composer.read_with(cx, |composer, cx| composer.draft(cx)),
                ""
            );
            assert!(
                sent(&host)
                    .iter()
                    .all(|payload| !matches!(payload, ClientPayload::Command(_))),
                "disabled composer must not send commands"
            );
        }
        deliver("thread-a", false, cx);
        resize(cx, 1200.);
        assert_eq!(
            composer.read_with(cx, |composer, cx| composer.draft(cx)),
            "parent draft"
        );
        // A software keyboard waits for an explicit tap on the composer.
        let card = cx.debug_bounds("composer-card").expect("parent composer");
        cx.simulate_click(
            gpui::point(card.center().x, card.top() + px(24.)),
            gpui::Modifiers::default(),
        );
        draw(cx);
        cx.simulate_keystrokes("end x");
        draw(cx);
        assert_eq!(
            composer.read_with(cx, |composer, cx| composer.draft(cx)),
            "parent draftx"
        );
    }

    #[gpui::test]
    fn software_keyboard_restored_terminal_does_not_raise_keyboard(cx: &mut TestAppContext) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        cx.update(|cx| crate::window_seam::override_mobile_for_test(cx, true));
        let (shell, host, _, cx) = mount_restored_at_width(
            cx,
            &["hosts", "threads", "thread", "panel"],
            true,
            1024.,
            "terminal",
        );
        restore_index(&shell, &host, true, cx);
        let mut status = session_status("thread-a", std::path::Path::new("/project"));
        status.terminal_open = true;
        host.incoming
            .try_send(
                encode_line(&HostMessage::Event(EventEnvelope {
                    request_id: None,
                    topic: Topic::SessionStatus {
                        session_id: "thread-a".into(),
                    },
                    event: ServerEvent::SessionStatusReplaced(Box::new(status)),
                }))
                .unwrap(),
            )
            .unwrap();
        await_restore_update(&shell, cx, |store| store.chat_active_session().is_some());
        host.incoming
            .try_send(
                encode_line(&HostMessage::Event(EventEnvelope {
                    request_id: None,
                    topic: Topic::Settings,
                    event: ServerEvent::SettingsSnapshot(Default::default()),
                }))
                .unwrap(),
            )
            .unwrap();
        host.incoming
            .try_send(
                encode_line(&HostMessage::Event(EventEnvelope {
                    request_id: None,
                    topic: Topic::SessionEvents {
                        session_id: "thread-a".into(),
                    },
                    event: ServerEvent::SessionSnapshot {
                        total: 0,
                        total_turns: 0,
                        truncated: false,
                        from: 0,
                        end: 0,
                        records: vec![],
                    },
                }))
                .unwrap(),
            )
            .unwrap();
        await_restore_update(&shell, cx, |store| !store.chat_loading());
        assert!(store_of(&shell, cx).read_with(cx, |store, _| store.panel_state().terminal_open));
        cx.executor().advance_clock(Duration::from_secs(1));
        draw(cx);
        cx.update(|window, cx| {
            assert!(
                window.focused(cx).is_none(),
                "a restored terminal must not steal focus after navigation has blurred"
            )
        });
    }

    #[gpui::test]
    fn project_choice_reuses_drafts_and_focuses_only_on_desktop(cx: &mut TestAppContext) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        for (mobile, compact, projects) in [
            (false, false, 2),
            (true, false, 2),
            (true, true, 2),
            (true, true, 1),
        ] {
            cx.update(|cx| crate::window_seam::override_mobile_for_test(cx, mobile));
            new_thread_from_projects(cx, projects, compact);
        }
    }

    fn new_thread_from_projects(cx: &mut TestAppContext, project_count: usize, compact: bool) {
        use tcode_core::project::Project;
        use tcode_runtime::pipe::{HostServices, spawn_host};
        use tcode_services::store::SessionStore;

        cx.update(crate::theme::init);
        let root =
            crate::store::tests::scratch_root(&format!("new-thread-{project_count}-{compact}"));
        let disk = SessionStore::open_at(root.clone()).unwrap();
        let projects: Vec<_> = (0..project_count)
            .map(|index| Project {
                id: format!("project-{index}"),
                name: format!("Project {index}"),
                root: root.join(format!("project-{index}")),
                quick_chat: false,
                icon_path: None,
                permission_defaults: Default::default(),
                created_at: index as u64,
            })
            .collect();
        let expected = projects.last().unwrap().clone();
        let host = spawn_host(disk, HostServices::default()).unwrap();
        smol::block_on(host.update_state_for_test(move |state, _| {
            state.projects = projects;
            state.settings.language = Some("en".into());
        }))
        .unwrap();
        let mut driver = crate::store::tests::LocalHostDriver::new(&host);
        let (shell, _transport, cx) = mount(cx);
        // Establish the startup viewport before the baseline can select a draft.
        // Shrinking an already populated wide layout intentionally opens its thread.
        if compact {
            as_mobile(cx);
        }
        cx.simulate_resize(size(px(if compact { 393. } else { 1024. }), px(852.)));
        cx.executor()
            .advance_clock(std::time::Duration::from_millis(250));
        draw(cx);
        let store = store_of(&shell, cx);
        // Keep the shell's transport deterministic; the real host link owns
        // its pump, as in the store tests, and snapshots are drained below.
        store.update(cx, |store, cx| {
            *store = WorkspaceStore::new_attached(
                driver.link.clone(),
                crate::store::WorkspaceAttachment::Local,
                None,
                None,
                false,
                cx,
            )
        });
        driver.wait_until(cx, &store, "project baseline", |cx| {
            store.read_with(cx, |store, _| store.baseline_ready())
        });
        draw(cx);
        let mut selected_draft = None;
        for reopen in [false, true] {
            if reopen && compact {
                cx.update(|window, cx| {
                    shell.update(cx, |shell, cx| assert!(shell.back(window, cx)));
                });
                cx.executor()
                    .advance_clock(std::time::Duration::from_millis(250));
                draw(cx);
            }
            if compact {
                let plus = cx
                    .debug_bounds("compact-new-thread")
                    .expect("Threads new-thread button");
                cx.simulate_click(plus.center(), gpui::Modifiers::default());
            } else {
                shell
                    .read_with(cx, |shell, _| shell.window_state())
                    .update(cx, |state, cx| {
                        state.open_palette(cx);
                    });
            }
            draw(cx);
            if project_count > 1 || !compact {
                assert!(shell.read_with(cx, |shell, cx| shell.window_state.read(cx).palette_open));
                cx.update(|window, cx| {
                    let palette = &shell.read(cx).attachment.as_ref().unwrap().palette;
                    let query = palette.read(cx).query_focus_handle(cx);
                    // The chooser is tapped through on a phone; a desktop
                    // window (even a narrow one) types into it.
                    assert_eq!(
                        query.is_focused(window),
                        !(compact && crate::window_seam::is_mobile(cx)),
                        "project chooser raises the keyboard only off-phone"
                    );
                    assert!(
                        palette
                            .read(cx)
                            .focus_handle(cx)
                            .contains_focused(window, cx),
                        "the palette owns focus so Escape closes it"
                    );
                });
                let row = cx
                    .debug_bounds("palette-new-thread-project-1")
                    .expect("project choice");
                cx.simulate_click(row.center(), gpui::Modifiers::default());
                draw(cx);
            }
            shell.read_with(cx, |shell, cx| {
                let state = shell.window_state.read(cx);
                assert!(!state.palette_open, "choosing a project closes the chooser");
                if compact {
                    assert_eq!(
                        state.history().last(),
                        Some(&Destination::Thread),
                        "starting a draft navigates independently of session selection"
                    );
                } else {
                    assert_eq!(state.route(), Route::Chat);
                }
            });
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            loop {
                driver.step(cx, &store);
                draw(cx);
                if store.read_with(cx, |store, _| {
                    !store.chat_loading()
                        && store
                            .chat_active_session()
                            .is_some_and(|(_, cwd, draft)| cwd == expected.root && draft)
                }) {
                    break;
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "selected project's draft and event baseline reach chat"
                );
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            let focus = shell.read_with(cx, |shell, cx| {
                shell
                    .attachment
                    .as_ref()
                    .unwrap()
                    .chat
                    .read(cx)
                    .composer()
                    .read(cx)
                    .input_focus_handle(cx)
            });
            cx.update(|window, cx| {
                assert_eq!(
                    focus.is_focused(window),
                    !compact && !crate::window_seam::is_mobile(cx)
                )
            });
            if cx.update(|_, cx| crate::window_seam::is_mobile(cx)) {
                // Let the compact push finish sliding before tapping into it.
                cx.executor().advance_clock(Duration::from_millis(250));
                draw(cx);
                let card = cx.debug_bounds("composer-card").expect("draft composer");
                cx.simulate_click(
                    gpui::point(card.center().x, card.top() + px(24.)),
                    gpui::Modifiers::default(),
                );
                draw(cx);
                cx.update(|window, _| {
                    assert!(focus.is_focused(window), "explicit composer tap focuses")
                });
                // Rotate away and back to the width the scenario started at.
                let rotation = if compact {
                    [1024., 393.]
                } else {
                    [393., 1024.]
                };
                for width in rotation {
                    resize(cx, width);
                    cx.update(|window, _| {
                        assert!(focus.is_focused(window), "rotation preserves editing focus")
                    });
                }
            }
            let selected = store.read_with(cx, |store, _| store.active_session_id());
            assert!(selected.is_some());
            if reopen {
                assert_eq!(
                    selected, selected_draft,
                    "reopening reuses the selected draft"
                );
            }
            selected_draft = selected;
        }
        driver.shutdown(cx, &store, &host);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[gpui::test]
    fn header_controls_activate_after_pointer_jitter_without_dragging(cx: &mut TestAppContext) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        let (shell, host, cx) = mount(cx);
        for (topic, event) in [
            (
                Topic::Index,
                ServerEvent::IndexSnapshot(IndexSnapshot {
                    summary: Default::default(),
                    sessions: Vec::new(),
                    projects: Vec::new(),
                }),
            ),
            (
                Topic::Settings,
                ServerEvent::SettingsSnapshot(Default::default()),
            ),
        ] {
            host.incoming
                .try_send(
                    encode_line(&HostMessage::Event(EventEnvelope {
                        request_id: None,
                        topic,
                        event,
                    }))
                    .unwrap(),
                )
                .unwrap();
        }
        await_restore_update(&shell, cx, |store| !store.chat_loading());
        let state = shell.read_with(cx, |shell, _| shell.window_state());
        for (width, selector) in [(1024., "toggle-sidebar"), (393., "compact-search")] {
            if width < 900. {
                as_mobile(cx);
            }
            resize(cx, width);
            cx.executor().advance_clock(Duration::from_millis(250));
            draw(cx);
            let start = cx
                .debug_bounds(selector)
                .unwrap_or_else(|| panic!("missing {selector}"))
                .center();
            let end = start + gpui::point(px(1.), px(1.));
            cx.simulate_mouse_down(start, MouseButton::Left, gpui::Modifiers::default());
            // TestWindow rejects native window moves, so a leaked press fails here.
            cx.simulate_mouse_move(end, Some(MouseButton::Left), gpui::Modifiers::default());
            cx.simulate_mouse_up(end, MouseButton::Left, gpui::Modifiers::default());
            state.read_with(cx, |state, _| {
                if width >= 900. {
                    assert!(state.sidebar_collapsed);
                } else {
                    assert!(state.palette_open);
                }
            });
        }
    }

    fn mount(cx: &mut TestAppContext) -> (Entity<AppShell>, MountedShell, &mut VisualTestContext) {
        mount_initial(cx, Some(AttachmentTarget::Local))
    }

    fn mount_initial(
        cx: &mut TestAppContext,
        initial: Option<AttachmentTarget>,
    ) -> (Entity<AppShell>, MountedShell, &mut VisualTestContext) {
        // As `run_shell` does before opening the window: the shell alone,
        // without the window root, initializes nothing of gpui-base itself.
        cx.update(crate::theme::init);
        let (to_host, outgoing) = async_channel::unbounded();
        let (incoming, from_host) = async_channel::unbounded();
        let (states, state) = async_channel::unbounded();
        let transport = RefCell::new(Some(Transport {
            originals: None,
            to_host: to_host.into(),
            from_host,
            state,
            current_host: None,
        }));
        let mounted = Rc::new(RefCell::new(None));
        let capture = mounted.clone();
        let (_, cx) = cx.add_window_view(move |window, cx| {
            let window_state = cx.new(|_| WindowState::new(false));
            let shell = cx.new(|cx| {
                AppShell::new(
                    window_state,
                    ShellSetup {
                        local: Some(Rc::new(move |_| {
                            Ok(transport.borrow_mut().take().expect("one attachment"))
                        })),
                        initial,
                        ..Default::default()
                    },
                    window,
                    cx,
                )
            });
            *capture.borrow_mut() = Some(shell.clone());
            gpui_base::Root::new(shell, window, cx)
        });
        let shell = mounted.borrow_mut().take().unwrap();
        if let Some(store) = shell.read_with(cx, |shell, _| shell.store()) {
            crate::store::tests::seed_full_scope(&store, &incoming, Vec::new(), cx);
        }
        (
            shell,
            MountedShell {
                outgoing,
                observed: RefCell::new(Vec::new()),
                incoming,
                states,
            },
            cx,
        )
    }

    #[gpui::test]
    fn connecting_from_the_hosts_panel_releases_its_entity_before_attachment(
        cx: &mut TestAppContext,
    ) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        let (shell, _host, cx) = mount_initial(cx, None);
        as_mobile(cx);
        resize(cx, 393.);
        shell
            .read_with(cx, |shell, _| shell.window_state())
            .update(cx, |state, cx| {
                state.go(Destination::Thread, cx);
                state.go(Destination::Panel, cx);
            });
        let hosts = shell.read_with(cx, |shell, _| shell.hosts.clone());
        cx.update(|window, cx| {
            set_back_target(window.window_handle(), &shell, cx);
            // Connect runs inside RemotePanel's click listener. Attaching
            // updates that same panel to point at the new workspace store.
            hosts.update(cx, |_, cx| {
                switch_current(AttachmentTarget::Local, window, cx);
            });
        });
        draw(cx);
        shell.read_with(cx, |shell, cx| {
            assert!(shell.store().is_some());
            assert_eq!(
                shell.window_state.read(cx).history(),
                [Destination::Hosts, Destination::Threads]
            );
        });
        assert!(cx.update(|window, cx| shell.update(cx, |shell, cx| shell.back(window, cx))));
        draw(cx);
        assert_eq!(
            shell.read_with(cx, |shell, cx| shell.destination(cx)),
            Destination::Hosts
        );
        assert!(!cx.update(|window, cx| shell.update(cx, |shell, cx| shell.back(window, cx))));
    }

    /// A local host that did not start at launch is on Machines from the
    /// first frame, whatever the window attached to. Retry asks bootstrap to
    /// start it again; a failed start keeps the machine the window is on, and
    /// one that comes up replaces it.
    #[gpui::test]
    fn a_local_host_that_does_not_start_is_shown_and_retried(cx: &mut TestAppContext) {
        struct Desk {
            saved: tcode_client::pairing::PairedHost,
            transports: RefCell<Vec<Transport>>,
        }
        impl ClientHost for Desk {
            fn device_name(&self) -> String {
                "desk".into()
            }
            fn device_id(&self) -> String {
                "desk".into()
            }
            fn device_platform(&self) -> Option<String> {
                None
            }
            fn load_hosts(&self) -> Vec<tcode_client::pairing::PairedHost> {
                vec![self.saved.clone()]
            }
            fn save_hosts(&self, _: &[tcode_client::pairing::PairedHost]) {}
            fn last_host_id(&self) -> Option<String> {
                None
            }
            fn set_last_host_id(&self, _: Option<&str>) {}
            fn pair(
                &self,
                _: tcode_client::pairing::PairInvite,
                _: Option<std::net::IpAddr>,
            ) -> tcode_client::host::HostFuture<'_, Result<tcode_client::pairing::PairedHost, String>>
            {
                unreachable!("nothing is paired here")
            }
            fn connect(&self, _: &tcode_client::pairing::PairedHost) -> Transport {
                self.transports.borrow_mut().pop().expect("one connection")
            }
        }
        fn transport() -> (Transport, async_channel::Sender<String>) {
            let (to_host, _) = async_channel::unbounded();
            let (incoming, from_host) = async_channel::unbounded();
            let (_, state) = async_channel::unbounded();
            let transport = Transport {
                originals: None,
                to_host: to_host.into(),
                from_host,
                state,
                current_host: None,
            };
            (transport, incoming)
        }

        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        cx.update(crate::theme::init);
        let busy = "another Tcode host is already using the data directory /tmp/demo";
        let (local_transport, local_incoming) = transport();
        // Answered from the back: busy on the first Retry, up on the second.
        let answers = RefCell::new(vec![Ok(local_transport), Err(busy.to_owned())]);
        let attempts = Rc::new(Cell::new(0));
        let local: LocalTransport = {
            let attempts = attempts.clone();
            Rc::new(move |_| {
                attempts.set(attempts.get() + 1);
                answers.borrow_mut().pop().expect("an unexpected start")
            })
        };
        let saved = tcode_client::pairing::PairedHost {
            host_id: "studio".into(),
            name: "Studio".into(),
            traverse: Vec::new(),
            relay: None,
            addrs: vec!["127.0.0.1:47503".into()],
            last_connected_unix: None,
            group_id: None,
            space_id: None,
            space_name: None,
        };
        let (remote_transport, _remote_incoming) = transport();
        let desk: Rc<dyn ClientHost> = Rc::new(Desk {
            saved: saved.clone(),
            transports: RefCell::new(vec![remote_transport]),
        });
        cx.update(|cx| {
            cx.set_global(crate::remote::ClientAttachment::new(
                desk.clone(),
                true,
                switch_current,
            ))
        });
        let mounted = Rc::new(RefCell::new(None));
        let capture = mounted.clone();
        let (_, cx) = cx.add_window_view(move |window, cx| {
            let window_state = cx.new(|_| WindowState::new(false));
            let shell = cx.new(|cx| {
                AppShell::new(
                    window_state,
                    ShellSetup {
                        client_host: Some(desk),
                        local: Some(local),
                        local_failure: Some(busy.to_owned()),
                        initial: Some(AttachmentTarget::Remote(Box::new(saved))),
                        ..Default::default()
                    },
                    window,
                    cx,
                )
            });
            *capture.borrow_mut() = Some(shell.clone());
            gpui_base::Root::new(shell, window, cx)
        });
        let shell: Entity<AppShell> = mounted.borrow_mut().take().unwrap();
        cx.update(|window, cx| set_back_target(window.window_handle(), &shell, cx));
        resize(cx, 1024.);
        let attached = |cx: &mut VisualTestContext| {
            shell.read_with(cx, |shell, cx| {
                shell
                    .store()
                    .map(|store| store.read(cx).remote_host_id().map(str::to_owned))
            })
        };
        let open_machines = |cx: &mut VisualTestContext| {
            shell
                .read_with(cx, |shell, _| shell.window_state())
                .update(cx, |state, cx| state.go(Destination::Hosts, cx));
            draw(cx);
        };
        let retry = |cx: &mut VisualTestContext| {
            let button = cx.debug_bounds("hosts-local-retry").expect("Retry");
            cx.simulate_click(button.center(), gpui::Modifiers::default());
            draw(cx);
        };

        open_machines(cx);
        assert_eq!(attempts.get(), 0, "the launch's failure is not asked again");
        assert_eq!(attached(cx), Some(Some("studio".into())));
        assert!(cx.debug_bounds("hosts-local-failure").is_some());

        retry(cx);
        assert_eq!(attempts.get(), 1, "Retry starts the local host again");
        assert_eq!(
            attached(cx),
            Some(Some("studio".into())),
            "a failed start keeps the machine the window is on"
        );
        open_machines(cx);
        assert!(cx.debug_bounds("hosts-local-failure").is_some());

        retry(cx);
        assert_eq!(attempts.get(), 2);
        assert_eq!(attached(cx), Some(None), "the local host replaces it");
        let store = shell.read_with(cx, |shell, _| shell.store()).unwrap();
        crate::store::tests::seed_full_scope(&store, &local_incoming, Vec::new(), cx);
        open_machines(cx);
        assert!(cx.debug_bounds("hosts-local-failure").is_none());
    }

    #[gpui::test]
    fn compact_palette_is_reachable_from_threads_and_thread(cx: &mut TestAppContext) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        let (shell, _host, cx) = mount(cx);
        as_mobile(cx);
        cx.simulate_resize(size(px(393.), px(852.)));
        draw(cx);
        cx.executor().advance_clock(Duration::from_millis(250));
        draw(cx);
        let state = shell.read_with(cx, |shell, _| shell.window_state());
        for selector in ["compact-search", "compact-open-palette"] {
            let button = cx.debug_bounds(selector).expect("compact palette entry");
            cx.simulate_click(button.center(), gpui::Modifiers::default());
            draw(cx);
            assert!(state.read_with(cx, |state, _| state.palette_open));
            let card = cx.debug_bounds("palette-card").expect("palette sheet");
            assert_eq!(card.left(), px(0.));
            assert_eq!(card.right(), px(393.));
            assert_eq!(card.bottom(), px(852.));
            let position = gpui::point(px(30.), px(26.));
            let history = state.read_with(cx, |state, _| state.history().to_vec());
            cx.simulate_event(gpui::MouseDownEvent {
                button: gpui::MouseButton::Left,
                position,
                click_count: 1,
                ..Default::default()
            });
            draw(cx);
            cx.simulate_event(gpui::MouseUpEvent {
                button: gpui::MouseButton::Left,
                position,
                click_count: 1,
                ..Default::default()
            });
            assert!(!state.read_with(cx, |state, _| state.palette_open));
            assert_eq!(
                state.read_with(cx, |state, _| state.history().to_vec()),
                history
            );
            cx.simulate_click(button.center(), gpui::Modifiers::default());
            draw(cx);
            assert!(state.read_with(cx, |state, _| state.palette_open));
            cx.update(|window, cx| {
                let palette = &shell.read(cx).attachment.as_ref().unwrap().palette;
                assert!(
                    palette
                        .read(cx)
                        .focus_handle(cx)
                        .contains_focused(window, cx)
                );
            });
            crate::window_seam::occlude_for_test(
                cx,
                gpui::Edges {
                    top: px(47.),
                    bottom: px(400.),
                    ..Default::default()
                },
            );
            draw(cx);
            let card = cx.debug_bounds("palette-card").unwrap();
            assert_eq!(card.top(), px(99.));
            assert_eq!(card.bottom(), px(452.));
            assert_eq!(card.size.width, px(393.));
            let list = cx.debug_bounds("palette-list").unwrap();
            assert!(list.size.height > px(0.) && list.bottom() <= card.bottom());
            cx.update(|window, cx| {
                shell.update(cx, |shell, cx| assert!(shell.back(window, cx)));
                assert!(
                    state.read(cx).palette_open,
                    "Back dismisses the keyboard first"
                );
            });
            crate::window_seam::occlude_for_test(cx, gpui::Edges::default());
            cx.update(|window, cx| {
                shell.update(cx, |shell, cx| assert!(shell.back(window, cx)));
            });
            assert!(!state.read_with(cx, |state, _| state.palette_open));
            state.update(cx, |state, cx| state.open_thread(cx));
            cx.executor().advance_clock(Duration::from_millis(250));
            draw(cx);
        }
    }

    fn draw(cx: &mut VisualTestContext) {
        cx.run_until_parked();
        cx.update(|window, cx| {
            // A frame that reuses any view keeps every debug bound of the
            // previous one, so an element no longer painted would still be found.
            window.refresh();
            _ = window.draw(cx);
        });
    }

    fn resize(cx: &mut VisualTestContext, width: f32) {
        cx.simulate_resize(size(px(width), px(800.)));
        draw(cx);
    }

    /// Make the test app a mobile build: only there is a phone-sized window
    /// compact, a desktop window being wide at any width.
    fn as_mobile(cx: &mut VisualTestContext) {
        cx.update(|_, cx| crate::window_seam::override_mobile_for_test(cx, true));
    }

    fn touch_drag(
        cx: &mut VisualTestContext,
        id: u64,
        steps: &[(gpui::TouchPhase, f32, f32, u64)],
    ) {
        for &(phase, x, y, elapsed) in steps {
            cx.executor().advance_clock(Duration::from_millis(elapsed));
            cx.update(|window, cx| {
                window.dispatch_event(
                    gpui::PlatformInput::Touch(gpui::TouchEvent {
                        id: gpui::TouchId(id),
                        phase,
                        position: gpui::point(px(x), px(y)),
                        predicted_position: None,
                        force: None,
                    }),
                    cx,
                );
            });
            draw(cx);
        }
    }

    #[gpui::test]
    fn edge_swipe_dismisses_sheet_before_navigating_underneath(cx: &mut TestAppContext) {
        use gpui::TouchPhase::{Ended, Moved, Started};
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        let (shell, _host, cx) = mount(cx);
        as_mobile(cx);
        resize(cx, 393.);
        cx.update(|window, cx| set_back_target(window.window_handle(), &shell, cx));
        cx.update(|window, cx| shell.update(cx, |shell, cx| while shell.back(window, cx) {}));
        draw(cx);
        assert!(!cx.cx.update(can_swipe_back));
        shell.update(cx, |shell, cx| shell.go(Destination::Thread, cx));
        draw(cx);
        cx.executor().advance_clock(Duration::from_millis(250));
        draw(cx);
        let state = shell.read_with(cx, |shell, _| shell.window_state());
        let history = state.read_with(cx, |state, _| state.history().to_vec());
        for (id, end_x, elapsed, closes) in [
            (1, 70., 400, false),
            (2, 180., 400, true),
            (3, 70., 40, true),
        ] {
            state.update(cx, |state, cx| state.open_palette(cx));
            draw(cx);
            cx.executor().advance_clock(Duration::from_millis(250));
            draw(cx);
            assert!(cx.cx.update(can_swipe_back));
            let resting = cx.debug_bounds("palette-card").unwrap();
            touch_drag(
                cx,
                id,
                &[
                    (Started, 10., 600., 0),
                    (Moved, (10. + end_x) / 2., 600., elapsed / 2),
                    (Moved, end_x, 600., elapsed / 2),
                ],
            );
            let dragged = cx.debug_bounds("palette-card").unwrap();
            assert!(
                dragged.top() > resting.top(),
                "sheet follows the edge gesture vertically"
            );
            touch_drag(cx, id, &[(Ended, end_x, 600., 10)]);
            draw(cx);
            cx.executor().advance_clock(Duration::from_millis(500));
            draw(cx);
            assert_eq!(
                state.read_with(cx, |state, _| state.palette_open),
                !closes,
                "gesture {id}"
            );
            assert_eq!(
                state.read_with(cx, |state, _| state.history().to_vec()),
                history
            );
            if !closes {
                assert_eq!(cx.debug_bounds("palette-card").unwrap(), resting);
                state.update(cx, |state, cx| state.close_palette(cx));
                draw(cx);
                cx.executor().advance_clock(Duration::from_millis(250));
                draw(cx);
            } else {
                assert!(cx.debug_bounds("palette-card").is_none());
            }
        }
        let depth = shell.read_with(cx, |shell, cx| shell.nav.read(cx).depth());
        assert!(cx.cx.update(handle_back));
        draw(cx);
        shell.read_with(cx, |shell, cx| {
            assert_eq!(shell.nav.read(cx).depth(), depth - 1)
        });
    }

    #[gpui::test]
    fn image_viewer_takes_back_before_the_page_underneath(cx: &mut TestAppContext) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        let (shell, _host, cx) = mount(cx);
        as_mobile(cx);
        resize(cx, 393.);
        cx.update(|window, cx| set_back_target(window.window_handle(), &shell, cx));
        shell.update(cx, |shell, cx| shell.go(Destination::Thread, cx));
        draw(cx);
        cx.executor().advance_clock(Duration::from_millis(250));
        draw(cx);
        assert!(cx.cx.update(can_swipe_back));
        let depth = shell.read_with(cx, |shell, cx| shell.nav.read(cx).depth());
        cx.update(|window, cx| {
            crate::image_viewer::open(
                gpui::ImageSource::Custom(std::sync::Arc::new(|_, _| None)),
                "image.png",
                window,
                cx,
            )
        });
        draw(cx);
        assert!(!cx.cx.update(can_swipe_back));
        assert!(cx.cx.update(handle_back));
        draw(cx);
        cx.executor().advance_clock(Duration::from_millis(250));
        draw(cx);
        assert!(!cx.update(|window, cx| crate::overlay::dialog_open(window, cx)));
        shell.read_with(cx, |shell, cx| {
            assert_eq!(shell.nav.read(cx).depth(), depth)
        });
        assert!(cx.cx.update(can_swipe_back));
    }

    #[gpui::test]
    fn compact_edge_swipe_tracks_cancels_and_completes_before_next_tap(cx: &mut TestAppContext) {
        use gpui::TouchPhase::{Cancelled, Ended, Moved, Started};

        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        let (shell, host, cx) = mount(cx);
        as_mobile(cx);
        resize(cx, 393.);
        let root = shell.read_with(cx, |shell, cx| shell.nav.read(cx).depth());
        let mut meta = tcode_core::project::SessionMeta::new(
            agent::ProviderKind::ClaudeCode,
            "/tmp/tcode-compact-panel".into(),
            None,
        );
        meta.id = "thread-1".into();
        for (topic, event) in [
            (
                Topic::Index,
                ServerEvent::IndexSnapshot(IndexSnapshot {
                    summary: Default::default(),
                    sessions: vec![meta],
                    projects: Vec::new(),
                }),
            ),
            (
                Topic::Settings,
                ServerEvent::SettingsSnapshot(Default::default()),
            ),
        ] {
            host.incoming
                .try_send(
                    encode_line(&HostMessage::Event(EventEnvelope {
                        request_id: None,
                        topic,
                        event,
                    }))
                    .unwrap(),
                )
                .unwrap();
        }
        let conversation = (0..24)
            .flat_map(|index| {
                [
                    agent::AgentEvent::TurnStarted {
                        turn_id: format!("scroll-{index}"),
                    },
                    agent::AgentEvent::ItemCompleted(agent::ThreadItem {
                        id: format!("message-{index}"),
                        parent_item_id: None,
                        content: agent::ItemContent::UserMessage {
                            text: format!("Keep reading position {index}\n").repeat(4),
                            context_len: None,
                            attachments: Vec::new(),
                        },
                    }),
                    agent::AgentEvent::TurnCompleted {
                        turn_id: format!("scroll-{index}"),
                        status: agent::TurnStatus::Completed,
                        usage: None,
                    },
                ]
            })
            .collect();
        seed_wide_diff_with(&shell, &host, cx, conversation);
        draw(cx);
        let chat = shell.read_with(cx, |shell, _| {
            shell.attachment.as_ref().unwrap().chat.clone()
        });
        let mut reading_position = None;
        for destination in [Destination::Thread, Destination::Panel] {
            shell.update(cx, |shell, cx| shell.go(destination, cx));
            draw(cx);
            cx.executor().advance_clock(Duration::from_millis(250));
            draw(cx);
            if destination == Destination::Thread {
                let viewport = chat.read_with(cx, |chat, _| chat.capture_viewport().unwrap());
                cx.simulate_event(gpui::ScrollWheelEvent {
                    position: viewport.center(),
                    delta: gpui::ScrollDelta::Pixels(gpui::point(px(0.), px(1000.))),
                    ..Default::default()
                });
                draw(cx);
                cx.executor().advance_clock(Duration::from_secs(1));
                draw(cx);
                assert!(
                    cx.debug_bounds("scroll-to-end").is_some(),
                    "fixture scrolls away from the conversation tail"
                );
                reading_position = (0..100).find_map(|index| {
                    let selector: &'static str = format!("timeline-row-{index}").leak();
                    let bounds = cx.debug_bounds(selector)?;
                    (bounds.top() >= viewport.top() && bounds.top() < viewport.bottom())
                        .then_some((selector, bounds.top()))
                });
                assert!(
                    reading_position.is_some(),
                    "a scrolled conversation row is visible"
                );
            }
        }
        let (reading_row, reading_top) = reading_position.unwrap();
        let state = shell.read_with(cx, |shell, _| shell.window_state());
        let history = state.read_with(cx, |state, _| state.history().to_vec());
        let composer = shell.read_with(cx, |shell, cx| {
            shell.attachment.as_ref().unwrap().chat.read(cx).composer()
        });
        cx.update(|window, cx| {
            composer.update(cx, |composer, cx| {
                composer.set_draft("keep this draft", window, cx);
            })
        });
        draw_until(&shell, cx, &host, "diff-body", None);
        let line = "diff-line-new-a/deeply/nested/module/tree/with/an/absurdly/long/file_name.rs-1";
        for (id, mode, end_x, elapsed, commits) in [
            (3, crate::theme::ThemeMode::Light, 70., 400, false),
            (4, crate::theme::ThemeMode::Light, 180., 400, true),
            (5, crate::theme::ThemeMode::Light, 70., 40, true),
            (6, crate::theme::ThemeMode::Dark, 70., 400, false),
            (7, crate::theme::ThemeMode::Dark, 180., 400, true),
            (8, crate::theme::ThemeMode::Dark, 70., 40, true),
        ] {
            cx.update(|window, cx| crate::theme::change_mode(mode, Some(window), cx));
            draw(cx);
            let initial_line = cx.debug_bounds(line).expect("diff line before scrolling");
            let body = cx.debug_bounds("diff-body").unwrap();
            cx.simulate_event(gpui::ScrollWheelEvent {
                position: body.center(),
                delta: gpui::ScrollDelta::Pixels(gpui::point(px(-120.), px(0.))),
                ..Default::default()
            });
            draw(cx);
            cx.executor().advance_clock(Duration::from_secs(1));
            draw(cx);
            let before_swipe = cx.debug_bounds(line).expect("diff before gesture");
            assert!(
                before_swipe.left() < initial_line.left(),
                "fixture scrolls real content"
            );
            touch_drag(
                cx,
                id,
                &[
                    (Started, 10., 300., 0),
                    (Moved, 40., 300., elapsed / 2),
                    (Moved, end_x, 300., elapsed / 2),
                ],
            );
            let current = cx
                .debug_bounds("back-current-page")
                .expect("page follows finger");
            assert!((current.left() - px(end_x - 10.)).abs() < px(1.));
            let previous = cx
                .debug_bounds("back-previous-page")
                .expect("real previous page");
            assert!(previous.left() < px(0.) && previous.right() < px(393.));
            assert!(cx.debug_bounds("compact-thread-page").is_some());
            assert!(
                cx.debug_bounds("chat-composer").is_some(),
                "underlying composer is visible"
            );
            assert_eq!(
                cx.debug_bounds(reading_row).unwrap().top(),
                reading_top,
                "previous page keeps its scroll position"
            );
            assert_eq!(
                state.read_with(cx, |state, _| state.history().to_vec()),
                history
            );
            touch_drag(cx, id, &[(Ended, end_x, 300., 10)]);
            assert_eq!(
                cx.debug_bounds("back-current-page").unwrap(),
                current,
                "release starts at the finger"
            );
            cx.executor().advance_clock(Duration::from_secs(1));
            draw(cx);
            assert!(cx.debug_bounds("back-current-page").is_none());
            assert_eq!(
                composer.read_with(cx, |composer, cx| composer.draft(cx)),
                "keep this draft"
            );
            if commits {
                assert_eq!(
                    state.read_with(cx, |state, _| state.destination()),
                    Destination::Thread
                );
                shell.update(cx, |shell, cx| shell.go(Destination::Panel, cx));
                draw(cx);
                cx.executor().advance_clock(Duration::from_millis(250));
                draw(cx);
            } else {
                assert_eq!(
                    cx.debug_bounds(line).unwrap(),
                    before_swipe,
                    "cancel keeps scroll position for gesture {id}"
                );
                assert_eq!(
                    state.read_with(cx, |state, _| state.history().to_vec()),
                    history
                );
            }
        }
        touch_drag(cx, 9, &[(Started, 10., 300., 0), (Moved, 70., 300., 200)]);
        assert!(cx.debug_bounds("back-current-page").is_some());
        resize(cx, 1024.);
        resize(cx, 393.);
        touch_drag(cx, 9, &[(Cancelled, 70., 300., 0)]);
        assert!(cx.debug_bounds("back-current-page").is_none());
        assert_eq!(
            state.read_with(cx, |state, _| state.history().to_vec()),
            history
        );
        // Touch hardware reports a moving finger every 8–16ms, so the release
        // carries a fling velocity.
        let mut swipe = vec![(Started, 10., 300., 0)];
        swipe.extend((1..=10).map(|step| (Moved, 10. + 20. * step as f32, 300., 8)));
        swipe.push((Ended, 210., 300., 8));
        touch_drag(cx, 1, &swipe);
        cx.executor().advance_clock(Duration::from_millis(300));
        draw(cx);
        shell.read_with(cx, |shell, cx| {
            assert_eq!(shell.nav.read(cx).depth(), root + 1)
        });
        let back = cx
            .debug_bounds("compact-back")
            .expect("Back on revealed page");
        let x = f32::from(back.center().x);
        let y = f32::from(back.center().y);
        touch_drag(cx, 2, &[(Started, x, y, 0), (Ended, x, y, 50)]);
        shell.read_with(cx, |shell, cx| assert_eq!(shell.nav.read(cx).depth(), root));
    }

    #[gpui::test]
    fn compact_touch_drag_away_from_edge_preserves_page(cx: &mut TestAppContext) {
        use gpui::TouchPhase::{Ended, Moved, Started};

        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        let (shell, _host, cx) = mount(cx);
        as_mobile(cx);
        resize(cx, 393.);
        shell.update(cx, |shell, cx| shell.go(Destination::Thread, cx));
        draw(cx);
        cx.executor().advance_clock(Duration::from_millis(250));
        draw(cx);
        let before = shell.read_with(cx, |shell, cx| {
            shell.window_state.read(cx).history().to_vec()
        });
        for (id, start_x, middle_x) in [(1, 60., 240.), (2, 60., 10.)] {
            // Catch an edge pan's fling with the next off-edge contact. GPUI
            // starts that contact directly as a pan, without a TouchDrag offer.
            touch_drag(
                cx,
                10 + id,
                &[
                    (Started, 10., 300., 0),
                    (Moved, 25., 300., 10),
                    (Ended, 25., 300., 10),
                ],
            );
            touch_drag(
                cx,
                id,
                &[
                    (Started, start_x, 300., 0),
                    (Moved, middle_x, 300., 50),
                    (Moved, 280., 300., 50),
                    (Ended, 280., 300., 10),
                ],
            );
            shell.read_with(cx, |shell, cx| {
                assert_eq!(shell.window_state.read(cx).history(), before);
                assert_eq!(shell.nav.read(cx).depth(), before.len());
            });
        }
    }

    #[gpui::test]
    fn compact_tap_push_and_back_pop_slide(cx: &mut TestAppContext) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        let (shell, _host, cx) = mount(cx);
        as_mobile(cx);
        resize(cx, 393.);
        cx.executor().advance_clock(Duration::from_millis(250));
        draw(cx);
        shell.update(cx, |shell, cx| shell.go(Destination::Thread, cx));
        draw(cx);
        cx.executor().advance_clock(Duration::from_millis(100));
        draw(cx);
        let incoming = cx
            .debug_bounds("compact-thread-page")
            .expect("incoming thread");
        let outgoing = cx
            .debug_bounds("compact-threads-page")
            .expect("outgoing list");
        assert!(incoming.left() > px(0.) && incoming.left() < px(393.));
        assert!(outgoing.left() < px(0.), "push moves the list left");
        cx.executor().advance_clock(Duration::from_millis(150));
        draw(cx);
        assert_eq!(
            cx.debug_bounds("compact-thread-page").unwrap().left(),
            px(0.)
        );
        assert!(cx.update(|window, cx| shell.update(cx, |shell, cx| shell.back(window, cx))));
        draw(cx);
        cx.executor().advance_clock(Duration::from_millis(100));
        draw(cx);
        let outgoing = cx
            .debug_bounds("compact-thread-page")
            .expect("outgoing thread");
        let incoming = cx
            .debug_bounds("compact-threads-page")
            .expect("incoming list");
        assert!(outgoing.left() > px(0.) && outgoing.left() < px(393.));
        assert!(
            incoming.left() < px(0.),
            "pop reveals the list from the left"
        );
    }

    /// Every line the client has sent since the last drain.
    fn sent(host: &MountedShell) -> Vec<ClientPayload> {
        let mut payloads: Vec<_> = host
            .observed
            .borrow_mut()
            .drain(..)
            .map(|message| message.payload)
            .collect();
        while let Ok(line) = host.outgoing.try_recv() {
            payloads.push(decode_client_line(&line).expect("client line").payload);
        }
        payloads
    }

    fn store_of(shell: &Entity<AppShell>, cx: &VisualTestContext) -> Entity<WorkspaceStore> {
        shell.read_with(cx, |shell, _| shell.store().expect("attached"))
    }

    /// A long title takes the header's free width; it is cut only once the
    /// header runs out of room, whether or not a pull request is linked.
    #[gpui::test]
    fn a_long_title_uses_the_headers_free_width(cx: &mut TestAppContext) {
        cx.update(crate::theme::init);
        let root = crate::store::tests::scratch_root("long-title");
        let disk = tcode_services::store::SessionStore::open_at(root.clone()).unwrap();
        let project = tcode_core::project::Project::from_root(root.join("project"));
        let mut meta = tcode_core::project::SessionMeta::new(
            agent::ProviderKind::Codex,
            project.root.clone(),
            None,
        );
        meta.id = "first".into();
        meta.title = "Investigate why the composer drops its stop button mid-turn".into();
        meta.project_id = Some(project.id.clone());
        let host =
            tcode_runtime::pipe::spawn_host(disk, tcode_runtime::pipe::HostServices::default())
                .unwrap();
        smol::block_on(host.update_state_for_test(move |state, _| {
            state.projects = vec![project];
            state.sessions = vec![meta].into();
        }))
        .unwrap();
        let mut driver = crate::store::tests::LocalHostDriver::new(&host);
        let (shell, _transport, cx) = mount(cx);
        let store = store_of(&shell, cx);
        store.update(cx, |store, cx| {
            *store = WorkspaceStore::new_attached(
                driver.link.clone(),
                crate::store::WorkspaceAttachment::Local,
                None,
                None,
                false,
                cx,
            );
            store.select_session("first".into());
        });
        driver.wait_until(cx, &store, "selected thread baseline", |cx| {
            store.read_with(cx, |store, _| {
                store.baseline_ready() && !store.threads_loading() && !store.chat_loading()
            })
        });
        resize(cx, 1600.);
        shell.update(cx, |shell, cx| shell.go(Destination::Thread, cx));
        draw(cx);
        let title = cx.debug_bounds("chat-title").expect("thread title");
        assert!(
            title.size.width > px(300.),
            "the title is cut to {:?} in a 1600px window",
            title.size.width
        );
        driver.shutdown(cx, &store, &host);
        std::fs::remove_dir_all(root).unwrap();
    }

    /// The model picker raises the keyboard only where it cannot cover the
    /// list: a desktop window (even a narrow one) focuses the search as it
    /// opens; a phone leaves it unfocused, with the picker still owning focus
    /// so Escape closes it.
    #[gpui::test]
    fn model_picker_focuses_its_search_only_off_phone(cx: &mut TestAppContext) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        cx.update(crate::theme::init);
        let root = crate::store::tests::scratch_root("model-picker-focus");
        let disk = tcode_services::store::SessionStore::open_at(root.clone()).unwrap();
        let project = tcode_core::project::Project::from_root(root.join("project"));
        let mut meta = tcode_core::project::SessionMeta::new(
            agent::ProviderKind::Codex,
            project.root.clone(),
            None,
        );
        meta.id = "first".into();
        meta.project_id = Some(project.id.clone());
        let host =
            tcode_runtime::pipe::spawn_host(disk, tcode_runtime::pipe::HostServices::default())
                .unwrap();
        smol::block_on(host.update_state_for_test(move |state, _| {
            state.projects = vec![project];
            state.sessions = vec![meta].into();
        }))
        .unwrap();
        let mut driver = crate::store::tests::LocalHostDriver::new(&host);
        let (shell, _transport, cx) = mount(cx);
        cx.update(|window, cx| set_back_target(window.window_handle(), &shell, cx));
        let store = store_of(&shell, cx);
        store.update(cx, |store, cx| {
            *store = WorkspaceStore::new_attached(
                driver.link.clone(),
                crate::store::WorkspaceAttachment::Local,
                None,
                None,
                false,
                cx,
            );
            store.select_session("first".into());
        });
        driver.wait_until(cx, &store, "selected thread baseline", |cx| {
            store.read_with(cx, |store, _| {
                store.baseline_ready() && !store.threads_loading() && !store.chat_loading()
            })
        });
        let search = shell.read_with(cx, |shell, cx| {
            let chat = shell.attachment.as_ref().unwrap().chat.read(cx);
            chat.composer().read(cx).model_search_focus_handle(cx)
        });

        for (mobile, width) in [(false, 1200.), (false, 393.), (true, 393.)] {
            cx.update(|_, cx| crate::window_seam::override_mobile_for_test(cx, mobile));
            resize(cx, width);
            shell.update(cx, |shell, cx| shell.go(Destination::Thread, cx));
            // Let the compact push animation settle so the picker is in place.
            cx.executor().advance_clock(Duration::from_millis(400));
            draw(cx);
            let picker = cx.debug_bounds("model-picker").expect("model picker");
            cx.simulate_click(picker.center(), gpui::Modifiers::default());
            draw(cx);
            draw(cx);
            let compact = shell.read_with(cx, |shell, cx| shell.compact(cx));
            assert_eq!(
                cx.debug_bounds("touch-picker-sheet").is_some(),
                compact,
                "mobile={mobile} width={width}: a compact window opens a sheet"
            );
            cx.update(|window, _| {
                assert_eq!(
                    search.is_focused(window),
                    !mobile,
                    "mobile={mobile} width={width}: the search raises the keyboard only off-phone"
                );
            });
            cx.simulate_keystrokes("escape");
            draw(cx);
            draw(cx);
            assert!(
                cx.debug_bounds("touch-picker-sheet").is_none(),
                "mobile={mobile} width={width}: Escape reaches the picker through its focus"
            );
            cx.update(|window, _| assert!(!search.is_focused(window)));
        }
        cx.update(|_, cx| crate::window_seam::override_mobile_for_test(cx, false));
        driver.shutdown(cx, &store, &host);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[gpui::test]
    fn thread_shortcuts_follow_list_order_and_leave_model_picker_numbers_alone(
        cx: &mut TestAppContext,
    ) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        let root = crate::store::tests::scratch_root("thread-shortcuts");
        let disk = tcode_services::store::SessionStore::open_at(root.clone()).unwrap();
        let project = tcode_core::project::Project::from_root(root.join("project"));
        let sessions: Vec<_> = [("third", 10), ("first", 30), ("second", 20)]
            .into_iter()
            .map(|(id, updated_at)| {
                let mut meta = tcode_core::project::SessionMeta::new(
                    agent::ProviderKind::Codex,
                    project.root.clone(),
                    None,
                );
                meta.id = id.into();
                meta.project_id = Some(project.id.clone());
                meta.updated_at = updated_at;
                meta
            })
            .collect();
        let host =
            tcode_runtime::pipe::spawn_host(disk, tcode_runtime::pipe::HostServices::default())
                .unwrap();
        smol::block_on(host.update_state_for_test(move |state, _| {
            state.projects = vec![project];
            state.sessions = sessions.into();
        }))
        .unwrap();
        let mut driver = crate::store::tests::LocalHostDriver::new(&host);
        let (shell, _transport, cx) = mount(cx);
        cx.update(|_, cx| crate::shortcut::init(cx));
        cx.update(|window, cx| set_back_target(window.window_handle(), &shell, cx));
        let store = store_of(&shell, cx);
        store.update(cx, |store, cx| {
            *store = WorkspaceStore::new_attached(
                driver.link.clone(),
                crate::store::WorkspaceAttachment::Local,
                None,
                None,
                false,
                cx,
            );
            store.select_session("first".into());
        });
        driver.wait_until(cx, &store, "selected thread baseline", |cx| {
            store.read_with(cx, |store, _| {
                store.baseline_ready() && !store.threads_loading() && !store.chat_loading()
            })
        });
        resize(cx, 1200.);
        draw(cx);

        let [first_key, second_key, out_of_range_key] = if cfg!(target_os = "macos") {
            ["cmd-1", "cmd-2", "cmd-9"]
        } else {
            ["ctrl-1", "ctrl-2", "ctrl-9"]
        };
        let picker = cx.debug_bounds("model-picker").expect("model picker");
        cx.simulate_click(picker.center(), gpui::Modifiers::default());
        draw(cx);
        cx.simulate_keystrokes(second_key);
        draw(cx);
        assert_eq!(
            store
                .read_with(cx, |store, _| store.active_session_id())
                .as_deref(),
            Some("first"),
            "model picker must keep number keys"
        );
        cx.simulate_keystrokes("escape");
        draw(cx);

        for (keys, expected) in [
            (second_key, "second"),
            ("ctrl-tab", "third"),
            ("ctrl-tab", "first"),
            ("ctrl-shift-tab", "third"),
            (out_of_range_key, "third"),
            (first_key, "first"),
        ] {
            cx.simulate_keystrokes(keys);
            driver.wait_until(cx, &store, "selected thread baseline", |cx| {
                store.read_with(cx, |store, _| {
                    store.baseline_ready() && !store.threads_loading() && !store.chat_loading()
                })
            });
            assert_eq!(
                store
                    .read_with(cx, |store, _| store.active_session_id())
                    .as_deref(),
                Some(expected),
                "{keys}"
            );
        }
        cx.update(|window, cx| window.blur(cx));
        draw(cx);
        cx.simulate_keystrokes(second_key);
        assert_eq!(
            store
                .read_with(cx, |store, _| store.active_session_id())
                .as_deref(),
            Some("second"),
            "shortcuts work without a focused input"
        );
        shell.update(cx, |shell, cx| shell.go(Destination::Settings, cx));
        draw(cx);
        cx.simulate_keystrokes(first_key);
        assert_eq!(
            shell.read_with(cx, |shell, cx| shell.destination(cx)),
            Destination::Thread
        );
        as_mobile(cx);
        resize(cx, 393.);
        shell.read_with(cx, |shell, cx| {
            assert!(shell.compact(cx));
            assert_eq!(shell.destination(cx), Destination::Thread);
        });
        cx.simulate_keystrokes("ctrl-tab");
        assert_eq!(
            store
                .read_with(cx, |store, _| store.active_session_id())
                .as_deref(),
            Some("second"),
            "compact navigation uses the same shortcuts"
        );
        cx.update(|_, cx| crate::window_seam::override_mobile_for_test(cx, false));
        resize(cx, 1200.);
        draw(cx);
        let row = cx
            .debug_bounds("sidebar-thread-second")
            .expect("second row");
        let settle = gpui::point(row.right() - px(18.), row.top() + px(14.));
        cx.simulate_mouse_move(settle, None, gpui::Modifiers::default());
        draw(cx);
        cx.simulate_click(settle, gpui::Modifiers::default());
        driver.wait_until(cx, &store, "thread settlement update", |cx| {
            store.read_with(cx, |store, _| {
                store
                    .sidebar_sessions()
                    .iter()
                    .any(|meta| meta.id == "second" && meta.is_settled())
            })
        });
        draw(cx);
        assert!(cx.update(|window, cx| window.has_notification::<crate::sidebar::ThreadUndo>(cx)));
        let undo_key = if cfg!(target_os = "macos") {
            "cmd-z"
        } else {
            "ctrl-z"
        };
        let composer = shell.read_with(cx, |shell, cx| {
            shell.attachment.as_ref().unwrap().chat.read(cx).composer()
        });
        let input = composer.read_with(cx, |composer, cx| composer.input_focus_handle(cx));
        cx.update(|window, cx| input.focus(window, cx));
        draw(cx);
        cx.simulate_keystrokes("x");
        draw(cx);
        assert_eq!(
            composer.read_with(cx, |composer, cx| composer.draft(cx)),
            "x"
        );
        cx.simulate_keystrokes(undo_key);
        assert_eq!(
            composer.read_with(cx, |composer, cx| composer.draft(cx)),
            ""
        );
        assert!(store.read_with(cx, |store, _| {
            store
                .sidebar_sessions()
                .iter()
                .any(|meta| meta.id == "second" && meta.is_settled())
        }));
        cx.update(|window, cx| window.blur(cx));
        draw(cx);
        cx.simulate_keystrokes(undo_key);
        driver.wait_until(cx, &store, "thread settlement update", |cx| {
            store.read_with(cx, |store, _| {
                store
                    .sidebar_sessions()
                    .iter()
                    .any(|meta| meta.id == "second" && !meta.is_settled())
            })
        });
        assert!(!cx.update(|window, cx| window.has_notification::<crate::sidebar::ThreadUndo>(cx)));
        driver.shutdown(cx, &store, &host);
        std::fs::remove_dir_all(root).unwrap();
    }

    /// A desktop tiled to half a screen is still the whole product: the width
    /// rule belongs to mobile builds, so no desktop width is compact.
    #[gpui::test]
    fn a_narrow_desktop_window_keeps_the_wide_workspace(cx: &mut TestAppContext) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        let (shell, _host, cx) = mount(cx);
        cx.update(|_, cx| crate::window_seam::override_mobile_for_test(cx, false));
        for width in [1024., 899., 400.] {
            resize(cx, width);
            shell.read_with(cx, |shell, cx| {
                assert!(!shell.compact(cx), "{width}px on a desktop build is wide");
                assert_eq!(shell.window_state.read(cx).route(), Route::Chat);
            });
            assert!(
                cx.debug_bounds("sidebar-feature-hosts").is_some(),
                "{width}px keeps the workspace sidebar"
            );
            assert!(cx.debug_bounds("compact-threads-page").is_none());
        }
    }

    /// Rotating a tablet across the breakpoint is a layout decision and
    /// nothing else: it must not touch the attachment, the selected thread,
    /// the draft or the split.
    #[gpui::test]
    fn the_breakpoint_flips_at_nine_hundred_and_disturbs_nothing_else(cx: &mut TestAppContext) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        let (shell, host, cx) = mount(cx);
        as_mobile(cx);
        let store = store_of(&shell, cx);
        host.incoming
            .try_send(
                encode_line(&HostMessage::Event(EventEnvelope {
                    request_id: None,
                    topic: Topic::Index,
                    event: ServerEvent::IndexSnapshot(IndexSnapshot {
                        summary: Default::default(),
                        sessions: Vec::new(),
                        projects: Vec::new(),
                    }),
                }))
                .unwrap(),
            )
            .unwrap();
        store.update(cx, |store, cx| {
            store.drain_host_events_for_test(cx);
            store.select_session("thread-1".into());
        });

        resize(cx, 1024.);
        shell.read_with(cx, |shell, cx| {
            assert!(!shell.compact(cx), "1024px is the wide split");
        });
        draw(cx);
        draw(cx);
        let from = gpui::point(px(SIDEBAR_WIDTH), px(400.));
        let to = gpui::point(px(300.), px(400.));
        cx.simulate_mouse_down(from, MouseButton::Left, gpui::Modifiers::default());
        for step in [from + gpui::point(px(5.), px(0.)), to] {
            cx.simulate_mouse_move(step, Some(MouseButton::Left), gpui::Modifiers::default());
            draw(cx);
        }
        cx.simulate_mouse_up(to, MouseButton::Left, gpui::Modifiers::default());
        draw(cx);
        shell.read_with(cx, |shell, cx| {
            let attachment = shell.attachment.as_ref().unwrap();
            assert_eq!(attachment.split.read(cx).sizes()[0], px(300.));
            assert_eq!(attachment.sidebar_width.get(), px(300.));
        });
        let composer = shell.read_with(cx, |shell, cx| {
            shell
                .attachment
                .as_ref()
                .expect("attached")
                .chat
                .read(cx)
                .composer()
        });
        cx.update(|window, cx| {
            composer.update(cx, |composer, cx| {
                composer.set_draft("half a thought", window, cx)
            });
        });
        draw(cx);
        let _ = sent(&host);

        resize(cx, 899.);
        shell.read_with(cx, |shell, cx| {
            assert!(shell.compact(cx), "899px is compact");
            assert_eq!(
                shell.destination(cx),
                Destination::Thread,
                "a selected thread is the compact destination"
            );
        });
        resize(cx, 900.);
        shell.read_with(cx, |shell, cx| {
            assert!(!shell.compact(cx), "900px is wide, not compact");
        });
        resize(cx, 899.);
        shell.read_with(cx, |shell, cx| assert!(shell.compact(cx)));

        // The thread, the draft and the composer itself all survive.
        assert_eq!(
            store.read_with(cx, |store, _| store.active_session_id()),
            Some("thread-1".into())
        );
        composer.read_with(cx, |composer, cx| {
            assert_eq!(composer.draft(cx), "half a thought");
            assert!(composer.is_compact(), "the composer followed the layout");
        });
        assert_eq!(
            shell.read_with(cx, |shell, cx| shell
                .attachment
                .as_ref()
                .unwrap()
                .chat
                .read(cx)
                .composer()
                .entity_id()),
            composer.entity_id(),
            "the composer entity must be updated, never rebuilt"
        );

        // Nothing about the link changed: no shutdown, and no fresh index
        // subscription (which is what a reattach would look like).
        for payload in sent(&host) {
            assert!(
                !matches!(
                    payload,
                    ClientPayload::Command(Command::ShutdownAllAndFlush)
                        | ClientPayload::Subscribe(tcode_protocol::Subscription {
                            topic: Topic::Index,
                            ..
                        })
                ),
                "resizing must not detach or reconnect: {payload:?}"
            );
        }

        resize(cx, 1024.);
        draw(cx);
        draw(cx);
        shell.read_with(cx, |shell, cx| {
            assert!(!shell.compact(cx));
            assert_eq!(
                shell
                    .attachment
                    .as_ref()
                    .expect("attached")
                    .split
                    .read(cx)
                    .sizes()
                    .first()
                    .copied(),
                Some(px(300.)),
                "the wide split comes back at the width it was left at"
            );
        });
    }

    /// The one Back chain: overlays first, then one step of the window's
    /// history, and `false` only at the root — where the platform closes the
    /// app. Leaving a page never touches the attachment.
    #[cfg(preview_backend)]
    #[gpui::test]
    fn compact_preview_release_and_navigation(cx: &mut TestAppContext) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        let (shell, host, _, cx) = mount_restored(cx, &["hosts", "threads", "thread"], true);
        let store = store_of(&shell, cx);
        restore_index(&shell, &host, true, cx);
        restore_status(&shell, &host, cx);
        await_restore_update(&shell, cx, |store| {
            store.preview_active_identity().is_some()
        });
        cx.simulate_resize(size(px(393.), px(852.)));
        draw(cx);
        let (preview, state) = shell.read_with(cx, |shell, _| {
            (
                shell.attachment.as_ref().unwrap().preview.clone(),
                shell.window_state(),
            )
        });
        let lifecycle = preview.read_with(cx, |preview, _| preview.lifecycle());
        let key = store.read_with(cx, |store, _| store.preview_active_identity().unwrap().1);
        store.update(cx, |store, cx| {
            store.toggle_preview_panel(cx);
            store.set_preview_url(&key, "https://example.com/".into(), cx);
        });
        lifecycle.update(cx, |lifecycle, _| lifecycle.seed_stub(&key));
        state.update(cx, |state, cx| state.go(Destination::Panel, cx));
        draw(cx);
        cx.update(|window, cx| {
            shell.update(cx, |shell, cx| assert!(shell.back(window, cx)));
        });
        draw(cx);
        assert!(lifecycle.read_with(cx, |lifecycle, _| lifecycle.has_view(&key)));
        assert_eq!(
            store
                .read_with(cx, |store, _| store.preview_url(&key))
                .as_deref(),
            Some("https://example.com/")
        );
        state.update(cx, |state, cx| state.go(Destination::Panel, cx));
        draw(cx);
        // Dispatch the same action as the compact overflow menu.
        cx.update(|window, cx| {
            preview.update(cx, |preview, cx| {
                preview.on_preview_action(&crate::preview_panel::PreviewAction::Close, window, cx)
            });
        });
        draw(cx);
        assert!(!lifecycle.read_with(cx, |lifecycle, _| lifecycle.has_view(&key)));
        assert_eq!(
            store.read_with(cx, |store, _| store.preview_url(&key)),
            None
        );
        assert_eq!(
            preview.read_with(cx, |preview, cx| preview.url_field(cx)),
            ""
        );
        assert_eq!(
            state.read_with(cx, |state, _| state.destination()),
            Destination::Panel
        );
        store.update(cx, |store, cx| {
            store.set_preview_url(&key, "https://example.com/again".into(), cx)
        });
        lifecycle.update(cx, |lifecycle, _| lifecycle.seed_stub(&key));
        cx.update(|window, cx| shell.update(cx, |shell, cx| assert!(shell.back(window, cx))));
        draw(cx);
        cx.update(|window, cx| shell.update(cx, |shell, cx| assert!(shell.back(window, cx))));
        draw(cx);
        assert_eq!(
            state.read_with(cx, |state, _| state.destination()),
            Destination::Threads
        );
        assert!(!lifecycle.read_with(cx, |lifecycle, _| lifecycle.has_view(&key)));
        assert_eq!(
            store.read_with(cx, |store, _| store.preview_url(&key)),
            None
        );
    }

    #[gpui::test]
    fn back_unwinds_overlays_then_the_history_and_stops_at_the_root(cx: &mut TestAppContext) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        let (shell, _host, cx) = mount(cx);
        let store = store_of(&shell, cx);
        store.update(cx, |store, _| store.select_session("thread-1".into()));
        as_mobile(cx);
        resize(cx, 393.);

        let back = |cx: &mut VisualTestContext| {
            let consumed =
                cx.update(|window, cx| shell.update(cx, |shell, cx| shell.back(window, cx)));
            draw(cx);
            consumed
        };

        shell.read_with(cx, |shell, cx| {
            assert_eq!(shell.destination(cx), Destination::Thread)
        });

        // An open overlay outranks navigation.
        let window_state = shell.read_with(cx, |shell, _| shell.window_state());
        window_state.update(cx, |state, cx| state.open_palette(cx));
        assert!(back(cx), "back closes the overlay");
        shell.read_with(cx, |shell, cx| {
            assert!(!shell.window_state.read(cx).palette_open);
            assert_eq!(
                shell.destination(cx),
                Destination::Thread,
                "and nothing else"
            );
        });

        // Thread → Threads → Hosts. Neither step detaches: only connecting
        // somewhere else, or an explicit Disconnect, does that.
        assert!(back(cx));
        shell.read_with(cx, |shell, cx| {
            assert_eq!(shell.destination(cx), Destination::Threads);
            assert!(shell.attachment.is_some(), "still attached");
        });
        assert!(back(cx));
        shell.read_with(cx, |shell, cx| {
            assert_eq!(shell.destination(cx), Destination::Hosts);
            assert!(
                shell.attachment.is_some(),
                "walking back to the hosts list is not a decision to leave the host"
            );
        });

        window_state.update(cx, |state, cx| state.go(Destination::Pair, cx));
        draw(cx);
        assert!(back(cx));
        assert_eq!(
            shell.read_with(cx, |shell, cx| shell.destination(cx)),
            Destination::Hosts
        );
        assert!(!back(cx), "the root belongs to the platform");
    }

    /// Settings is two pages of that same history, not a route with a back row
    /// of its own: detail → root → whatever Settings was opened from.
    #[gpui::test]
    fn back_leaves_a_settings_section_then_settings_then_the_page_below(cx: &mut TestAppContext) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        let (shell, _host, cx) = mount(cx);
        as_mobile(cx);
        resize(cx, 393.);
        let window_state = shell.read_with(cx, |shell, _| shell.window_state());

        window_state.update(cx, |state, cx| state.open_settings(cx));
        draw(cx);
        let general = cx
            .debug_bounds("settings-nav-general")
            .expect("General settings row");
        cx.simulate_click(general.center(), gpui::Modifiers::default());
        draw(cx);
        shell.read_with(cx, |shell, cx| {
            assert_eq!(shell.destination(cx), Destination::SettingsSection)
        });

        let back = |cx: &mut VisualTestContext| {
            let consumed =
                cx.update(|window, cx| shell.update(cx, |shell, cx| shell.back(window, cx)));
            draw(cx);
            consumed
        };
        assert!(back(cx));
        shell.read_with(cx, |shell, cx| {
            assert_eq!(shell.destination(cx), Destination::Settings)
        });
        assert!(back(cx));
        shell.read_with(cx, |shell, cx| {
            assert_eq!(
                shell.destination(cx),
                Destination::Threads,
                "settings returns to the page it was opened from"
            );
        });
    }

    /// Wide, the pairing page is pushed over Machines, and its header's Back
    /// returns to Machines rather than leaving the Hosts route; Machines, a
    /// root the sidebar switches to, has no Back. What was pasted there, and
    /// the error it drew, stay behind on the page that was left.
    #[gpui::test]
    fn the_wide_pair_page_goes_back_to_machines(cx: &mut TestAppContext) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        let (shell, _host, cx) = mount(cx);
        resize(cx, 1024.);
        let state = shell.read_with(cx, |shell, _| shell.window_state());
        state.update(cx, |state, cx| state.go(Destination::Hosts, cx));
        draw(cx);
        assert!(
            cx.debug_bounds("hosts-back").is_none(),
            "Machines is a root"
        );
        let paste = cx
            .debug_bounds("hosts-pair")
            .expect("Machines offers pasting an invitation");
        cx.simulate_click(paste.center(), gpui::Modifiers::default());
        draw(cx);
        cx.simulate_input("not an invitation");
        draw(cx);
        assert!(
            cx.debug_bounds("hosts-pair-error").is_some(),
            "the paste page flags what is not an invitation"
        );
        let back = cx
            .debug_bounds("hosts-back")
            .expect("the pairing page offers Back");
        cx.simulate_click(back.center(), gpui::Modifiers::default());
        draw(cx);
        assert_eq!(
            shell.read_with(cx, |shell, cx| shell.destination(cx)),
            Destination::Hosts
        );
        assert!(cx.debug_bounds("hosts-route").is_some());
        assert!(
            cx.debug_bounds("hosts-pair-error").is_none(),
            "the error does not follow the user back to Machines"
        );
    }

    /// A store selection can come from the sidebar or palette. In wide layout
    /// it switches the content column away from Hosts without requiring Back.
    #[gpui::test]
    fn wide_hosts_route_keeps_its_sidebar_until_a_thread_selection(cx: &mut TestAppContext) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        let (shell, _host, cx) = mount(cx);
        resize(cx, 1024.);
        draw(cx);
        for thread in ["thread-1", "thread-2"] {
            let feature = cx
                .debug_bounds("sidebar-feature-hosts")
                .expect("the Machines feature row is visible");
            cx.simulate_mouse_down(
                feature.center(),
                gpui::MouseButton::Left,
                gpui::Modifiers::default(),
            );
            cx.simulate_mouse_up(
                feature.center(),
                gpui::MouseButton::Left,
                gpui::Modifiers::default(),
            );
            draw(cx);
            assert!(cx.debug_bounds("hosts-route").is_some());
            assert!(cx.debug_bounds("sidebar-feature-hosts").is_some());
            assert_eq!(
                shell.read_with(cx, |shell, cx| shell.window_state.read(cx).route()),
                Route::Hosts
            );

            let store = store_of(&shell, cx);
            store.update(cx, |store, cx| {
                store.select_session(thread.into());
                cx.notify();
            });
            draw(cx);

            shell.read_with(cx, |shell, cx| {
                assert_eq!(shell.window_state.read(cx).route(), Route::Chat);
            });
            assert!(
                cx.debug_bounds("hosts-route").is_none(),
                "the chat content replaces Machines as soon as the store selection changes"
            );
            assert_eq!(
                shell.read_with(cx, |shell, cx| shell
                    .window_state
                    .read(cx)
                    .history()
                    .to_vec()),
                vec![Destination::Hosts, Destination::Thread]
            );
        }
        let state = shell.read_with(cx, |shell, _| shell.window_state());
        state.update(cx, |state, cx| state.go(Destination::Hosts, cx));
        draw(cx);
        assert!(cx.update(|window, cx| shell.update(cx, |shell, cx| shell.back(window, cx))));
        draw(cx);
        assert_eq!(state.read_with(cx, |state, _| state.route()), Route::Chat);
        assert!(!cx.update(|window, cx| shell.update(cx, |shell, cx| shell.back(window, cx))));

        // The control toggles: on Machines, or on Pair over it, a click returns
        // to the thread the window was showing.
        let click_control = |cx: &mut VisualTestContext| {
            let control = cx
                .debug_bounds("sidebar-feature-hosts")
                .expect("the Machines control is visible");
            cx.simulate_click(control.center(), gpui::Modifiers::default());
            draw(cx);
        };
        for pairing in [false, true] {
            click_control(cx);
            assert_eq!(state.read_with(cx, |state, _| state.route()), Route::Hosts);
            if pairing {
                state.update(cx, |state, cx| state.go(Destination::Pair, cx));
                draw(cx);
            }
            click_control(cx);
            assert_eq!(
                state.read_with(cx, |state, _| state.history().to_vec()),
                vec![Destination::Hosts, Destination::Thread],
                "pairing: {pairing}"
            );
            assert!(cx.debug_bounds("hosts-route").is_none());
        }
    }

    /// Settings replaces the whole window, unlike the Hosts route: the left
    /// column is the settings rail, so the workspace sidebar — and the feature
    /// area in it — is gone while the route is showing.
    #[gpui::test]
    fn the_wide_settings_route_replaces_the_sidebar_with_its_own_rail(cx: &mut TestAppContext) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        let (shell, _host, cx) = mount(cx);
        resize(cx, 1024.);
        let window_state = shell.read_with(cx, |shell, _| shell.window_state());
        window_state.update(cx, |state, cx| state.open_settings(cx));
        draw(cx);
        draw(cx);
        shell.read_with(cx, |shell, cx| {
            assert_eq!(shell.window_state.read(cx).route(), Route::Settings);
        });
        assert!(
            cx.debug_bounds("settings-route").is_some(),
            "settings fills the window"
        );
        assert!(
            cx.debug_bounds("sidebar-feature-hosts").is_none(),
            "and the workspace sidebar is not beside it"
        );
        assert!(
            cx.debug_bounds("settings-device-caption").is_some(),
            "the rail carries the settings group captions instead"
        );
        assert!(
            cx.debug_bounds("settings-nav-threads").is_some(),
            "thread settings are a rail page of their own, as on a phone"
        );
        let general = cx
            .debug_bounds("settings-nav-general")
            .expect("settings navigation");
        cx.simulate_click(general.center(), gpui::Modifiers::default());
        draw(cx);
        assert!(cx.update(|window, cx| shell.update(cx, |shell, cx| shell.back(window, cx))));
        draw(cx);
        assert_eq!(
            window_state.read_with(cx, |state, _| state.route()),
            Route::Chat
        );
        assert!(cx.debug_bounds("sidebar-feature-hosts").is_some());
        assert!(!cx.update(|window, cx| shell.update(cx, |shell, cx| shell.back(window, cx))));
    }

    /// Visiting Hosts from an open thread is navigation, not a reconnection:
    /// the link, the selected thread and the views over it all stay put, and
    /// Back returns to exactly where the visit started.
    #[gpui::test]
    fn a_hosts_visit_keeps_the_link_and_returns_to_the_thread(cx: &mut TestAppContext) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        let (shell, host, cx) = mount(cx);
        let store = store_of(&shell, cx);
        store.update(cx, |store, _| store.select_session("thread-1".into()));
        as_mobile(cx);
        resize(cx, 393.);
        let chat = shell.read_with(cx, |shell, _| {
            shell
                .attachment
                .as_ref()
                .expect("attached")
                .chat
                .entity_id()
        });
        let _ = sent(&host);

        let window_state = shell.read_with(cx, |shell, _| shell.window_state());
        window_state.update(cx, |state, cx| state.go(Destination::Hosts, cx));
        draw(cx);
        shell.read_with(cx, |shell, cx| {
            assert_eq!(shell.destination(cx), Destination::Hosts);
            assert!(shell.attachment.is_some(), "the visit keeps the attachment");
        });
        for payload in sent(&host) {
            assert!(
                !matches!(
                    payload,
                    ClientPayload::Command(Command::ShutdownAllAndFlush)
                        | ClientPayload::Subscribe(tcode_protocol::Subscription {
                            topic: Topic::Index,
                            ..
                        })
                ),
                "visiting hosts must not detach or reconnect: {payload:?}"
            );
        }

        assert!(cx.update(|window, cx| shell.update(cx, |shell, cx| shell.back(window, cx))));
        draw(cx);
        shell.read_with(cx, |shell, cx| {
            assert_eq!(shell.destination(cx), Destination::Thread);
            assert_eq!(
                shell
                    .attachment
                    .as_ref()
                    .expect("attached")
                    .chat
                    .entity_id(),
                chat,
                "the views over the workspace were never rebuilt"
            );
        });
        assert_eq!(
            store.read_with(cx, |store, _| store.active_session_id()),
            Some("thread-1".into())
        );
    }

    /// A session the diff panel can address: the host replaces this value
    /// wholesale, so the fields the panel does not read stay at their zero.
    fn session_status(session: &str, cwd: &std::path::Path) -> tcode_protocol::SessionStatus {
        tcode_protocol::SessionStatus {
            session_id: session.into(),
            title: "Compact panel".into(),
            cwd: cwd.to_path_buf(),
            attachments_dir: cwd.to_path_buf(),
            provider: agent::ProviderKind::ClaudeCode,
            requested_model: None,
            requested_profile_id: None,
            acp_agent_id: None,
            project_id: None,
            queued_messages: Vec::new(),
            review_comment_drafts: Vec::new(),
            terminals: Vec::new(),
            active_terminal_id: None,
            terminal_splits: Vec::new(),
            terminal_contexts: Vec::new(),
            terminal_open: false,
            terminal_height: 240.,
            delivery_in_flight: None,
            activity: tcode_protocol::SessionActivity {
                working: false,
                turn_running: false,
                waiting: false,
                waiting_for_approval: false,
                waiting_for_input: false,
                failed: false,
                unread: false,
                fork: tcode_protocol::ForkAvailability::Available,
                agent: None,
            },
            stopping: false,
            turn_retry: None,
            goal: None,
            native_rewind_blocked: false,
            checkout_blocked: false,
            conversation_read_only: false,
            terminal_limit_reached: false,
            terminal_split_available: false,
            usage: None,
            context_window: None,
            running_turn: None,
            pending_approvals: Vec::new(),
            pending_user_input: None,
            steering_supported: false,
            provider_option_descriptors: Vec::new(),
            provider_option_selections: Vec::new(),
            provider_option_requested_selections: Vec::new(),
            provider_commands: Vec::new(),
            git_branch: None,
            branches: Vec::new(),
            draft: false,
            draft_workspace: Default::default(),
            worktree: None,
            preparing_worktree: false,
            relay_confirmation: None,
            native_rewind_pending: false,
            native_rewind_prefill_available: false,
            model_pending_restart: false,
            options_pending_restart: false,
            injected_tools: None,
            control: None,
        }
    }

    /// Give the shell one selected session whose only net change is a single
    /// unbreakable 400-character line, so the diff panel has real content far
    /// wider than a phone page.
    fn seed_wide_diff(shell: &Entity<AppShell>, host: &MountedShell, cx: &mut VisualTestContext) {
        seed_wide_diff_with(shell, host, cx, Vec::new());
    }

    /// [`seed_wide_diff`], with `extra` events after the turn's changes.
    fn seed_wide_diff_with(
        shell: &Entity<AppShell>,
        host: &MountedShell,
        cx: &mut VisualTestContext,
        extra: Vec<agent::AgentEvent>,
    ) {
        let session = "thread-1";
        let cwd = std::path::Path::new("/tmp/tcode-compact-panel");
        let path = "a/deeply/nested/module/tree/with/an/absurdly/long/file_name.rs";
        let long = "x".repeat(400);
        let diff = format!(
            "diff --git a/{path} b/{path}\n--- a/{path}\n+++ b/{path}\n\
             @@ -1 +1 @@\n-fn old() {{}}\n+fn new_{long}() {{}}\n"
        );
        let events = [
            (
                Topic::SessionStatus {
                    session_id: session.into(),
                },
                ServerEvent::SessionStatusReplaced(Box::new(session_status(session, cwd))),
            ),
            (
                Topic::SessionEvents {
                    session_id: session.into(),
                },
                ServerEvent::SessionSnapshot {
                    total: 0,
                    total_turns: 0,
                    truncated: false,
                    from: 0,
                    end: 2 + extra.len() as u64,
                    records: [
                        agent::AgentEvent::TurnStarted {
                            turn_id: "turn-1".into(),
                        },
                        agent::AgentEvent::TurnChangesUpdated {
                            turn_id: "turn-1".into(),
                            changes: agent::file_changes_from_unified_diff(&diff)
                                .expect("one file section"),
                            completeness: agent::ChangeCompleteness::Exact,
                        },
                    ]
                    .into_iter()
                    .chain(extra)
                    .map(Into::into)
                    .collect(),
                },
            ),
        ];
        for (topic, event) in events {
            host.incoming
                .try_send(
                    encode_line(&HostMessage::Event(EventEnvelope {
                        request_id: None,
                        topic,
                        event,
                    }))
                    .unwrap(),
                )
                .unwrap();
        }
        let store = store_of(shell, cx);
        store.update(cx, |store, _| store.select_session(session.into()));
        cx.run_until_parked();
        store.update(cx, |store, cx| store.drain_host_events_for_test(cx));
    }

    /// A reader scrolled up from a running turn reads still history: the
    /// turn's clock, out of view, no longer invalidates the timeline.
    #[gpui::test]
    fn a_running_turn_out_of_view_leaves_the_timeline_still(cx: &mut TestAppContext) {
        use std::cell::Cell;
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        // With motion reduced the clock is all that moves: the scrollbar's
        // fade after the scroll, timed by the wall clock, draws no frames.
        cx.update(|cx| cx.set_reduce_motion(true));
        let (shell, host, cx) = mount(cx);
        let session = "thread-1";
        let mut status = session_status(session, std::path::Path::new("/tmp/tcode-running"));
        status.activity.working = true;
        status.activity.turn_running = true;
        status.running_turn = Some(tcode_core::session::RunningTurn {
            turn: 19,
            started_at: Some(crate::time::now_millis()),
        });
        let item = |id: String, content: agent::ItemContent| {
            agent::AgentEvent::ItemCompleted(agent::ThreadItem {
                id,
                parent_item_id: None,
                content,
            })
        };
        let answer = "A paragraph of the answer.\n\n".repeat(12);
        let mut records = Vec::new();
        for turn in 0..20 {
            let turn_id = format!("turn-{turn}");
            records.push(agent::AgentEvent::TurnStarted {
                turn_id: turn_id.clone(),
            });
            records.push(item(
                format!("user-{turn}"),
                agent::ItemContent::UserMessage {
                    text: format!("Question {turn}"),
                    context_len: None,
                    attachments: Vec::new(),
                },
            ));
            if turn < 19 {
                records.push(item(
                    format!("assistant-{turn}"),
                    agent::ItemContent::AssistantMessage {
                        text: answer.clone(),
                    },
                ));
                records.push(agent::AgentEvent::TurnCompleted {
                    turn_id,
                    status: agent::TurnStatus::Completed,
                    usage: None,
                });
            }
        }
        let end = records.len() as u64;
        let mut meta = tcode_core::project::SessionMeta::new(
            agent::ProviderKind::ClaudeCode,
            "/tmp/tcode-running".into(),
            None,
        );
        meta.id = session.into();
        for (topic, event) in [
            (
                Topic::Settings,
                ServerEvent::SettingsSnapshot(Default::default()),
            ),
            (
                Topic::Index,
                ServerEvent::IndexSnapshot(IndexSnapshot {
                    summary: Default::default(),
                    sessions: vec![meta],
                    projects: Vec::new(),
                }),
            ),
            (
                Topic::SessionStatus {
                    session_id: session.into(),
                },
                ServerEvent::SessionStatusReplaced(Box::new(status)),
            ),
            (
                Topic::SessionEvents {
                    session_id: session.into(),
                },
                ServerEvent::SessionSnapshot {
                    total: 0,
                    total_turns: 0,
                    truncated: false,
                    from: 0,
                    end,
                    records: records.into_iter().map(Into::into).collect(),
                },
            ),
        ] {
            host.incoming
                .try_send(
                    encode_line(&HostMessage::Event(EventEnvelope {
                        request_id: None,
                        topic,
                        event,
                    }))
                    .unwrap(),
                )
                .unwrap();
        }
        let store = store_of(&shell, cx);
        store.update(cx, |store, _| store.select_session(session.into()));
        await_restore_update(&shell, cx, |store| {
            store.composer_state().turn_running && !store.chat_loading()
        });
        resize(cx, 1024.);
        assert!(cx.debug_bounds("working-status-19").is_some());

        let timeline = cx.debug_bounds("timeline-row-0").map_or_else(
            || cx.debug_bounds("chat-header-scroll").unwrap().bottom() + px(200.),
            |row| row.center().y,
        );
        cx.simulate_event(gpui::ScrollWheelEvent {
            position: gpui::point(px(600.), timeline),
            delta: gpui::ScrollDelta::Pixels(gpui::point(px(0.), px(20_000.))),
            ..Default::default()
        });
        // Frames as the platform delivers them, each after a clock tick.
        let frame = |cx: &mut VisualTestContext| {
            cx.executor().advance_clock(Duration::from_millis(100));
            cx.run_until_parked();
            cx.update(|window, cx| {
                window.simulate_next_frame(cx);
                let _ = window.draw(cx);
            });
        };
        // The scroll's frame, and the one after it settling what it laid out.
        frame(cx);
        frame(cx);
        draw(cx);
        assert!(cx.debug_bounds("working-status-19").is_none());

        let chat = shell.read_with(cx, |shell, _| {
            shell.attachment.as_ref().expect("attached").chat.clone()
        });
        let invalidations = Rc::new(Cell::new(0));
        let _observer = cx.update(|_, cx| {
            let invalidations = invalidations.clone();
            cx.observe(&chat, move |_, _| {
                invalidations.set(invalidations.get() + 1)
            })
        });
        for _ in 0..5 {
            frame(cx);
        }
        assert_eq!(invalidations.get(), 0, "the clock invalidated the timeline");
    }

    #[gpui::test]
    fn switching_language_retitles_the_window(cx: &mut TestAppContext) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        crate::settings::apply_locale(Some(crate::LANGUAGE_SIMPLIFIED_CHINESE));
        let (shell, host, cx) = mount(cx);
        let session = "draft-1";
        let mut status = session_status(session, std::path::Path::new("/tmp/tcode-title"));
        status.draft = true;
        host.incoming
            .try_send(
                encode_line(&HostMessage::Event(EventEnvelope {
                    request_id: None,
                    topic: Topic::SessionStatus {
                        session_id: session.into(),
                    },
                    event: ServerEvent::SessionStatusReplaced(Box::new(status)),
                }))
                .unwrap(),
            )
            .unwrap();
        let store = store_of(&shell, cx);
        store.update(cx, |store, _| store.select_session(session.into()));
        cx.run_until_parked();
        store.update(cx, |store, cx| store.drain_host_events_for_test(cx));
        crate::settings::assert_follows_language_switch(cx, "chat.new_thread", |cx| {
            cx.window_title().expect("the shell titles its window")
        });
    }

    /// The panel first asks the host for each changed file's current text, then
    /// builds its rendered files off the render thread. This workspace has no
    /// checkout, so refuse those reads — the panel then renders from the stored
    /// patch alone — and draw until `selector` is actually laid out. A
    /// working-tree read is answered with `git_diff` when given; returns how
    /// many the client made.
    fn draw_until(
        shell: &Entity<AppShell>,
        cx: &mut VisualTestContext,
        host: &MountedShell,
        selector: &'static str,
        git_diff: Option<&tcode_protocol::GitDiffResult>,
    ) -> usize {
        let mut git_diff_reads = 0;
        for _ in 0..12 {
            let store = store_of(shell, cx);
            store.update(cx, |store, cx| store.drain_host_events_for_test(cx));
            while let Ok(line) = host.outgoing.try_recv() {
                let line = decode_client_line(&line).expect("client line");
                let ClientPayload::Query(query) = line.payload else {
                    continue;
                };
                let result = match (&query, git_diff) {
                    (tcode_protocol::Query::LoadGitDiff { .. }, Some(diff)) => {
                        git_diff_reads += 1;
                        Ok(tcode_protocol::QueryResponse::GitDiff(diff.clone()))
                    }
                    _ => Err(tcode_protocol::ProtocolError {
                        code: "not_found".into(),
                        message: "no checkout in this test".into(),
                    }),
                };
                host.incoming
                    .try_send(
                        encode_line(&HostMessage::QueryResult {
                            id: line.id,
                            result,
                        })
                        .unwrap(),
                    )
                    .unwrap();
            }
            cx.executor()
                .advance_clock(std::time::Duration::from_millis(100));
            draw(cx);
            if cx.debug_bounds(selector).is_some() {
                return git_diff_reads;
            }
        }
        panic!("{selector} never laid out");
    }

    /// The compact Panel page owns the chrome: its segmented control is the only
    /// selector, so the diff panel's own tab row and its right-panel window
    /// controls are not built at all — and the page holds its 16pt inset even
    /// when the diff line is far wider than the window.
    #[gpui::test]
    fn diff_panel_controls_follow_layout_and_long_lines_stay_inside_the_compact_page(
        cx: &mut TestAppContext,
    ) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        let (shell, host, cx) = mount(cx);
        seed_wide_diff(&shell, &host, cx);
        resize(cx, 1024.);
        store_of(&shell, cx).update(cx, |store, cx| store.toggle_diff_panel(cx));
        draw_until(&shell, cx, &host, "diff-body", None);
        assert!(cx.debug_bounds("right-panel-tabs").is_some());
        assert!(cx.debug_bounds("diff-close").is_some());
        as_mobile(cx);
        resize(cx, 393.);
        shell.update(cx, |shell, cx| shell.open_panels(cx));
        cx.executor().advance_clock(Duration::from_millis(250));
        draw_until(&shell, cx, &host, "diff-body", None);

        assert!(
            cx.debug_bounds("right-panel-tabs").is_none(),
            "the Diff | Tasks tab row is the wide layout's selector"
        );
        assert!(
            cx.debug_bounds("diff-close").is_none(),
            "close is a right-panel affordance; Back leaves the compact page"
        );

        let body = cx
            .debug_bounds("diff-body")
            .expect("the diff body laid out");
        assert_eq!(
            body.left(),
            px(crate::material::COMPACT_PAGE_INSET),
            "the body starts at the page inset"
        );
        assert_eq!(
            body.right(),
            px(393. - crate::material::COMPACT_PAGE_INSET),
            "and ends at it: a 400-character line scrolls inside the body \
             instead of running off the page"
        );
    }

    /// An agent's thread with every right-panel tab, two-digit counts on Agents
    /// and Pull requests, the longest git label, all five header toggles and a
    /// link back to a parent whose title is longer than the link shows.
    fn mount_crowded_thread(
        cx: &mut TestAppContext,
    ) -> (Entity<AppShell>, MountedShell, &mut VisualTestContext) {
        let (shell, host, cx) = mount(cx);
        cx.update(|_, cx| crate::window_seam::override_mobile_for_test(cx, false));
        let mut meta = tcode_core::project::SessionMeta::new(
            agent::ProviderKind::ClaudeCode,
            "/tmp/tcode-compact-panel".into(),
            None,
        );
        let mut parent = tcode_core::project::SessionMeta::new(
            agent::ProviderKind::ClaudeCode,
            "/tmp/tcode-compact-panel".into(),
            None,
        );
        parent.id = "thread-0".into();
        parent.title = "Plan the checkout service retry and timeout policy".into();
        meta.id = "thread-1".into();
        meta.parent_session_id = Some(parent.id.clone());
        meta.pull_requests = (1..=10)
            .map(|number| tcode_core::pull_request::ThreadPullRequestLink {
                key: tcode_core::pull_request::PullRequestKey {
                    host: "github.com".into(),
                    repository: "acme/api".into(),
                    number,
                },
                source: tcode_core::pull_request::PullRequestSource::Manual,
                url: String::new(),
                linked_at: None,
                snapshot: None,
                stack: Default::default(),
                sync_error: None,
                watch: None,
            })
            .collect();
        let git = tcode_core::git::GitStatus {
            is_repo: true,
            has_commits: true,
            has_upstream: true,
            has_working_tree_changes: true,
            ..Default::default()
        };
        for (topic, event) in [
            (
                Topic::Settings,
                ServerEvent::SettingsSnapshot(Default::default()),
            ),
            (
                Topic::Index,
                ServerEvent::IndexSnapshot(IndexSnapshot {
                    summary: Default::default(),
                    sessions: vec![parent, meta],
                    projects: Vec::new(),
                }),
            ),
            (
                Topic::GitStatus {
                    session_id: "thread-1".into(),
                },
                ServerEvent::GitStatusReplaced(tcode_protocol::GitStatusStatus {
                    status: Some(git),
                    busy: false,
                }),
            ),
        ] {
            host.incoming
                .try_send(
                    encode_line(&HostMessage::Event(EventEnvelope {
                        request_id: None,
                        topic,
                        event,
                    }))
                    .unwrap(),
                )
                .unwrap();
        }
        let agents = (0..10)
            .map(|index| {
                agent::AgentEvent::ItemStarted(agent::ThreadItem {
                    id: format!("agent-{index}"),
                    parent_item_id: None,
                    content: agent::ItemContent::Subagent {
                        agent_type: "explore".into(),
                        description: format!("Survey module {index}"),
                        status: agent::ItemStatus::InProgress,
                        summary: None,
                        model: None,
                        effort: None,
                    },
                })
            })
            .collect();
        seed_wide_diff_with(&shell, &host, cx, agents);
        (shell, host, cx)
    }

    /// The right panel at the widths a desktop window passes through, on a
    /// thread whose header and tab strip are as full as they get, with the
    /// sidebar open and collapsed. Docked, the panel is the width the user chose,
    /// less whatever the chat needs for its minimum — more while the header
    /// clears the macOS traffic lights or links to its parent — and it comes
    /// back to that width after the window narrows and widens again, after it
    /// floated, after a sidebar drag squeezed it, and after it was expanded to
    /// everything beside the sidebar and dragged there; the header's last
    /// toggle ends before the panel starts. At every width no tab label is cut
    /// off and the close button is on screen and closes the panel.
    #[gpui::test]
    fn the_right_panel_keeps_its_width_and_leaves_the_chat_header_clear(cx: &mut TestAppContext) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        let (shell, host, cx) = mount_crowded_thread(cx);
        let store = store_of(&shell, cx);
        let window_state = shell.read_with(cx, |shell, _| shell.window_state());
        resize(cx, 1600.);
        store.update(cx, |store, cx| store.toggle_diff_panel(cx));
        draw_until(&shell, cx, &host, "diff-body", None);
        assert!(
            cx.debug_bounds("agent-parent-link").is_some(),
            "the header links back to the parent"
        );

        let walk = |cx: &mut VisualTestContext, widths: &[f32]| {
            for &width in widths {
                resize(cx, width);
                draw_until(&shell, cx, &host, "diff-body", None);
                draw(cx);
                let (sidebar, collapsed) = shell.read_with(cx, |shell, cx| {
                    let attachment = shell.attachment.as_ref().expect("attached");
                    let collapsed = shell.window_state.read(cx).sidebar_collapsed;
                    let sidebar = if collapsed {
                        0.
                    } else {
                        f32::from(attachment.sidebar_width.get())
                    };
                    (sidebar, collapsed)
                });
                let chat_min = CHAT_MIN_WIDTH
                    + store.read_with(cx, |store, _| {
                        crate::chat::header_extra_width(store, collapsed, false, false)
                    });
                let beside = width - sidebar;
                if beside >= chat_min + RIGHT_PANEL_MIN_WIDTH {
                    let panel = cx.debug_bounds("right-panel").expect("the docked panel");
                    assert_eq!(
                        panel.size.width,
                        px(RIGHT_PANEL_WIDTH.min(beside - chat_min)),
                        "{width}pt, sidebar {sidebar}pt"
                    );
                    let toggle = cx
                        .debug_bounds("diff-panel")
                        .expect("the header's diff toggle");
                    assert!(
                        toggle.right() <= panel.left(),
                        "{width}pt, sidebar {sidebar}pt: the header ends at {:?}, \
                         under a panel starting at {:?}",
                        toggle.right(),
                        panel.left()
                    );
                } else {
                    assert!(
                        cx.debug_bounds("right-panel-overlay").is_some(),
                        "{width}pt, sidebar {sidebar}pt: the panel floats"
                    );
                }
                let tabs = cx.debug_bounds("right-panel-tabs").expect("the tab list");
                for tab in ["diff-tab", "plan-tab", "agents-tab", "pull-requests-tab"] {
                    let bounds = cx.debug_bounds(tab).expect(tab);
                    assert!(
                        bounds.right() <= tabs.right(),
                        "{width}pt: {tab} ends at {:?}, past the tab list's {:?}",
                        bounds.right(),
                        tabs.right()
                    );
                }
                let close = cx.debug_bounds("diff-close").expect("the close button");
                assert!(close.right() <= px(width), "{width}pt: close at {close:?}");
            }
        };

        walk(cx, &[1600., 1200., 1600., 1140., 720., 1600., 1200.]);

        resize(cx, 900.);
        draw_until(&shell, cx, &host, "diff-body", None);
        draw(cx);
        let overlay = cx.debug_bounds("right-panel-overlay").unwrap();
        let from = gpui::point(overlay.left(), px(400.));
        let to = from + gpui::point(px(60.), px(0.));
        cx.simulate_mouse_down(from, MouseButton::Left, gpui::Modifiers::default());
        for step in [from + gpui::point(px(5.), px(0.)), to] {
            cx.simulate_mouse_move(step, Some(MouseButton::Left), gpui::Modifiers::default());
            draw(cx);
        }
        cx.simulate_mouse_up(to, MouseButton::Left, gpui::Modifiers::default());
        draw(cx);
        let resized = cx.debug_bounds("right-panel-overlay").unwrap().size.width;
        assert!(
            resized < overlay.size.width,
            "the floating edge shrinks the panel"
        );
        resize(cx, 1600.);
        draw_until(&shell, cx, &host, "diff-body", None);
        draw(cx);
        assert_eq!(
            cx.debug_bounds("right-panel").unwrap().size.width,
            resized,
            "docking remembers the width chosen while floating"
        );
        // Return to the default for the remaining width walk.
        shell.update(cx, |shell, _| {
            let attachment = shell.attachment.as_mut().unwrap();
            attachment.right_width.set(px(RIGHT_PANEL_WIDTH));
            attachment.right_restore_pending = true;
        });
        resize(cx, 1200.);
        draw(cx);
        draw(cx);

        // At 1200pt the chat's minimum already holds the panel below the width
        // the user chose; a sidebar drag squeezes it further, but only a drag
        // of the panel's own handle chooses a width.
        let from = gpui::point(px(SIDEBAR_WIDTH), px(400.));
        let to = gpui::point(px(SIDEBAR_WIDTH + 40.), px(400.));
        // As a pointer does: the drag starts on the first move and moves the
        // handle on the next, and the release listener exists only while a
        // resize is under way, so each step gets its frame.
        cx.simulate_mouse_down(from, MouseButton::Left, gpui::Modifiers::default());
        for step in [from + gpui::point(px(5.), px(0.)), to] {
            cx.simulate_mouse_move(step, Some(MouseButton::Left), gpui::Modifiers::default());
            draw(cx);
        }
        cx.simulate_mouse_up(to, MouseButton::Left, gpui::Modifiers::default());
        draw(cx);
        assert!(
            shell.read_with(cx, |shell, _| {
                shell
                    .attachment
                    .as_ref()
                    .expect("attached")
                    .sidebar_width
                    .get()
            }) > px(SIDEBAR_WIDTH),
            "the drag moved the sidebar"
        );
        walk(cx, &[1600.]);

        // Expanded, the panel takes everything beside the sidebar, however
        // wide the window; restored, it is back at the width the user chose.
        store.update(cx, |store, cx| store.toggle_diff_expanded(cx));
        for width in [1600., 2000.] {
            resize(cx, width);
            draw_until(&shell, cx, &host, "diff-body", None);
            draw(cx);
            let sidebar = shell.read_with(cx, |shell, _| {
                shell
                    .attachment
                    .as_ref()
                    .expect("attached")
                    .sidebar_width
                    .get()
            });
            let panel = cx.debug_bounds("right-panel").expect("the expanded panel");
            assert_eq!(
                (panel.left(), panel.right()),
                (sidebar, px(width)),
                "{width}pt: the expanded panel spans the window beside the sidebar"
            );
        }
        // Drags on the expanded panel's left edge do not choose a width for
        // the docked panel.
        for _ in 0..2 {
            let edge = cx.debug_bounds("right-panel").expect("the expanded panel");
            let from = gpui::point(edge.left(), px(400.));
            let to = gpui::point(edge.left() + px(12.), px(400.));
            cx.simulate_mouse_down(from, MouseButton::Left, gpui::Modifiers::default());
            for step in [from + gpui::point(px(5.), px(0.)), to] {
                cx.simulate_mouse_move(step, Some(MouseButton::Left), gpui::Modifiers::default());
                draw(cx);
            }
            cx.simulate_mouse_up(to, MouseButton::Left, gpui::Modifiers::default());
            draw(cx);
        }
        store.update(cx, |store, cx| store.toggle_diff_expanded(cx));
        walk(cx, &[1600.]);

        window_state.update(cx, |state, cx| state.toggle_sidebar_collapsed(&store, cx));
        walk(cx, &[1600., 1250., 1100., 1000., 900., 860., 720., 1600.]);

        resize(cx, 720.);
        draw_until(&shell, cx, &host, "diff-body", None);
        let close = cx.debug_bounds("diff-close").expect("the close button");
        cx.simulate_click(close.center(), gpui::Modifiers::default());
        draw(cx);
        assert!(
            !store.read_with(cx, |store, _| store.panel_state().right_panel_open),
            "nothing covers the close button"
        );
    }

    #[gpui::test]
    fn floating_panel_takes_focus_and_returns_it_on_close(cx: &mut TestAppContext) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        let (shell, host, cx) = mount_crowded_thread(cx);
        let store = store_of(&shell, cx);
        resize(cx, 1600.);
        let input = shell.read_with(cx, |shell, cx| {
            shell
                .attachment
                .as_ref()
                .unwrap()
                .chat
                .read(cx)
                .composer()
                .read(cx)
                .input_focus_handle(cx)
        });
        cx.update(|window, cx| input.focus(window, cx));
        store.update(cx, |store, cx| store.toggle_diff_panel(cx));
        draw_until(&shell, cx, &host, "diff-body", None);
        resize(cx, 720.);
        draw(cx);
        cx.update(|window, _| {
            assert!(
                !input.is_focused(window),
                "the covered composer cannot keep focus"
            )
        });
        let close = cx.debug_bounds("diff-close").unwrap();
        cx.simulate_click(close.center(), gpui::Modifiers::default());
        draw(cx);
        cx.update(|window, _| assert!(input.is_focused(window), "closing returns to the composer"));
    }

    #[gpui::test]
    fn narrow_chat_header_scrolls_to_its_panel_controls(cx: &mut TestAppContext) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        let (shell, host, cx) = mount_crowded_thread(cx);
        let store = store_of(&shell, cx);
        resize(cx, 720.);
        draw(cx);
        draw(cx);
        let header = cx.debug_bounds("chat-header-scroll").unwrap();
        cx.simulate_event(gpui::ScrollWheelEvent {
            position: header.center(),
            delta: gpui::ScrollDelta::Pixels(gpui::point(px(-1000.), px(0.))),
            ..Default::default()
        });
        draw(cx);
        let toggle = cx.debug_bounds("diff-panel").unwrap();
        assert!(
            toggle.left() >= header.left() && toggle.right() <= header.right(),
            "the last panel toggle must scroll into view"
        );
        cx.simulate_click(toggle.center(), gpui::Modifiers::default());
        draw_until(&shell, cx, &host, "diff-body", None);
        assert!(store.read_with(cx, |store, _| store.panel_state().right_panel_open));
    }

    #[gpui::test]
    fn escape_closes_the_floating_panel(cx: &mut TestAppContext) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        let (shell, host, cx) = mount_crowded_thread(cx);
        let store = store_of(&shell, cx);
        resize(cx, 720.);
        store.update(cx, |store, cx| store.toggle_diff_panel(cx));
        draw_until(&shell, cx, &host, "diff-body", None);
        cx.simulate_keystrokes("escape");
        draw(cx);
        assert!(!store.read_with(cx, |store, _| store.panel_state().right_panel_open));
        assert!(cx.debug_bounds("right-panel-overlay").is_none());
    }

    /// Expand and Restore lay the right panel out again without remounting it:
    /// what its elements keep by their place in the tree — the focus a press
    /// gives the Expand button, the scroll of a list — is still theirs after.
    #[gpui::test]
    fn expanding_the_right_panel_keeps_its_elements_state(cx: &mut TestAppContext) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        let (shell, host, cx) = mount_crowded_thread(cx);
        let store = store_of(&shell, cx);
        resize(cx, 1600.);
        store.update(cx, |store, cx| store.toggle_diff_panel(cx));
        draw_until(&shell, cx, &host, "diff-body", None);

        let press_expand = |cx: &mut VisualTestContext| {
            let expand = cx.debug_bounds("diff-expand").expect("the expand button");
            cx.simulate_click(expand.center(), gpui::Modifiers::default());
            draw(cx);
            draw(cx);
            cx.update(|window, cx| window.focused(cx))
        };
        let expanded = press_expand(cx);
        assert!(
            store.read_with(cx, |store, _| store.panel_state().right_panel_expanded),
            "the press expands the panel"
        );
        assert!(
            expanded.is_some(),
            "the expanded panel's button keeps its focus"
        );
        let restored = press_expand(cx);
        assert!(
            !store.read_with(cx, |store, _| store.panel_state().right_panel_expanded),
            "the second press restores it"
        );
        assert_eq!(
            restored, expanded,
            "the restored panel's button is the same one"
        );
    }

    /// The working tree changes outside any turn — the user edits a file while
    /// another tab is showing. Choosing the Diff tab reads the tree again, as
    /// opening the panel does, and a tree with nothing to show draws the empty
    /// state rather than an empty list.
    #[gpui::test]
    fn diff_tab_reads_the_working_tree_again_when_chosen(cx: &mut TestAppContext) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        let (shell, host, cx) = mount(cx);
        open_working_tree_panel(&shell, &host, cx);
        let store = store_of(&shell, cx);
        let clean = tcode_protocol::GitDiffResult::default();
        assert_eq!(draw_until(&shell, cx, &host, "diff-empty", Some(&clean)), 1);
        assert!(
            cx.debug_bounds("diff-body").is_none(),
            "a clean tree draws the empty state, not an empty list"
        );

        store.update(cx, |store, cx| {
            store.set_right_tab(RightTab::PullRequests, cx)
        });
        draw(cx);
        let changed = tcode_protocol::GitDiffResult {
            changes: vec![agent::FileChange {
                path: "README.md".into(),
                kind: agent::FileChangeKind::Modify,
                diff: Some("@@ -1 +1,2 @@\n # acme-api\n+Checkout service.\n".into()),
            }],
            texts: vec![tcode_protocol::GitFileText {
                old: Some("# acme-api\n".into()),
                new: Some("# acme-api\nCheckout service.\n".into()),
            }],
            ..Default::default()
        };
        store.update(cx, |store, cx| store.set_right_tab(RightTab::Diff, cx));
        assert_eq!(
            draw_until(&shell, cx, &host, "diff-body", Some(&changed)),
            1,
            "choosing the Diff tab reads the working tree again and lists the changed file"
        );
    }

    /// A thread whose turn is running, with the diff panel open on its working tree.
    fn open_working_tree_panel(
        shell: &Entity<AppShell>,
        host: &MountedShell,
        cx: &mut VisualTestContext,
    ) {
        let session = "thread-1";
        let cwd = std::path::Path::new("/tmp/tcode-working-tree");
        let events = [
            (
                Topic::SessionStatus {
                    session_id: session.into(),
                },
                ServerEvent::SessionStatusReplaced(Box::new(session_status(session, cwd))),
            ),
            (
                Topic::SessionEvents {
                    session_id: session.into(),
                },
                ServerEvent::SessionSnapshot {
                    total: 0,
                    total_turns: 0,
                    truncated: false,
                    from: 0,
                    end: 1,
                    records: vec![
                        agent::AgentEvent::TurnStarted {
                            turn_id: "turn-1".into(),
                        }
                        .into(),
                    ],
                },
            ),
        ];
        for (topic, event) in events {
            host.incoming
                .try_send(
                    encode_line(&HostMessage::Event(EventEnvelope {
                        request_id: None,
                        topic,
                        event,
                    }))
                    .unwrap(),
                )
                .unwrap();
        }
        let store = store_of(shell, cx);
        store.update(cx, |store, _| store.select_session(session.into()));
        cx.run_until_parked();
        store.update(cx, |store, cx| store.drain_host_events_for_test(cx));
        resize(cx, 1200.);
        store.update(cx, |store, cx| store.toggle_diff_panel(cx));
    }

    /// Lets the client run and draw while the host holds every working-tree read; any other
    /// query is refused, as a workspace without a checkout would. Returns the held reads in
    /// the order the client made them.
    fn held_git_diff_reads(
        shell: &Entity<AppShell>,
        host: &MountedShell,
        cx: &mut VisualTestContext,
    ) -> Vec<u64> {
        let mut reads = Vec::new();
        for _ in 0..6 {
            store_of(shell, cx).update(cx, |store, cx| store.drain_host_events_for_test(cx));
            while let Ok(line) = host.outgoing.try_recv() {
                let line = decode_client_line(&line).expect("client line");
                let ClientPayload::Query(query) = line.payload else {
                    continue;
                };
                if matches!(query, tcode_protocol::Query::LoadGitDiff { .. }) {
                    reads.push(line.id);
                    continue;
                }
                answer_query(
                    host,
                    line.id,
                    Err(tcode_protocol::ProtocolError {
                        code: "not_found".into(),
                        message: "no checkout in this test".into(),
                    }),
                );
            }
            cx.executor()
                .advance_clock(std::time::Duration::from_millis(100));
            draw(cx);
        }
        reads
    }

    fn answer_query(
        host: &MountedShell,
        id: u64,
        result: Result<tcode_protocol::QueryResponse, tcode_protocol::ProtocolError>,
    ) {
        host.incoming
            .try_send(encode_line(&HostMessage::QueryResult { id, result }).unwrap())
            .unwrap();
    }

    fn answer_git_diff(host: &MountedShell, id: u64, diff: &tcode_protocol::GitDiffResult) {
        answer_query(
            host,
            id,
            Ok(tcode_protocol::QueryResponse::GitDiff(diff.clone())),
        );
    }

    /// A working tree in which each of `files` was created with `lines` lines.
    fn created_files(files: &[(&str, u32)]) -> tcode_protocol::GitDiffResult {
        let (changes, texts) = files
            .iter()
            .map(|&(path, lines)| {
                let text = (1..=lines).map(|line| format!("{path} line {line}\n"));
                let patch = format!(
                    "@@ -0,0 +1,{lines} @@\n{}",
                    text.clone()
                        .map(|line| format!("+{line}"))
                        .collect::<String>()
                );
                (
                    agent::FileChange {
                        path: path.into(),
                        kind: agent::FileChangeKind::Create,
                        diff: Some(patch),
                    },
                    tcode_protocol::GitFileText {
                        old: None,
                        new: Some(text.collect()),
                    },
                )
            })
            .unzip();
        tcode_protocol::GitDiffResult {
            changes,
            texts,
            ..Default::default()
        }
    }

    /// The agent ends a turn, so the working tree is read again.
    fn complete_turn(host: &MountedShell) {
        host.incoming
            .try_send(
                encode_line(&HostMessage::Event(EventEnvelope {
                    request_id: None,
                    topic: Topic::SessionEvents {
                        session_id: "thread-1".into(),
                    },
                    event: ServerEvent::SessionEvent(
                        agent::AgentEvent::TurnCompleted {
                            turn_id: "turn-1".into(),
                            status: agent::TurnStatus::Completed,
                            usage: None,
                        }
                        .into(),
                    ),
                }))
                .unwrap(),
            )
            .unwrap();
    }

    /// While an agent works the tree changes under the reader. Reading it again leaves the
    /// diff they were reading on screen — not a loading state — until the new one is built.
    #[gpui::test]
    fn working_tree_refresh_keeps_the_previous_diff_until_the_new_one_lands(
        cx: &mut TestAppContext,
    ) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        let (shell, host, cx) = mount(cx);
        open_working_tree_panel(&shell, &host, cx);
        let reads = held_git_diff_reads(&shell, &host, cx);
        assert_eq!(reads.len(), 1, "opening the panel reads the working tree");
        answer_git_diff(&host, reads[0], &created_files(&[("first.rs", 3)]));
        assert!(held_git_diff_reads(&shell, &host, cx).is_empty());
        assert!(cx.debug_bounds("diff-line-new-first.rs-1").is_some());

        complete_turn(&host);
        let reads = held_git_diff_reads(&shell, &host, cx);
        assert_eq!(reads.len(), 1, "the finished turn reads the tree again");
        assert!(
            cx.debug_bounds("diff-line-new-first.rs-1").is_some(),
            "the previous diff stays on screen while the host reads the tree"
        );

        answer_git_diff(
            &host,
            reads[0],
            &created_files(&[("first.rs", 3), ("second.rs", 2)]),
        );
        held_git_diff_reads(&shell, &host, cx);
        assert!(
            cx.debug_bounds("diff-line-new-second.rs-1").is_some(),
            "the new diff replaces it once built"
        );
    }

    /// Swapping in the new diff keeps the reader on the line they were reading, even when a
    /// file now listed above it pushes every row down.
    #[gpui::test]
    fn working_tree_refresh_keeps_the_reading_position(cx: &mut TestAppContext) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        let (shell, host, cx) = mount(cx);
        open_working_tree_panel(&shell, &host, cx);
        let reads = held_git_diff_reads(&shell, &host, cx);
        answer_git_diff(&host, reads[0], &created_files(&[("long.rs", 300)]));
        held_git_diff_reads(&shell, &host, cx);

        let body = cx
            .debug_bounds("diff-body")
            .expect("the diff body laid out");
        // A wheel turn at a time, each eased out and drawn, as a reader scrolls.
        for _ in 0..12 {
            cx.simulate_event(gpui::ScrollWheelEvent {
                position: body.center(),
                delta: gpui::ScrollDelta::Pixels(gpui::point(px(0.), px(-100.))),
                ..Default::default()
            });
            for _ in 0..5 {
                cx.executor().advance_clock(Duration::from_millis(100));
                draw(cx);
            }
        }
        // Debug selectors are looked up by `&'static str`.
        let selector =
            |line: u32| -> &'static str { format!("diff-line-new-long.rs-{line}").leak() };
        let top_line = |cx: &mut VisualTestContext| {
            (1..=300).find_map(|line| {
                cx.debug_bounds(selector(line))
                    .filter(|bounds| bounds.top() >= body.top())
                    .map(|bounds| (line, bounds.top()))
            })
        };
        let (line, top) = top_line(cx).expect("a line of long.rs is on screen");
        assert!(line > 20, "the reader scrolled into the file: {line}");

        complete_turn(&host);
        let reads = held_git_diff_reads(&shell, &host, cx);
        answer_git_diff(
            &host,
            reads[0],
            &created_files(&[("added.rs", 20), ("long.rs", 300)]),
        );
        held_git_diff_reads(&shell, &host, cx);
        assert!(
            cx.debug_bounds("diff-line-new-added.rs-1").is_none(),
            "the file listed above is out of view"
        );
        let after = cx
            .debug_bounds(selector(line))
            .expect("the line read before the refresh is still on screen");
        assert!(
            (after.top() - top).abs() < px(1.),
            "it stays where it was: {top:?} then {:?}",
            after.top()
        );
    }

    /// Reads answered out of order: an earlier read the host finishes last is superseded and
    /// never replaces the newer diff, nor what was shown while both were pending.
    #[gpui::test]
    fn working_tree_refresh_ignores_a_superseded_read(cx: &mut TestAppContext) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        let (shell, host, cx) = mount(cx);
        open_working_tree_panel(&shell, &host, cx);
        let reads = held_git_diff_reads(&shell, &host, cx);
        answer_git_diff(&host, reads[0], &created_files(&[("first.rs", 3)]));
        held_git_diff_reads(&shell, &host, cx);

        complete_turn(&host);
        let earlier = held_git_diff_reads(&shell, &host, cx);
        complete_turn(&host);
        let later = held_git_diff_reads(&shell, &host, cx);
        assert_eq!((earlier.len(), later.len()), (1, 1));

        answer_git_diff(&host, earlier[0], &created_files(&[("stale.rs", 3)]));
        held_git_diff_reads(&shell, &host, cx);
        assert!(cx.debug_bounds("diff-line-new-stale.rs-1").is_none());
        assert!(
            cx.debug_bounds("diff-line-new-first.rs-1").is_some(),
            "a superseded read leaves the shown diff in place"
        );

        answer_git_diff(&host, later[0], &created_files(&[("newest.rs", 3)]));
        held_git_diff_reads(&shell, &host, cx);
        assert!(cx.debug_bounds("diff-line-new-newest.rs-1").is_some());
        assert!(cx.debug_bounds("diff-line-new-stale.rs-1").is_none());
    }

    #[test]
    fn sidebar_overlay_follows_hover_only_on_collapsed_workspace_routes() {
        for (current, transition, popover, visible) in [
            (false, SidebarHoverTransition::Trigger(true), false, true),
            (false, SidebarHoverTransition::Trigger(false), false, false),
            (true, SidebarHoverTransition::Trigger(false), false, true),
            (false, SidebarHoverTransition::Overlay(true), false, true),
            (true, SidebarHoverTransition::Overlay(true), false, true),
            (true, SidebarHoverTransition::Overlay(false), false, false),
            (true, SidebarHoverTransition::Overlay(false), true, true),
            (false, SidebarHoverTransition::Overlay(false), true, false),
        ] {
            assert_eq!(
                next_sidebar_overlay_visibility(current, transition, true, Route::Chat, popover),
                visible
            );
            assert!(!next_sidebar_overlay_visibility(
                current,
                transition,
                false,
                Route::Chat,
                popover
            ));
            for route in [Route::Settings, Route::Hosts] {
                assert!(!next_sidebar_overlay_visibility(
                    current, transition, true, route, popover
                ));
            }
        }
    }
}
