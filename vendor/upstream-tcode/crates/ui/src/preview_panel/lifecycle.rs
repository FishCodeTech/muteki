//! Session-keyed ownership of native preview browsers.
//!
//! This module is the lifecycle seam for native children: slots, platform
//! creation, visibility, navigation warmth, key migration, pruning, and
//! teardown all live here. Callers must preserve two ordering facts:
//!
//! - Native children outlive GPUI layout nodes. Before the Preview panel can be
//!   unmounted, call [`BrowserLifecycle::hide_except`] with the key that may
//!   remain mounted (or `None` to hide every child). Only call
//!   [`BrowserLifecycle::set_visible`] after the owning WebView element has been
//!   laid out for the current frame.
//! - lb-wry drops value-operation callbacks until a first navigation has made a
//!   view warm. [`BrowserLifecycle::evaluate_json`] enforces that cold-start
//!   delay. The wait broker uses [`BrowserLifecycle::is_warm`],
//!   [`BrowserLifecycle::mark_warm`], and
//!   [`BrowserLifecycle::evaluate_ready`] to preserve the same ordering across
//!   its repeated probes.

use std::collections::{HashMap, HashSet};
use std::rc::Weak;
use std::time::Duration;

#[cfg(target_os = "android")]
use super::android::WebView;
use gpui::{App, AppContext as _, Context, Entity, Window};
#[cfg(not(target_os = "android"))]
use gpui_webview::WebView;
#[cfg(not(target_os = "android"))]
use preview_mcp::js;
#[cfg(not(target_os = "android"))]
use tcode_protocol::PreviewResponse;

use super::load_error::{self, LoadError};
use super::{ReplyTx, unavailable_message};

const STARTING_MESSAGE: &str = "preview is starting; retry the operation shortly";
const SMOKE_CREATION_QUEUED: &str = "preview creation is queued";
const SMOKE_CREATION_IN_FLIGHT: &str = "preview creation is in flight";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CreationPhase {
    Queued,
    InFlight,
}

#[derive(Clone)]
pub enum Availability {
    Starting(CreationPhase),
    Ready(Entity<WebView>),
    Unavailable,
}

impl Availability {
    /// Harness-facing classification without exposing platform slot internals.
    pub fn is_ready(&self) -> bool {
        matches!(self, Self::Ready(_))
    }

    /// The exact pending status used by the cross-platform lifecycle smoke run.
    pub fn pending_message(&self) -> Option<&'static str> {
        match self {
            Self::Starting(CreationPhase::Queued) => Some(SMOKE_CREATION_QUEUED),
            Self::Starting(CreationPhase::InFlight) => Some(SMOKE_CREATION_IN_FLIGHT),
            Self::Ready(_) | Self::Unavailable => None,
        }
    }

    pub fn is_unavailable(&self) -> bool {
        matches!(self, Self::Unavailable)
    }
}

enum WebViewSlot {
    #[cfg(test)]
    Stub,
    #[cfg(any(target_os = "windows", target_os = "android"))]
    Creating {
        id: u64,
        phase: CreationPhase,
        pending_url: Option<String>,
    },
    Ready {
        view: Entity<WebView>,
        #[cfg(any(target_os = "macos", target_os = "ios"))]
        remote: Option<super::remote::RemoteBrowser>,
    },
}

impl WebViewSlot {
    fn ready(&self) -> Option<&Entity<WebView>> {
        match self {
            #[cfg(any(target_os = "windows", target_os = "android"))]
            Self::Creating { .. } => None,
            #[cfg(test)]
            Self::Stub => None,
            Self::Ready { view, .. } => Some(view),
        }
    }

    fn is_ready(&self) -> bool {
        self.ready().is_some()
    }

    fn availability(&self) -> Availability {
        match self {
            #[cfg(any(target_os = "windows", target_os = "android"))]
            Self::Creating { phase, .. } => Availability::Starting(*phase),
            #[cfg(test)]
            Self::Stub => Availability::Unavailable,
            Self::Ready { view, .. } => Availability::Ready(view.clone()),
        }
    }
}

enum Creator {
    Available(platform::Adapter),
    Unavailable {
        /// Preserve an initialized WebContext for already-ready Windows views.
        adapter: Option<platform::Adapter>,
        error: String,
    },
}

