//! The right-panel "Preview" tab.
//!
//! The panel itself is shared by every client: the URL field, open-externally,
//! copy-URL, the per-conversation URL/canvas state and the loading/error copy
//! are ordinary replicated-store UI. What is native is the embedded browser —
//! creating a `gpui-webview` child view, driving its history and JS, and snapshotting
//! it — and that lives behind [`preview_backend`].
//!
//! ## Where a backend exists
//!
//! macOS, Windows, Linux, Android and iOS, and only with the `native-preview`
//! feature (`cfg(preview_backend)`, set by the build script). On Linux
//! `gpui-webview` embeds WebKitGTK as an X11 child and drives GTK from GPUI, so
//! the backend exists only when GPUI runs on X11 (see [`preview_backend`]).
//! Android hosts activity-owned WebViews through JNI; iOS adds a `gpui-webview`
//! child to the GPUI host view and places it itself (see [`ios`]). A client
//! without a backend — the browser, Linux on Wayland — neither offers the tab
//! ([`WorkspaceStore::offers_preview`](crate::store::WorkspaceStore::offers_preview))
//! nor subscribes to the agent's preview requests.
//!
//! Windows creation is deliberately asynchronous. WebView2 construction is
//! asynchronous underneath, but wry's synchronous `build_as_child` waits by
//! running a nested Win32 message pump. That pump can dispatch GPUI teardown
//! while the parent HWND is still underneath the creation call. We instead use
//! `build_as_child_async` on GPUI's window foreground executor and generation-tag
//! each pending child, so teardown can cancel the slot without re-entering GPUI
//! or allowing a stale completion to replace a newer preview. macOS keeps the
//! proven synchronous child-view path.
//!
//! ## Load errors
//!
//! A navigation that fails — an untrusted certificate, a dead port — leaves the
//! previous document on screen, so the JavaScript status probe cannot see it.
//! [`load_error`] observes the platform's own navigation callbacks (WKWebView's
//! delegate, WebView2's `NavigationCompleted`) and keeps the last failure per
//! webview; `preview_status` reports it as `load_error` and `preview_wait_for`
//! fails with it instead of waiting out its timeout.
//!
//! ## Native overlays
//!
//! On macOS, `gpui-webview` uses GPUI Fast composition to put deferred GPUI
//! overlays above the native page. Its element owns document bounds and clipping.
//! GPUI overlays are not reliably painted above the page after a window resize,
//! so the palette and the compact menu still hide the page on every platform.
//! Panel close, tab/conversation switch and route changes
//! hide inactive pages without discarding their loaded documents.

use gpui::{
    AnyElement, AppContext as _, ClipboardItem, Context, Entity, InteractiveElement as _,
    IntoElement, ParentElement as _, Render, Styled as _, Subscription, Window, div,
    prelude::FluentBuilder as _,
};
use gpui_base::v_flex;
use tcode_protocol::PreviewResponse;

use crate::icon::IconName;
use crate::material;
use crate::store::WorkspaceStore;
use crate::widgets::input::{Input, InputEvent, InputState};
use crate::widgets::menu::DropdownMenu as _;
use crate::window_caption;
use crate::window_state::{Route, WindowState};

/// Whether this client can embed a browser. Views ask before offering an action
/// that needs one, so nothing renders an enabled control that cannot work.
///
/// On Linux `gpui-webview` parents WebKitGTK to an X11 window, so a client that
/// GPUI connected to Wayland has none. GPUI picks Wayland whenever
/// `WAYLAND_DISPLAY` names one, and `guess_compositor` reports that same choice;
/// a window cannot change display server once open.
pub fn preview_backend() -> bool {
    #[cfg(all(preview_backend, target_os = "linux"))]
    {
        static ON_X11: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        *ON_X11.get_or_init(|| gpui::GraphicalEnvironment::detect().guess_compositor() == "X11")
    }
    #[cfg(not(all(preview_backend, target_os = "linux")))]
    {
        cfg!(preview_backend)
    }
}

#[cfg(preview_backend)]
pub(crate) mod lifecycle;
#[cfg(preview_backend)]
mod load_error;

/// The reply channel a broker request is answered on.
type ReplyTx = async_channel::Sender<Result<PreviewResponse, String>>;