/// The browser lifecycle is an entity so the Windows adapter can complete on
/// GPUI's foreground executor without routing back through `PreviewPanel`.
/// `owner` remains weak deliberately: a completion may install only while the
/// owning panel, the GPUI window, and its exact generation are all still live.
pub struct BrowserLifecycle {
    owner: Weak<()>,
    slots: HashMap<String, WebViewSlot>,
    warm: HashSet<String>,
    active_identity: Option<(String, String)>,
    /// The size the Preview panel last showed a page at: what a conversation
    /// nobody is looking at would get on screen.
    #[cfg(not(target_os = "android"))]
    panel_size: Option<gpui::Size<gpui::Pixels>>,
    creator: Creator,
    proxy: Option<tcode_traverse::preview::ProxyEntry>,
    endpoint: Option<tcode_traverse::preview::PreviewEndpoint>,
    #[cfg(any(target_os = "windows", target_os = "linux", target_os = "android"))]
    _native_proxy: Option<tcode_traverse::preview::NativeProxy>,
}

pub(super) struct KeyReconciliation {
    pub(super) key: Option<String>,
    pub(super) migrated_from: Option<String>,
}

impl BrowserLifecycle {
    pub(super) fn new(
        owner: Weak<()>,
        proxy: Result<Option<crate::store::PreviewTarget>, String>,
    ) -> Self {
        let endpoint = proxy.as_ref().ok().and_then(|target| {
            target.as_ref().map(|(host, tunnels)| {
                tcode_traverse::preview::PreviewEndpoint::new(host, tunnels.clone())
            })
        });
        #[cfg(any(target_os = "windows", target_os = "linux", target_os = "android"))]
        let mut native_proxy = None;
        // The browser engine is pointed at a loopback bridge; macOS and iOS
        // map URLs per route instead and need no entry.
        let proxy: Result<Option<tcode_traverse::preview::ProxyEntry>, String> =
            proxy.and_then(|target| {
                target
                    .map(|_target| {
                        #[cfg(any(
                            target_os = "windows",
                            target_os = "linux",
                            target_os = "android"
                        ))]
                        {
                            let bridge = tcode_traverse::preview::NativeProxy::new(
                                endpoint.clone().unwrap(),
                            )?;
                            let entry = bridge.entry();
                            native_proxy = Some(bridge);
                            Ok(entry)
                        }
                        #[cfg(any(target_os = "macos", target_os = "ios"))]
                        Ok(tcode_traverse::preview::ProxyEntry {
                            origin: String::new(),
                        })
                    })
                    .transpose()
            });
        let creator = match proxy
            .as_ref()
            .map_err(Clone::clone)
            .and_then(|proxy| platform::Adapter::new(proxy.as_ref()))
        {
            Ok(adapter) => Creator::Available(adapter),
            Err(error) => {
                log::warn!("preview: no webview ({error})");
                Creator::Unavailable {
                    adapter: None,
                    error,
                }
            }
        };
        Self {
            owner,
            proxy: proxy.ok().flatten(),
            endpoint,
            #[cfg(any(target_os = "windows", target_os = "linux", target_os = "android"))]
            _native_proxy: native_proxy,
            slots: HashMap::new(),
            warm: HashSet::new(),
            active_identity: None,
            #[cfg(not(target_os = "android"))]
            panel_size: None,
            creator,
        }
    }

    pub(super) fn refresh_endpoint(&self, host: &tcode_client::pairing::PairedHost) {
        if let Some(endpoint) = &self.endpoint
            && let Err(error) = endpoint.update(host)
        {
            log::warn!("preview: could not refresh paired machine address: {error}");
        }
    }

    pub(super) fn active_identity(&self) -> Option<&(String, String)> {
        self.active_identity.as_ref()
    }

    #[cfg(test)]
    pub(crate) fn seed_stub(&mut self, key: &str) {
        self.slots.insert(key.to_owned(), WebViewSlot::Stub);
    }

    #[cfg(test)]
    pub(crate) fn has_view(&self, key: &str) -> bool {
        self.slots.contains_key(key)
    }

    fn owner_is_live(&self) -> bool {
        self.owner.upgrade().is_some()
    }

    /// Get or lazily create the browser for `key`.
    ///
    /// Windows and Android retain the newest URL while asynchronous creation runs.
    /// The synchronous adapter starts at `about:blank`.
    pub fn ensure(
        &mut self,
        key: &str,
        initial_url: Option<&str>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Availability {
        if let Some(slot) = self.slots.get(key) {
            return slot.availability();
        }
        if !self.owner_is_live() || matches!(self.creator, Creator::Unavailable { .. }) {
            return Availability::Unavailable;
        }
        platform::start(self, key, initial_url, window, cx)
    }

    /// Navigate a session and mark it warm only if the native load was accepted.
    /// An asynchronous creation in progress retains the newest requested URL instead.
    pub fn navigate(
        &mut self,
        key: &str,
        url: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Availability {
        let availability = self.ensure(key, Some(url), window, cx);
        match &availability {
            Availability::Ready(webview) => {
                let raw = webview.read(cx).raw();
                // Clear the previous failure now rather than on WebKit's
                // asynchronous didStart, so a wait_for issued right after
                // this navigation cannot fail on the old record.
                load_error::forget(raw);
                #[cfg(any(target_os = "macos", target_os = "ios"))]
                let mapped = match self
                    .remote_browser(key)
                    .map(|remote| remote.navigate(url))
                    .transpose()
                {
                    Ok(mapped) => mapped,
                    Err(_) => {
                        cx.notify();
                        return availability;
                    }
                };
                #[cfg(any(target_os = "macos", target_os = "ios"))]
                let url = mapped.as_deref().unwrap_or(url);
                match raw.load_url(url) {
                    Ok(()) => {
                        self.warm.insert(key.to_string());
                    }
                    Err(error) => {
                        log::warn!("preview: failed to navigate {key}: {error}");
                    }
                }
            }
            #[cfg(any(target_os = "windows", target_os = "android"))]
            Availability::Starting(_) => {
                if let Some(WebViewSlot::Creating { pending_url, .. }) = self.slots.get_mut(key) {
                    *pending_url = Some(url.to_string());
                }
            }
            #[cfg(not(any(target_os = "windows", target_os = "android")))]
            Availability::Starting(_) => {}
            Availability::Unavailable => {}
        }
        availability
    }

    /// Show exactly `key` and hide every other ready native child.
    ///
    /// Call this only after the selected WebView element has been laid out in
    /// the current frame. Pass `None` to hide all children.
    pub fn set_visible(&mut self, key: Option<&str>, cx: &mut Context<Self>) {
        for (candidate, slot) in &self.slots {
            let Some(view) = slot.ready() else {
                continue;
            };
            let visible = Some(candidate.as_str()) == key;
            view.update(cx, |view, _| set_webview_visible(view, visible));
            #[cfg(not(target_os = "android"))]
            if visible {
                let size = view.read(cx).bounds().size;
                if size.width > gpui::px(0.) && size.height > gpui::px(0.) {
                    self.panel_size = Some(size);
                }
            }
        }
    }

    /// Give a hidden browser the viewport it would have on screen: `canvas`
    /// when the conversation fixed one, else the panel's, else `fallback`.
    /// A browser that was never shown is otherwise laid out at 0×0, which
    /// leaves an agent driving a background conversation with an empty page.
    #[cfg(not(target_os = "android"))]
    pub(super) fn stage_hidden(
        &self,
        key: &str,
        canvas: Option<gpui::Size<gpui::Pixels>>,
        fallback: gpui::Size<gpui::Pixels>,
        cx: &App,
    ) {
        let Some(view) = self.ready_view(key) else {
            return;
        };
        let view = view.read(cx);
        if view.visible() {
            return;
        }
        let own = Some(view.bounds().size)
            .filter(|size| size.width > gpui::px(0.) && size.height > gpui::px(0.));
        let size = canvas.or(self.panel_size).or(own).unwrap_or(fallback);
        let rect = wry::Rect {
            position: wry::dpi::LogicalPosition::new(-20000., -20000.).into(),
            size: wry::dpi::LogicalSize::new(f64::from(size.width), f64::from(size.height)).into(),
        };
        if let Err(error) = view.set_bounds(rect) {
            log::debug!("preview: failed to size hidden webview {key}: {error}");
        }
        #[cfg(target_os = "ios")]
        super::ios::set_frame(
            view.raw(),
            gpui::Bounds::new(gpui::point(gpui::px(-20000.), gpui::px(-20000.)), size),
        );
    }

    /// Hide children that cannot remain mounted, without showing a newly
    /// selected child before its GPUI owner has current bounds.
    pub(super) fn hide_except(&mut self, key: Option<&str>, cx: &mut Context<Self>) {
        for (candidate, slot) in &self.slots {
            if Some(candidate.as_str()) == key {
                continue;
            }
            let Some(view) = slot.ready() else {
                continue;
            };
            view.update(cx, |view, _| set_webview_visible(view, false));
        }
    }

    /// Reconcile the physical session with its stable cache key. Draft -> stored
    /// commits retain the physical id, so every cached lifecycle fact moves as
    /// one operation. The returned old key lets chrome preserve an in-progress
    /// address-bar edit while it updates only its mirror identity.
    pub(super) fn reconcile_key(&mut self, current: Option<(String, String)>) -> KeyReconciliation {
        let migration = match (self.active_identity.as_ref(), current.as_ref()) {
            (Some((old_session, old_key)), Some((session, key)))
                if old_session == session && old_key != key =>
            {
                Some((old_key.clone(), key.clone()))
            }
            _ => None,
        };
        let migrated_from = migration.map(|(old_key, key)| {
            self.migrate_key(&old_key, &key);
            old_key
        });
        self.active_identity = current.clone();
        KeyReconciliation {
            key: current.map(|(_, key)| key),
            migrated_from,
        }
    }

    fn migrate_key(&mut self, old_key: &str, key: &str) {
        if let Some(slot) = self.slots.remove(old_key) {
            if self.slots.contains_key(key) {
                drop(slot);
            } else {
                self.slots.insert(key.to_string(), slot);
            }
        }
        if self.warm.remove(old_key) {
            self.warm.insert(key.to_string());
        }
    }

    /// Tear down one ready or in-progress browser generation.
    pub fn drop_view(&mut self, key: &str, cx: &App) {
        self.forget_load_error(key, cx);
        self.slots.remove(key);
        self.warm.remove(key);
    }

    /// Tear down every browser whose session key is no longer live.
    pub fn prune(&mut self, live_keys: &HashSet<String>, cx: &App) {
        let deleted = self
            .slots
            .keys()
            .filter(|key| !live_keys.contains(*key))
            .cloned()
            .collect::<Vec<_>>();
        for key in deleted {
            self.forget_load_error(&key, cx);
            self.slots.remove(&key);
            self.warm.remove(&key);
        }
    }

    /// The last navigation failure the platform reported for this browser, if
    /// the current page is still the one it left behind.
    pub(super) fn load_error(&self, key: &str, cx: &App) -> Option<LoadError> {
        #[cfg(any(target_os = "macos", target_os = "ios"))]
        if let Some(remote) = self.remote_browser(key) {
            let routes = remote.routes.borrow();
            if let Some(message) = routes.error() {
                return Some(LoadError {
                    url: routes.current_url().unwrap_or_default().into(),
                    code: "RemotePreview".into(),
                    message,
                });
            }
        }
        let mut error = load_error::get(self.ready_view(key)?.read(cx).raw())?;
        error.url = self.logical_url(key, &error.url);
        Some(error)
    }

    #[cfg(any(target_os = "macos", target_os = "ios"))]
    fn remote_browser(&self, key: &str) -> Option<&super::remote::RemoteBrowser> {
        match self.slots.get(key)? {
            WebViewSlot::Ready { remote, .. } => remote.as_ref(),
            #[cfg(test)]
            WebViewSlot::Stub => None,
        }
    }

    pub(super) fn logical_url(&self, _key: &str, actual: &str) -> String {
        #[cfg(any(target_os = "macos", target_os = "ios"))]
        if let Some(remote) = self.remote_browser(_key) {
            return remote.routes.borrow().logical_url(actual);
        }
        actual.into()
    }

    #[cfg(any(target_os = "macos", target_os = "ios"))]
    pub(super) fn remote_url(&self, key: &str) -> Option<String> {
        self.remote_browser(key)?
            .routes
            .borrow()
            .current_url()
            .map(str::to_owned)
    }

    #[cfg(any(target_os = "macos", target_os = "ios"))]
    pub(super) fn external_url(&self, key: &str, logical: &str) -> Option<String> {
        match self.remote_browser(key) {
            Some(remote) => remote.routes.borrow().external_url(logical),
            None if self.proxy.is_some() => None,
            None => Some(logical.into()),
        }
    }

    fn forget_load_error(&self, key: &str, cx: &App) {
        if let Some(view) = self.ready_view(key) {
            load_error::forget(view.read(cx).raw());
        }
    }

    pub fn unavailable_error(&self) -> Option<&str> {
        match &self.creator {
            Creator::Available(_) => None,
            Creator::Unavailable { error, .. } => Some(error),
        }
    }

    pub(super) fn ready_view(&self, key: &str) -> Option<Entity<WebView>> {
        self.slots.get(key).and_then(WebViewSlot::ready).cloned()
    }

    pub(super) fn eval_fire(&self, key: &str, script: &str, cx: &Context<Self>) {
        if let Some(view) = self.ready_view(key) {
            let _ = view.read(cx).raw().evaluate_script(script);
        }
    }

    pub(super) fn is_warm(&self, key: &str) -> bool {
        self.warm.contains(key)
    }

    pub(super) fn mark_warm(&mut self, key: &str) {
        self.warm.insert(key.to_string());
    }

    /// Evaluate JSON against a ready, warm browser. Cold browsers wait for the
    /// initial `about:blank` navigation to flush lb-wry's callback queue first.
    pub(super) fn evaluate_json(
        &mut self,
        key: &str,
        initial_url: Option<&str>,
        script: &str,
        reply: ReplyTx,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match self.ensure(key, initial_url, window, cx) {
            Availability::Ready(_) => {}
            Availability::Starting(_) => {
                let _ = reply.try_send(Err(STARTING_MESSAGE.into()));
                return;
            }
            Availability::Unavailable => {
                let error = self.unavailable_error().unwrap_or_default();
                let _ = reply.try_send(Err(unavailable_message(error)));
                return;
            }
        }
        if self.is_warm(key) {
            self.evaluate_ready(key, script, reply, cx);
            return;
        }

        let key = key.to_string();
        let script = script.to_string();
        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(700))
                .await;
            let _ = this.update(cx, |lifecycle, cx| {
                lifecycle.mark_warm(&key);
                lifecycle.evaluate_ready(&key, &script, reply, cx);
            });
        })
        .detach();
    }

    /// Run a value operation after the caller has established warmth.
    pub(super) fn evaluate_ready(
        &self,
        key: &str,
        script: &str,
        reply: ReplyTx,
        cx: &Context<Self>,
    ) {
        let Some(view) = self.ready_view(key) else {
            let _ = reply.try_send(Err("preview browser is not open".into()));
            return;
        };
        #[cfg(target_os = "android")]
        view.read(cx).raw().evaluate_json(script, reply, cx);
        #[cfg(not(target_os = "android"))]
        {
            let result = view.read(cx).raw().evaluate_script_with_callback(script, {
                let reply = reply.clone();
                move |raw: String| {
                    let value = js::parse_result(&raw);
                    let _ = reply.try_send(Ok(PreviewResponse::Json(value)));
                }
            });
            if result.is_err() {
                let _ = reply.try_send(Err("failed to evaluate script in preview".into()));
            }
        }
    }

    fn record_unavailable(&mut self, error: String, cx: &mut Context<Self>) {
        log::warn!("preview: no webview ({error})");
        let previous = std::mem::replace(
            &mut self.creator,
            Creator::Unavailable {
                adapter: None,
                error: error.clone(),
            },
        );
        let adapter = match previous {
            Creator::Available(adapter) => Some(adapter),
            Creator::Unavailable { adapter, .. } => adapter,
        };
        self.creator = Creator::Unavailable { adapter, error };
        // An unavailable platform component invalidates every queued build.
        // Already-ready children remain usable until their normal teardown.
        self.slots.retain(|_, slot| slot.is_ready());
        cx.notify();
    }

    #[cfg(any(target_os = "windows", target_os = "android"))]
    fn has_creation(&self, creation_id: u64) -> bool {
        self.slots.values().any(|slot| {
            matches!(
                slot,
                WebViewSlot::Creating { id, .. } if *id == creation_id
            )
        })
    }

    #[cfg(target_os = "windows")]
    fn mark_creation_in_flight(&mut self, creation_id: u64) -> bool {
        for slot in self.slots.values_mut() {
            if let WebViewSlot::Creating { id, phase, .. } = slot
                && *id == creation_id
            {
                *phase = CreationPhase::InFlight;
                return true;
            }
        }
        false
    }

    #[cfg(any(target_os = "windows", target_os = "android"))]
    fn remove_creation(&mut self, creation_id: u64) -> Option<(String, Option<String>)> {
        let key = self.slots.iter().find_map(|(key, slot)| {
            matches!(
                slot,
                WebViewSlot::Creating { id, .. } if *id == creation_id
            )
            .then(|| key.clone())
        })?;
        let WebViewSlot::Creating { pending_url, .. } = self.slots.remove(&key)? else {
            unreachable!("creation key stopped referring to a creating slot");
        };
        Some((key, pending_url))
    }

    #[cfg(target_os = "windows")]
    fn install_created_webview(
        &mut self,
        key: String,
        pending_url: Option<String>,
        raw: wry::WebView,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        load_error::install(&raw);
        let warm = if let Some(url) = &pending_url {
            match raw.load_url(url) {
                Ok(()) => true,
                Err(error) => {
                    log::warn!("preview: failed to replay URL for {key}: {error}");
                    false
                }
            }
        } else {
            false
        };
        let webview = cx.new(|cx| {
            let mut view = WebView::new(raw, window, cx);
            set_webview_visible(&mut view, false);
            view
        });
        self.slots
            .insert(key.clone(), WebViewSlot::Ready { view: webview });
        if warm {
            self.warm.insert(key);
        }
        cx.notify();
    }
}

#[cfg(not(target_os = "android"))]
fn set_webview_visible(view: &mut WebView, visible: bool) {
    if let Err(error) = view.set_visible(visible) {
        log::debug!("preview: failed to set webview visibility to {visible}: {error}");
    }
}

#[cfg(target_os = "android")]
fn set_webview_visible(view: &mut WebView, visible: bool) {
    if visible {
        view.show();
    } else {
        view.hide();
        if let Err(error) = view.raw().focus_parent() {
            log::debug!("preview: failed to focus parent while hiding webview: {error}");
        }
        if let Err(error) = view.raw().set_visible(false) {
            log::debug!("preview: failed to hide native webview: {error}");
        }
    }
}

#[cfg(not(any(target_os = "windows", target_os = "android")))]
mod platform {
    use super::*;

    pub(super) struct Adapter;

    impl Adapter {
        pub(super) fn new(
            _proxy: Option<&tcode_traverse::preview::ProxyEntry>,
        ) -> Result<Self, String> {
            Ok(Self)
        }
    }

    pub(super) fn start(
        lifecycle: &mut BrowserLifecycle,
        key: &str,
        _initial_url: Option<&str>,
        window: &mut Window,
        cx: &mut Context<BrowserLifecycle>,
    ) -> Availability {
        // Start on about:blank so lb-wry begins a navigation and flushes its
        // pending-scripts buffer, making later evaluation callbacks fire.
        let builder = wry::WebViewBuilder::new()
            .with_devtools(true)
            .with_url("about:blank");
        let builder = match super::super::proxy::builder(builder, lifecycle.proxy.as_ref()) {
            Ok(builder) => builder,
            Err(error) => {
                lifecycle.record_unavailable(error, cx);
                return Availability::Unavailable;
            }
        };
        let built = WebView::build(builder, window, cx);
        let webview = match built {
            Ok(webview) => cx.new(|_| webview),
            Err(error) => {
                lifecycle.record_unavailable(error.to_string(), cx);
                return Availability::Unavailable;
            }
        };
        load_error::install(webview.read(cx).raw());
        #[cfg(target_os = "linux")]
        trap_gdk_x_errors();
        #[cfg(target_os = "ios")]
        super::super::ios::adopt(webview.read(cx).raw());
        webview.update(cx, |view, _| set_webview_visible(view, false));
        #[cfg(any(target_os = "macos", target_os = "ios"))]
        let remote = lifecycle
            .endpoint
            .clone()
            .map(|endpoint| super::super::remote::RemoteBrowser::install(&webview, endpoint, cx));
        lifecycle.slots.insert(
            key.to_string(),
            WebViewSlot::Ready {
                view: webview.clone(),
                #[cfg(any(target_os = "macos", target_os = "ios"))]
                remote,
            },
        );
        Availability::Ready(webview)
    }
}