// Only a build with a backend routes by these; the tests below pin the contract
// on every target so a portable edit cannot quietly change it.
#[cfg_attr(not(preview_backend), allow(dead_code))]
fn visible_preview_key(
    active_key: Option<&str>,
    route: Route,
    palette_open: bool,
    preview_panel_showing: bool,
) -> Option<&str> {
    (route == Route::Chat && !palette_open && preview_panel_showing)
        .then_some(active_key)
        .flatten()
}

/// Resolve an MCP request's physical session id to the stable WebView key.
/// Only the active surface can be an unsent project draft; every background
/// request therefore keys directly by its stored session id.
#[cfg_attr(not(preview_backend), allow(dead_code))]
fn preview_key_for_session(
    requested_session_id: &str,
    active_session_id: Option<&str>,
    active_key: Option<&str>,
) -> String {
    if active_session_id == Some(requested_session_id) {
        active_key.unwrap_or(requested_session_id).to_string()
    } else {
        requested_session_id.to_string()
    }
}

/// What an automation tool answers when the platform webview cannot be created:
/// say so plainly, with the underlying error, rather than leaving the agent to
/// guess why nothing happened.
#[cfg(preview_backend)]
fn unavailable_message(err: &str) -> String {
    format!(
        "the preview browser is unavailable on this machine \
         (the system webview component could not be created: {err})"
    )
}

/// An action on the current preview URL. The compact toolbar reaches these
/// through its overflow menu, which addresses items by action rather than by
/// callback.
#[derive(gpui::Action, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
#[action(namespace = tcode_preview, no_json)]
pub(crate) enum PreviewAction {
    Close,
    CopyUrl,
    OpenExternal,
    ScanPorts,
}

/// Add a scheme to a bare host/port (so `localhost:5173` becomes a real URL).
fn normalize_url(input: &str) -> String {
    let trimmed = input.trim();
    if trimmed.contains("://") || trimmed.starts_with("about:") {
        trimmed.to_string()
    } else {
        format!("http://{trimmed}")
    }
}

pub struct PreviewPanel {
    store: Entity<WorkspaceStore>,
    window_state: Entity<WindowState>,
    /// The shared address-bar input (reflects the active session's URL).
    url_input: Entity<InputState>,
    /// Session id whose URL is currently mirrored into `url_input`.
    mirrored: Option<String>,
    /// The last URL copied into the editor; preserve an unsent edit until the
    /// actual page URL or conversation changes.
    mirrored_url: Option<String>,
    compact_overflow_open: bool,
    #[cfg(preview_backend)]
    /// Discovered localhost dev-server ports (populated by the "Ports" button).
    dev_ports: Vec<u16>,
    #[cfg(preview_backend)]
    /// Discards a completed scan when a newer click has superseded it.
    port_scan_generation: u64,
    #[cfg(preview_backend)]
    backend: backend::Backend,
    _subscriptions: Vec<Subscription>,
}

impl PreviewPanel {
    pub fn new(
        store: Entity<WorkspaceStore>,
        window_state: Entity<WindowState>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let url_input = cx.new(|cx| InputState::new(window, cx));
        let subscriptions = vec![
            cx.observe(&store, |this, _, cx| {
                // Native child views outlive GPUI layout nodes. Visibility
                // therefore follows WorkspaceStore directly, even while this
                // entity is no longer mounted in the right-panel tree.
                this.prune_deleted_webviews(cx);
                this.sync_visibility(cx);
                cx.notify();
            }),
            #[cfg(preview_backend)]
            cx.subscribe(
                &store,
                |this, _, state: &tcode_client::ConnectionState, cx| {
                    if matches!(
                        state,
                        tcode_client::ConnectionState::Syncing { .. }
                            | tcode_client::ConnectionState::Connected { .. }
                    ) {
                        this.refresh_preview_endpoint(cx);
                    }
                },
            ),
            cx.observe(&window_state, |this, _, cx| {
                this.sync_visibility(cx);
                cx.notify();
            }),
            cx.subscribe_in(&url_input, window, Self::on_url_event),
        ];
        let mut panel = Self {
            store: store.clone(),
            window_state,
            url_input,
            mirrored: None,
            mirrored_url: None,
            compact_overflow_open: false,
            #[cfg(preview_backend)]
            dev_ports: Vec::new(),
            #[cfg(preview_backend)]
            port_scan_generation: 0,
            #[cfg(preview_backend)]
            backend: backend::Backend::new(store.read(cx).preview_proxy(), cx),
            _subscriptions: subscriptions,
        };
        panel.observe_backend(cx);
        panel
    }

    /// The stable conversation key the chrome is currently addressing.
    fn active_key(&mut self, cx: &mut Context<Self>) -> Option<String> {
        let current = self.store.read(cx).preview_active_identity();
        self.reconcile_active_key(current, cx)
    }

    /// Mirror a URL into the store, then navigate whatever backend exists.
    fn navigate(&mut self, key: &str, url: &str, window: &mut Window, cx: &mut Context<Self>) {
        let url = normalize_url(url);
        self.store
            .update(cx, |store, cx| store.set_preview_url(key, url.clone(), cx));
        self.navigate_backend(key, &url, window, cx);
        self.sync_visibility(cx);
        cx.notify();
    }

    fn on_url_event(
        &mut self,
        input: &Entity<InputState>,
        event: &InputEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let InputEvent::PressEnter { .. } = event {
            let url = input.read(cx).value().trim().to_string();
            if !url.is_empty()
                && let Some(key) = self.active_key(cx)
            {
                self.navigate(&key, &url, window, cx);
            }
        }
    }

    #[cfg(all(test, preview_backend))]
    pub(crate) fn url_field(&self, cx: &gpui::App) -> String {
        self.url_input.read(cx).value().to_string()
    }

    fn active_url(&mut self, cx: &mut Context<Self>) -> Option<String> {
        let key = self.active_key(cx)?;
        self.store.read(cx).preview_url(&key)
    }

    /// Hand the current URL to the OS browser. `cx.open_url` is gpui's
    /// cross-platform launcher (`open` / `ShellExecute` / `xdg-open`, and the
    /// browser's own `window.open`).
    fn open_in_system_browser(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        #[cfg(all(preview_backend, any(target_os = "macos", target_os = "ios")))]
        let url = self.external_preview_url(cx);
        #[cfg(not(all(preview_backend, any(target_os = "macos", target_os = "ios"))))]
        let url = self.active_url(cx);
        if let Some(url) = url {
            cx.open_url(&url);
        } else {
            use crate::overlay::{Notification, OverlayExt as _};
            window.push_notification(
                Notification::error(crate::tr!("preview.external_unavailable")),
                cx,
            );
        }
    }

    fn copy_url(&mut self, cx: &mut Context<Self>) {
        if let Some(url) = self.active_url(cx) {
            cx.write_to_clipboard(ClipboardItem::new_string(url));
        }
    }

    /// Release the conversation's browser and chrome without changing layout.
    /// Desktop close additionally collapses the panel; compact close stays at URL entry.
    pub(crate) fn release_preview(&mut self, cx: &mut Context<Self>) {
        if let Some(key) = self.active_key(cx) {
            self.drop_webview(&key, cx);
            self.store
                .update(cx, |store, cx| store.clear_preview_chrome(&key, cx));
        }
        // Un-mirror so a later reopen refreshes the address bar from the
        // (now empty) URL map instead of showing the stale address.
        self.mirrored = None;
        cx.notify();
    }

    fn close_panel(&mut self, cx: &mut Context<Self>) {
        self.release_preview(cx);
        self.store
            .update(cx, |store, cx| store.close_preview_panel(cx));
        cx.notify();
    }