/// gpui-webview 0.8 releases a webview by destroying its X11 composition
/// surface through GPUI's xcb connection while GDK's Xlib connection still
/// holds unsent requests for the WebKit windows inside it. Those requests then
/// fail with `BadWindow`, which GDK's default handler turns into `exit(1)` —
/// on closing a page, deleting its thread, or quitting. GDK's connection
/// carries only the embedded WebKit windows, so its X errors are trapped for
/// the rest of the process once GTK is up.
#[cfg(target_os = "linux")]
fn trap_gdk_x_errors() {
    static TRAPPED: std::sync::Once = std::sync::Once::new();
    TRAPPED.call_once(gdk::error_trap_push);
}

#[cfg(target_os = "windows")]
mod platform {
    use std::future::Future as _;
    use std::rc::Rc;

    use raw_window_handle::HasWindowHandle as _;

    use super::*;

    const SMOKE_CREATION_PAUSE: Duration = Duration::from_millis(50);

    pub(super) struct Adapter {
        web_context: Rc<async_lock::Mutex<(wry::WebContext, Option<tempfile::TempDir>)>>,
        next_creation_id: u64,
        smoke_creation_pause: bool,
    }

    impl Adapter {
        pub(super) fn new(
            _proxy: Option<&tcode_traverse::preview::ProxyEntry>,
        ) -> Result<Self, String> {
            // Proxy arguments include an attachment-specific loopback port.
            // WebView2 cannot share a user-data directory between environments
            // with different arguments, even while an old creation is finishing.
            let local_profile = crate::client_data_dir()
                .ok_or_else(|| "client data directory was not set at startup".to_string())?
                .join("WebView2");
            let temporary = if _proxy.is_some() {
                Some(
                    tempfile::Builder::new()
                        .prefix("tcode-preview-")
                        .tempdir()
                        .map_err(|error| error.to_string())?,
                )
            } else {
                None
            };
            let user_data_dir = temporary
                .as_ref()
                .map_or(local_profile, |directory| directory.path().to_path_buf());
            std::fs::create_dir_all(&user_data_dir).map_err(|error| {
                format!(
                    "failed to create WebView2 user-data directory {}: {error}",
                    user_data_dir.display()
                )
            })?;
            log::debug!(
                "preview: using WebView2 user-data directory {}",
                user_data_dir.display()
            );
            Ok(Self {
                web_context: Rc::new(async_lock::Mutex::new((
                    wry::WebContext::new(Some(user_data_dir)),
                    temporary,
                ))),
                next_creation_id: 0,
                // Preserve the harness's deterministic in-flight cancellation
                // window without putting smoke routing state back in the panel.
                smoke_creation_pause: std::env::args().any(|arg| arg == "--preview-smoke"),
            })
        }
    }