    fn render_chrome(&self, window: &Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Windows: an open Preview tab is the rightmost column, so its
        // chrome row is a top strip hosting the caption buttons, which reach
        // the window's true top-right corner.
        let hosts_caption = {
            let (diff_open, right_tab) = self.store.read(cx).window_caption_state();
            window_caption::hosts_caption_for_state(
                window_caption::CaptionSurface::Preview,
                self.window_state.read(cx).route(),
                diff_open,
                right_tab,
            )
        };
        // Port discovery scans *this* machine's listeners. Over a remote link
        // those ports belong to the wrong computer, so the affordance is hidden
        // rather than offering the user a list of their own dev servers as if
        // they were the host's. A host URL typed into the field still works.
        let offer_ports = !cfg!(any(target_os = "android", target_os = "ios"))
            && preview_backend()
            && !self.store.read(cx).is_remote();
        // Compact: one toolbar row on the page inset, with 44pt touch targets.
        // Close is a right-panel affordance — the Panel page's Back leaves it —
        // so it is not drawn here at all.
        let compact = self.window_state.read(cx).compact;
        let kind = if hosts_caption {
            material::Bar::Strip
        } else {
            material::Bar::Toolbar
        };
        material::bar("preview-chrome", kind, compact, cx)
            .when(hosts_caption, |chrome| chrome.pr_0())
            // Back / forward / reload drive a page. Without a backend they are
            // absent rather than present-but-dead.
            .children(self.history_controls(cx))
            .child(div().flex_1().min_w_0().child(
                Input::new(&self.url_input).placeholder(crate::tr!("preview.url_placeholder")),
            ))
            // Compact: the address field is the row. Everything that acts *on*
            // the current URL goes into one overflow menu rather than squeezing
            // the field down to a word.
            .when(compact, |chrome| {
                let panel = cx.entity().downgrade();
                chrome.child(
                    material::toolbar_icon_button(
                        "preview-overflow",
                        IconName::Ellipsis,
                        crate::tr!("mobile.more_actions"),
                        true,
                    )
                    .dropdown_menu(move |menu, _, cx| {
                        let _ = panel.update(cx, |panel, cx| {
                            panel.compact_overflow_open = true;
                            panel.sync_visibility(cx);
                            cx.notify();
                        });
                        let panel = panel.clone();
                        cx.on_release(move |_, cx| {
                            let _ = panel.update(cx, |panel, cx| {
                                panel.compact_overflow_open = false;
                                cx.notify();
                            });
                        })
                        .detach();
                        menu.menu(
                            crate::tr!("preview.close_compact").into_owned(),
                            Box::new(PreviewAction::Close),
                        )
                        .menu(
                            crate::tr!("preview.copy_url").into_owned(),
                            Box::new(PreviewAction::CopyUrl),
                        )
                        .menu(
                            crate::tr!("preview.open_external").into_owned(),
                            Box::new(PreviewAction::OpenExternal),
                        )
                        .menu_with_enable(
                            crate::tr!("preview.scan_ports").into_owned(),
                            Box::new(PreviewAction::ScanPorts),
                            offer_ports,
                        )
                    }),
                )
            })
            .when(!compact && offer_ports, |chrome| {
                chrome.child(
                    material::toolbar_icon_button(
                        "preview-ports",
                        IconName::Globe,
                        crate::tr!("preview.scan_ports"),
                        false,
                    )
                    .on_click(cx.listener(|this, _, _, cx| this.rescan_ports(cx))),
                )
            })
            .when(!compact, |chrome| {
                chrome
                    .child(
                        material::toolbar_icon_button(
                            "preview-copy-url",
                            IconName::Copy,
                            crate::tr!("preview.copy_url"),
                            false,
                        )
                        .on_click(cx.listener(|this, _, _, cx| this.copy_url(cx))),
                    )
                    .child(
                        material::toolbar_icon_button(
                            "preview-open-external",
                            IconName::ExternalLink,
                            crate::tr!("preview.open_external"),
                            false,
                        )
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.open_in_system_browser(window, cx)
                        })),
                    )
            })
            .when(!compact, |chrome| {
                chrome.child(
                    material::toolbar_icon_button(
                        "preview-close",
                        IconName::Close,
                        crate::tr!("preview.close"),
                        false,
                    )
                    .on_click(cx.listener(|this, _, _, cx| this.close_panel(cx))),
                )
            })
            .children(hosts_caption.then(|| window_caption::caption_controls(window, cx)))
    }

    pub(crate) fn on_preview_action(
        &mut self,
        action: &PreviewAction,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match action {
            PreviewAction::Close => self.release_preview(cx),
            PreviewAction::CopyUrl => self.copy_url(cx),
            PreviewAction::OpenExternal => self.open_in_system_browser(_window, cx),
            PreviewAction::ScanPorts => self.rescan_ports(cx),
        }
    }

    #[cfg(preview_backend)]
    fn render_note(
        &self,
        icon: IconName,
        title: String,
        detail: Option<String>,
        cx: &Context<Self>,
    ) -> AnyElement {
        material::empty_state(
            crate::icon::Icon::new(icon),
            title,
            detail.unwrap_or_default(),
            cx,
        )
        .into_any_element()
    }
}

impl Render for PreviewPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // When the embedded browser is turned off in Settings → Browser, hide
        // the chrome and webview entirely and show a quiet placeholder.
        if !self.store.read(cx).preview_browser_settings().enabled {
            return v_flex().size_full().child(material::empty_state(
                crate::icon::Icon::new(IconName::EyeOff),
                crate::tr!("browser.disabled_panel"),
                "",
                cx,
            ));
        }
        let active = self.active_key(cx);
        let current_url = active
            .as_ref()
            .and_then(|id| self.store.read(cx).preview_url(id));
        if active != self.mirrored || current_url != self.mirrored_url {
            let value = current_url.clone().unwrap_or_default();
            self.url_input
                .update(cx, |state, cx| state.set_value(&value, window, cx));
            self.mirrored = active.clone();
            self.mirrored_url = current_url;
        }

        let body = self.render_body(active.as_deref(), window, cx);
        v_flex()
            .size_full()
            .on_action(cx.listener(Self::on_preview_action))
            .child(self.render_chrome(window, cx))
            .children(self.render_port_row(cx))
            .child(body)
    }
}

#[cfg(not(preview_backend))]
mod portable {
    use tcode_protocol::PreviewRequest;

    use super::*;

    impl PreviewPanel {
        pub(crate) fn set_compact_panel_selected(
            &mut self,
            _selected: bool,
            _cx: &mut Context<Self>,
        ) {
        }

        pub(super) fn observe_backend(&mut self, _cx: &mut Context<Self>) {}

        pub(super) fn reconcile_active_key(
            &mut self,
            current: Option<(String, String)>,
            _cx: &mut Context<Self>,
        ) -> Option<String> {
            current.map(|(_, key)| key)
        }

        pub(super) fn navigate_backend(
            &mut self,
            _key: &str,
            _url: &str,
            _window: &mut Window,
            _cx: &mut Context<Self>,
        ) {
        }

        pub(super) fn drop_webview(&mut self, key: &str, _cx: &mut Context<Self>) {
            if self.mirrored.as_deref() == Some(key) {
                self.mirrored = None;
            }
        }

        pub(super) fn prune_deleted_webviews(&mut self, cx: &mut Context<Self>) {
            let live = self.store.read(cx).preview_live_keys();
            if self
                .mirrored
                .as_ref()
                .is_some_and(|key| !live.contains(key))
            {
                self.mirrored = None;
            }
        }

        pub(super) fn history_controls(&self, _cx: &mut Context<Self>) -> Vec<AnyElement> {
            Vec::new()
        }

        pub(super) fn render_port_row(&self, _cx: &mut Context<Self>) -> Option<AnyElement> {
            None
        }

        pub(super) fn rescan_ports(&mut self, _cx: &mut Context<Self>) {}

        pub fn sync_visibility(&mut self, _cx: &mut Context<Self>) {}

        /// Unreachable: without a backend the store never offers the tab.
        pub(super) fn render_body(
            &mut self,
            _active: Option<&str>,
            _window: &mut Window,
            _cx: &mut Context<Self>,
        ) -> AnyElement {
            div().into_any_element()
        }

        /// Nothing here can drive a page. Answer immediately and explicitly:
        /// a client that stayed silent would leave the agent's call to time out.
        pub fn handle_op(
            &mut self,
            session_id: String,
            op: PreviewRequest,
            reply: ReplyTx,
            _window: &mut Window,
            _cx: &mut Context<Self>,
        ) {
            log::info!("preview: rejecting op {op:?} for session {session_id} (no backend)");
            // Naming the client stops the agent retrying instead of waiting out
            // a timeout it can never satisfy here.
            let _ = reply.try_send(Err(crate::tr!("preview.unsupported_client").into_owned()));
        }
    }
}

#[cfg(preview_backend)]
mod backend;

#[cfg(test)]
mod tests {
    #[allow(unused_imports)]
    use tcode_protocol::PreviewRequest;

    use super::*;

    /// A client with no embedded browser is still reachable over the preview
    /// topic. It must answer, and say why, rather than let the agent's call sit
    /// until it times out.
    #[cfg(not(preview_backend))]
    #[gpui::test]
    fn a_client_without_a_backend_refuses_preview_requests_immediately(
        cx: &mut gpui::TestAppContext,
    ) {
        use tcode_runtime::pipe::{HostServices, spawn_host};

        let root = crate::store::tests::scratch_root("preview-unsupported");
        let host = spawn_host(
            tcode_services::store::SessionStore::open_at(root.clone()).unwrap(),
            HostServices::default(),
        )
        .expect("spawn preview test host");
        let mut driver = crate::store::tests::LocalHostDriver::new(&host);
        let store = cx.new(|cx| {
            WorkspaceStore::new_attached(
                driver.link.clone(),
                crate::store::WorkspaceAttachment::Local,
                None,
                None,
                false,
                cx,
            )
        });
        driver.wait_until(cx, &store, "workspace baseline", |cx| {
            store.read_with(cx, |store, _| store.baseline_ready())
        });
        let window_state = cx.new(|_| WindowState::new(false));
        let (panel, cx) = cx.add_window_view(|window, cx| {
            PreviewPanel::new(store.clone(), window_state.clone(), window, cx)
        });
        let cx: &mut gpui::VisualTestContext = cx;

        let (reply, answers) = async_channel::bounded(1);
        cx.update(|window, cx| {
            panel.update(cx, |panel, cx| {
                panel.handle_op("session".into(), PreviewRequest::Status, reply, window, cx);
            });
        });
        cx.run_until_parked();

        let answer = answers
            .try_recv()
            .expect("an answer, not a dropped request");
        assert_eq!(
            answer,
            Err(crate::tr!("preview.unsupported_client").into_owned())
        );

        driver.shutdown(cx, &store, &host);
        let _ = std::fs::remove_dir_all(root);
    }