    pub(super) fn start(
        lifecycle: &mut BrowserLifecycle,
        key: &str,
        initial_url: Option<&str>,
        window: &mut Window,
        cx: &mut Context<BrowserLifecycle>,
    ) -> Availability {
        let (web_context, creation_id, smoke_creation) = match &mut lifecycle.creator {
            Creator::Available(adapter) => {
                adapter.next_creation_id = adapter.next_creation_id.wrapping_add(1).max(1);
                (
                    adapter.web_context.clone(),
                    adapter.next_creation_id,
                    adapter.smoke_creation_pause,
                )
            }
            Creator::Unavailable { .. } => return Availability::Unavailable,
        };
        let parent = match window.window_handle() {
            Ok(handle) => handle.as_raw(),
            Err(error) => {
                lifecycle.record_unavailable(error.to_string(), cx);
                return Availability::Unavailable;
            }
        };

        let proxy = lifecycle.proxy.clone();
        let creation_key = key.to_string();
        lifecycle.slots.insert(
            creation_key.clone(),
            WebViewSlot::Creating {
                id: creation_id,
                phase: CreationPhase::Queued,
                pending_url: initial_url.map(str::to_string),
            },
        );

        cx.spawn_in(window, async move |lifecycle, cx| {
            // WebViewBuilder borrows WebContext mutably for the lifetime of its
            // future. Serialize that borrow without blocking GPUI; a cancelled
            // queued slot is discarded before wry is ever polled.
            let mut web_context = web_context.lock().await;
            let slot_is_live = lifecycle
                .read_with(cx, |lifecycle, _| {
                    lifecycle.owner_is_live() && lifecycle.has_creation(creation_id)
                })
                .unwrap_or(false);
            if !slot_is_live {
                return;
            }
            if cx.update(|_, _| ()).is_err() {
                let _ = lifecycle.update(cx, |lifecycle, cx| {
                    lifecycle.remove_creation(creation_id);
                    cx.notify();
                });
                return;
            }

            // SAFETY: this task is confined to GPUI's UI thread and just
            // revalidated the owning GPUI window without yielding. wry reads
            // the handle synchronously on the first poll, before awaiting
            // WebView2's environment/controller callbacks. The HWND may be
            // destroyed after that await by design; the async path then
            // completes with either an error or a raw child we immediately
            // discard unless the same window/panel/generation are still live.
            let parent = unsafe { raw_window_handle::WindowHandle::borrow_raw(parent) };
            let built = {
                let builder = wry::WebViewBuilder::new_with_web_context(&mut web_context.0)
                    .with_devtools(true)
                    .with_url("about:blank");
                let builder = match super::super::proxy::builder(builder, proxy.as_ref()) {
                    Ok(builder) => builder,
                    Err(error) => { let _ = lifecycle.update(cx, |lifecycle, cx| lifecycle.record_unavailable(error, cx)); return; }
                };
                let mut build = Box::pin(builder.build_as_child_async(&parent));
                let first_poll = std::future::poll_fn(|task_cx| {
                    std::task::Poll::Ready(match build.as_mut().poll(task_cx) {
                        std::task::Poll::Ready(result) => Some(result),
                        std::task::Poll::Pending => None,
                    })
                })
                .await;
                match first_poll {
                    Some(result) => result,
                    None => {
                        let _ = lifecycle.update(cx, |lifecycle, cx| {
                            if lifecycle.mark_creation_in_flight(creation_id) {
                                cx.notify();
                            }
                        });
                        if smoke_creation {
                            cx.background_executor().timer(SMOKE_CREATION_PAUSE).await;
                        }
                        build.as_mut().await
                    }
                }
            };
            drop(web_context);

            match built {
                Ok(raw) => {
                    let mut raw = Some(raw);
                    let installed = matches!(
                        cx.update(|window, app| {
                            lifecycle.update(app, |lifecycle, cx| {
                                if !lifecycle.owner_is_live() {
                                    return false;
                                }
                                let Some((key, pending_url)) =
                                    lifecycle.remove_creation(creation_id)
                                else {
                                    return false;
                                };
                                lifecycle.install_created_webview(
                                    key,
                                    pending_url,
                                    raw.take().expect("raw webview already consumed"),
                                    window,
                                    cx,
                                );
                                true
                            })
                        }),
                        Ok(Ok(true))
                    );
                    if let Some(raw) = raw {
                        drop_raw_webview(raw, &creation_key, "creation cancellation");
                    }
                    if !installed {
                        log::debug!(
                            "preview: discarded stale creation {creation_id} for {creation_key}"
                        );
                    }
                }
                Err(error) => {
                    let error = error.to_string();
                    let recorded = matches!(
                        cx.update(|_, app| {
                            lifecycle.update(app, |lifecycle, cx| {
                                if !lifecycle.owner_is_live()
                                    || !lifecycle.has_creation(creation_id)
                                {
                                    return false;
                                }
                                lifecycle.record_unavailable(error.clone(), cx);
                                true
                            })
                        }),
                        Ok(Ok(true))
                    );
                    if !recorded {
                        log::debug!(
                            "preview: creation {creation_id} for {creation_key} failed after teardown: {error}"
                        );
                    }
                }
            }
        })
        .detach();

        Availability::Starting(CreationPhase::Queued)
    }