    #[gpui::test]
    fn switching_language_relocalizes_the_address_placeholder(cx: &mut gpui::TestAppContext) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        crate::settings::apply_locale(Some(crate::LANGUAGE_SIMPLIFIED_CHINESE));
        cx.update(crate::theme::init);
        let (to_host, _outgoing) = async_channel::unbounded();
        let (incoming, from_host) = async_channel::unbounded();
        let deferred = std::iter::from_fn(|| from_host.try_recv().ok()).collect();
        let link = tcode_client::HostLink::new(to_host, from_host);
        let store = cx.new(|cx| {
            WorkspaceStore::new_attached(
                link,
                crate::store::WorkspaceAttachment::Local,
                None,
                None,
                false,
                cx,
            )
        });
        crate::store::tests::seed_full_scope(&store, &incoming, deferred, cx);
        let window_state = cx.new(|_| WindowState::new(false));
        let (panel, cx) = cx.add_window_view(|window, cx| {
            PreviewPanel::new(store.clone(), window_state, window, cx)
        });
        crate::settings::assert_follows_language_switch(cx, "preview.url_placeholder", |cx| {
            panel.read_with(cx, |panel, cx| {
                let input = panel.url_input.read(cx);
                input.presentation().placeholder().to_string()
            })
        });
    }

    #[test]
    fn routed_session_uses_active_draft_key_only_for_the_active_surface() {
        assert_eq!(
            preview_key_for_session(
                "physical-draft",
                Some("physical-draft"),
                Some("draft:project-a")
            ),
            "draft:project-a"
        );
        assert_eq!(
            preview_key_for_session(
                "stored-background",
                Some("physical-draft"),
                Some("draft:project-a")
            ),
            "stored-background"
        );
        assert_eq!(
            preview_key_for_session(
                "stored-active",
                Some("stored-active"),
                Some("stored-active")
            ),
            "stored-active"
        );
    }

    #[test]
    fn normalize_url_adds_a_scheme_to_bare_hosts() {
        assert_eq!(normalize_url("localhost:5173"), "http://localhost:5173");
        assert_eq!(normalize_url(" https://x.dev "), "https://x.dev");
        assert_eq!(normalize_url("about:blank"), "about:blank");
    }

    #[test]
    fn native_overlay_is_visible_only_while_preview_owns_it() {
        assert_eq!(
            visible_preview_key(Some("thread-a"), Route::Chat, false, true),
            Some("thread-a")
        );
        assert_eq!(
            visible_preview_key(Some("thread-a"), Route::Chat, false, false),
            None,
            "closing Preview or selecting Diff/Plan must hide the native child"
        );
        assert_eq!(
            visible_preview_key(Some("thread-b"), Route::Chat, true, true),
            None,
            "the command palette must cover the whole workspace"
        );
        assert_eq!(
            visible_preview_key(Some("thread-b"), Route::Settings, false, true),
            None,
            "leaving Chat unmounts the preview layout"
        );
        assert_eq!(visible_preview_key(None, Route::Chat, false, true), None);
    }
}

#[cfg(all(preview_backend, target_os = "android"))]
mod android;
#[cfg(any(test, all(preview_backend, target_os = "android")))]
mod android_geometry;

#[cfg(all(preview_backend, target_os = "ios"))]
mod ios;

#[cfg(all(
    preview_backend,
    any(
        target_os = "macos",
        target_os = "windows",
        target_os = "linux",
        target_os = "ios"
    )
))]
mod proxy;

#[cfg(all(preview_backend, any(target_os = "macos", target_os = "ios")))]
mod remote;