    fn drop_raw_webview(raw: wry::WebView, key: &str, reason: &str) {
        load_error::forget(&raw);
        if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(raw))).is_err() {
            log::error!("preview: raw webview drop panicked for {key} after {reason}");
        }
    }
}

#[cfg(target_os = "android")]
mod platform {
    use super::*;
    use futures::StreamExt as _;
    pub(super) struct Adapter {
        next_creation_id: u64,
    }
    impl Adapter {
        pub(super) fn new(
            _proxy: Option<&tcode_traverse::preview::ProxyEntry>,
        ) -> Result<Self, String> {
            Ok(Self {
                next_creation_id: 0,
            })
        }
    }
    pub(super) fn start(
        lifecycle: &mut BrowserLifecycle,
        key: &str,
        initial_url: Option<&str>,
        window: &mut Window,
        cx: &mut Context<BrowserLifecycle>,
    ) -> Availability {
        let initial_url = initial_url.unwrap_or("about:blank").to_owned();
        let (raw, mut events) =
            match super::super::android::RawWebView::new(&initial_url, lifecycle.proxy.as_ref()) {
                Ok(created) => created,
                Err(error) => {
                    lifecycle.record_unavailable(error, cx);
                    return Availability::Unavailable;
                }
            };
        let Creator::Available(adapter) = &mut lifecycle.creator else {
            return Availability::Unavailable;
        };
        adapter.next_creation_id = adapter.next_creation_id.wrapping_add(1).max(1);
        let id = adapter.next_creation_id;
        lifecycle.slots.insert(
            key.to_owned(),
            WebViewSlot::Creating {
                id,
                phase: CreationPhase::Queued,
                pending_url: Some(initial_url.clone()),
            },
        );
        cx.spawn_in(window, async move |this, cx| {
            let created = futures::future::select(
                Box::pin(events.next()),
                Box::pin(cx.background_executor().timer(Duration::from_secs(10))),
            )
            .await;
            let result = match created {
                futures::future::Either::Left((Some(event), _)) if event.kind == 7 => Ok(()),
                futures::future::Either::Left((Some(event), _)) => Err(event.message),
                _ => Err("Android WebView creation timed out or was cancelled".into()),
            };
            let _ = this.update(cx, |lifecycle, cx| {
                if !lifecycle.owner_is_live() || !lifecycle.has_creation(id) {
                    return;
                }
                match result {
                    Err(error) => lifecycle.record_unavailable(error, cx),
                    Ok(()) => {
                        let Some((key, pending_url)) = lifecycle.remove_creation(id) else {
                            return;
                        };
                        if let Some(url) = pending_url
                            && url != initial_url
                        {
                            let _ = raw.load_url(&url);
                        }
                        let view = cx.new(|cx| WebView::new(raw, events, cx));
                        cx.observe(&view, |_, _, cx| cx.notify()).detach();
                        lifecycle.slots.insert(key, WebViewSlot::Ready { view });
                        cx.notify();
                    }
                }
            });
        })
        .detach();
        Availability::Starting(CreationPhase::Queued)
    }
}
