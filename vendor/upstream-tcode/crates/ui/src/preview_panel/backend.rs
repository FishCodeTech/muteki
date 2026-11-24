//! The embedded-browser half of [`super::PreviewPanel`].
//!
//! Everything here needs a real system webview: child-view creation, history,
//! JS evaluation and snapshots. The panel's chrome, URL state and copy/open
//! actions stay in the parent module and compile everywhere.

use std::rc::Rc;
use std::time::Duration;

use gpui::{
    AnyElement, AppContext as _, Context, IntoElement, ParentElement as _, Styled as _, Window,
    div, px,
};
use gpui_base::v_flex;
use preview_mcp::{js, ports};
use tcode_protocol::{PreviewRequest, PreviewResponse};

use super::lifecycle::{Availability, BrowserLifecycle};
use super::{
    PreviewPanel, ReplyTx, preview_key_for_session, unavailable_message, visible_preview_key,
};
use crate::icon::IconName;
use crate::theme::ActiveTheme as _;
use crate::typography::TextRole as _;

const STARTING_MESSAGE: &str = "preview is starting; retry the operation shortly";

/// The error `preview_screenshot` reports where native-webview snapshots have no
/// implementation.
#[cfg_attr(
    any(target_os = "macos", target_os = "ios", target_os = "android"),
    allow(dead_code)
)]
const SCREENSHOT_UNSUPPORTED: &str =
    "preview_screenshot is only supported on macOS, iOS and Android";

fn wait_timeout_message(pending: &[String]) -> String {
    format!(
        "preview_wait_for timed out; unmet conditions: {}",
        pending.join(", ")
    )
}

pub(super) struct Backend {
    lifecycle: gpui::Entity<BrowserLifecycle>,
    /// The lifecycle holds this only weakly so an async Windows completion
    /// cannot install after its owning panel has been dropped.
    _owner: Rc<()>,
    compact_panel_selected: bool,
}

impl Backend {
    pub(super) fn new(
        proxy: Result<Option<crate::store::PreviewTarget>, String>,
        cx: &mut Context<PreviewPanel>,
    ) -> Self {
        let owner = Rc::new(());
        let lifecycle = cx.new(|_| BrowserLifecycle::new(Rc::downgrade(&owner), proxy));
        Self {
            lifecycle,
            _owner: owner,
            compact_panel_selected: false,
        }
    }
}

impl PreviewPanel {
    pub(super) fn refresh_preview_endpoint(&self, cx: &mut Context<Self>) {
        if let Ok(Some((host, _))) = self.store.read(cx).preview_proxy() {
            self.lifecycle_entity().read(cx).refresh_endpoint(&host);
        }
    }

    /// The compact shell owns which panel segment is mounted, including Terminal
    /// (which does not change the stored right-panel tab).
    pub(crate) fn set_compact_panel_selected(&mut self, selected: bool, cx: &mut Context<Self>) {
        self.backend.compact_panel_selected = selected;
        self.sync_visibility(cx);
    }

    fn lifecycle_entity(&self) -> gpui::Entity<BrowserLifecycle> {
        self.backend.lifecycle.clone()
    }

    #[cfg(any(target_os = "macos", target_os = "ios"))]
    pub(super) fn external_preview_url(&mut self, cx: &mut Context<Self>) -> Option<String> {
        let key = self.active_key(cx)?;
        let logical = self
            .lifecycle_entity()
            .read(cx)
            .remote_url(&key)
            .or_else(|| self.store.read(cx).preview_url(&key))?;
        self.lifecycle_entity()
            .read(cx)
            .external_url(&key, &logical)
    }

    pub(super) fn observe_backend(&mut self, cx: &mut Context<Self>) {
        let lifecycle = self.lifecycle_entity();
        self._subscriptions
            .push(cx.observe(&lifecycle, |this, _, cx| {
                this.sync_visibility(cx);
                cx.notify();
            }));
    }

    /// Reconcile the physical session with its stable cache key. Draft -> stored
    /// commits retain the same session id, so move all cached browser state
    /// across that one key transition.
    pub(super) fn reconcile_active_key(
        &mut self,
        current: Option<(String, String)>,
        cx: &mut Context<Self>,
    ) -> Option<String> {
        let lifecycle = self.lifecycle_entity();
        if self.window_state.read(cx).compact {
            let previous = lifecycle.read(cx).active_identity().cloned();
            if let Some((session, key)) = previous
                && current.as_ref().map(|(id, _)| id) != Some(&session)
            {
                self.drop_webview(&key, cx);
                self.store
                    .update(cx, |store, cx| store.clear_preview_chrome(&key, cx));
            }
        }
        let reconciliation = lifecycle.update(cx, |lifecycle, _| lifecycle.reconcile_key(current));
        if let Some(old_key) = reconciliation.migrated_from.as_deref()
            && self.mirrored.as_deref() == Some(old_key)
        {
            self.mirrored = reconciliation.key.clone();
        }
        reconciliation.key
    }

    pub(crate) fn lifecycle(&self) -> gpui::Entity<BrowserLifecycle> {
        self.backend.lifecycle.clone()
    }

    fn routed_key(&mut self, session_id: &str, cx: &mut Context<Self>) -> String {
        let active_key = self.active_key(cx);
        let active_session_id = self.store.read(cx).active_session_id();
        preview_key_for_session(
            session_id,
            active_session_id.as_deref(),
            active_key.as_deref(),
        )
    }

    /// Hide native children that no longer belong to the visible Preview
    /// panel. This deliberately never shows a child: an opening transition
    /// may still have stale bounds until `render` mounts its GPUI owner.
    /// `AppShell` calls this before it removes Preview from the layout tree.
    pub fn sync_visibility(&mut self, cx: &mut Context<Self>) {
        self.update_visibility(false, cx);
    }

    /// Full show/hide synchronization, called only while `PreviewPanel` is
    /// mounted and has laid out the WebView owner for this frame.
    fn sync_mounted_visibility(&mut self, cx: &mut Context<Self>) {
        self.update_visibility(true, cx);
    }

    fn update_visibility(&mut self, allow_show: bool, cx: &mut Context<Self>) {
        let active = self.active_key(cx);
        let window_state = self.window_state.read(cx);
        let visible = visible_preview_key(
            active.as_deref(),
            window_state.route(),
            window_state.palette_open,
            self.store.read(cx).preview_panel_showing()
                && self.store.read(cx).preview_browser_settings().enabled
                && (!window_state.compact
                    || (!self.compact_overflow_open
                        && self.backend.compact_panel_selected
                        && window_state.destination() == crate::window_state::Destination::Panel)),
        )
        .map(str::to_string);
        self.lifecycle_entity().update(cx, |lifecycle, cx| {
            if allow_show {
                lifecycle.set_visible(visible.as_deref(), cx);
            } else {
                lifecycle.hide_except(visible.as_deref(), cx);
            }
        });
    }

    /// Get or lazily create the browser for one stable conversation key.
    fn ensure_webview(
        &mut self,
        key: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Availability {
        let initial_url = self.store.read(cx).preview_url(key);
        self.lifecycle_entity().update(cx, |lifecycle, cx| {
            lifecycle.ensure(key, initial_url.as_deref(), window, cx)
        })
    }

    pub(super) fn drop_webview(&mut self, key: &str, cx: &mut Context<Self>) {
        self.lifecycle_entity()
            .update(cx, |lifecycle, cx| lifecycle.drop_view(key, cx));
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
        self.lifecycle_entity()
            .update(cx, |lifecycle, cx| lifecycle.prune(&live, cx));
    }

    pub(super) fn navigate_backend(
        &mut self,
        key: &str,
        url: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.lifecycle_entity().update(cx, |lifecycle, cx| {
            lifecycle.navigate(key, url, window, cx);
        });
    }

    /// Run raw JS on the active WebView via history/reload (fire-and-forget).
    fn eval_fire(&mut self, key: &str, script: &str, cx: &mut Context<Self>) {
        self.lifecycle_entity()
            .update(cx, |lifecycle, cx| lifecycle.eval_fire(key, script, cx));
    }

    pub(super) fn history_controls(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        use crate::material::toolbar_icon_button;
        let compact = self.window_state.read(cx).compact;
        vec![
            toolbar_icon_button(
                "preview-back",
                IconName::ArrowLeft,
                crate::tr!("preview.back"),
                compact,
            )
            .on_click(cx.listener(|this, _, window, cx| this.go_back(window, cx)))
            .into_any_element(),
            toolbar_icon_button(
                "preview-forward",
                IconName::ArrowRight,
                crate::tr!("preview.forward"),
                compact,
            )
            .on_click(cx.listener(|this, _, _, cx| this.go_forward(cx)))
            .into_any_element(),
            toolbar_icon_button(
                "preview-reload",
                IconName::Replace,
                crate::tr!("preview.reload"),
                compact,
            )
            .on_click(cx.listener(|this, _, _, cx| this.reload(cx)))
            .into_any_element(),
        ]
    }

    fn go_back(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(id) = self.active_key(cx)
            && let Availability::Ready(view) = self.ensure_webview(&id, window, cx)
        {
            view.update(cx, |view, _| {
                if let Err(error) = view.back() {
                    log::debug!("preview: failed to navigate back during teardown: {error}");
                }
            });
        }
    }

    fn go_forward(&mut self, cx: &mut Context<Self>) {
        if let Some(id) = self.active_key(cx) {
            #[cfg(target_os = "android")]
            self.eval_fire(&id, "history.forward();", cx);
            #[cfg(not(target_os = "android"))]
            if let Some(view) = self.lifecycle_entity().read(cx).ready_view(&id) {
                view.update(cx, |view, _| {
                    if let Err(error) = view.forward() {
                        log::debug!("preview: failed to navigate forward: {error}");
                    }
                });
            }
        }
    }

    fn reload(&mut self, cx: &mut Context<Self>) {
        if let Some(id) = self.active_key(cx) {
            self.eval_fire(&id, "location.reload();", cx);
        }
    }

    /// A row of quick-pick buttons for discovered localhost dev ports.
    pub(super) fn render_port_row(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        use crate::sizing::Sizable as _;
        use crate::widgets::button::Button;
        if self.dev_ports.is_empty() {
            return None;
        }
        let mut row = gpui_base::h_flex()
            .flex_none()
            .w_full()
            .gap(cx.theme().tokens.spacing.xs)
            .px(cx.theme().tokens.spacing.xs)
            .pb(cx.theme().tokens.spacing.xs)
            .flex_wrap();
        for port in self.dev_ports.clone() {
            row = row.child(
                Button::new(("dev-port", port as usize))
                    .outline()
                    .small()
                    .compact()
                    .label(format!(":{port}"))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        let url = format!("http://localhost:{port}/");
                        if let Some(key) = this.active_key(cx) {
                            this.navigate(&key, &url, window, cx);
                        }
                    })),
            );
        }
        Some(row.into_any_element())
    }

    pub(super) fn rescan_ports(&mut self, cx: &mut Context<Self>) {
        if cfg!(any(target_os = "android", target_os = "ios")) || self.store.read(cx).is_remote() {
            return;
        }
        self.port_scan_generation = self.port_scan_generation.wrapping_add(1);
        let generation = self.port_scan_generation;
        cx.spawn(async move |this, cx| {
            let ports = cx
                .background_executor()
                .spawn(async { ports::scan_listening() })
                .await;
            let _ = this.update(cx, |panel, cx| {
                if panel.port_scan_generation == generation {
                    panel.dev_ports = ports;
                    cx.notify();
                }
            });
        })
        .detach();
    }

    pub(super) fn render_body(
        &mut self,
        active: Option<&str>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        if self.window_state.read(cx).compact
            && let Some(key) = active
            && self.store.read(cx).preview_url(key).is_none()
            && self.lifecycle_entity().read(cx).ready_view(key).is_none()
        {
            return self.render_note(
                IconName::Globe,
                crate::tr!("preview.url_placeholder").into_owned(),
                None,
                cx,
            );
        }
        #[cfg(target_os = "android")]
        if let Some(key) = active {
            let url = self
                .lifecycle_entity()
                .read(cx)
                .ready_view(key)
                .map(|view| view.read(cx).url())
                .filter(|url| !url.is_empty());
            if let Some(url) = url
                && self.store.read(cx).preview_url(key).as_ref() != Some(&url)
            {
                self.store
                    .update(cx, |store, cx| store.set_preview_url(key, url.clone(), cx));
                self.url_input
                    .update(cx, |input, cx| input.set_value(url, window, cx));
            }
        }
        #[cfg(any(target_os = "macos", target_os = "ios"))]
        if let Some(key) = active
            && let Some(url) = self.lifecycle_entity().read(cx).remote_url(key)
            && self.store.read(cx).preview_url(key).as_ref() != Some(&url)
        {
            self.store
                .update(cx, |store, cx| store.set_preview_url(key, url.clone(), cx));
            self.url_input
                .update(cx, |input, cx| input.set_value(url, window, cx));
        }
        let body: AnyElement = match active {
            Some(id) => match self.ensure_webview(id, window, cx) {
                Availability::Ready(view) => {
                    #[cfg(target_os = "ios")]
                    let view = super::ios::element(view);
                    if let Some((width, height)) = self.store.read(cx).preview_canvas(id) {
                        div()
                            .flex()
                            .flex_1()
                            .min_h_0()
                            .items_center()
                            .justify_center()
                            .child(
                                div()
                                    .flex_none()
                                    .w(px(width as f32))
                                    .h(px(height as f32))
                                    .max_w_full()
                                    .max_h_full()
                                    .child(view),
                            )
                            .into_any_element()
                    } else {
                        div().flex_1().min_h_0().child(view).into_any_element()
                    }
                }
                Availability::Starting(_) => self.render_note(
                    IconName::Globe,
                    crate::tr!("preview.starting").into_owned(),
                    None,
                    cx,
                ),
                Availability::Unavailable => self.render_note(
                    IconName::TriangleAlert,
                    crate::tr!("preview.unavailable").into_owned(),
                    Some(
                        self.lifecycle_entity()
                            .read(cx)
                            .unavailable_error()
                            .map(str::to_owned)
                            .unwrap_or_else(|| crate::tr!("preview.unavailable_hint").into_owned()),
                    ),
                    cx,
                ),
            },
            None => self.render_note(
                IconName::Globe,
                crate::tr!("preview.no_session").into_owned(),
                None,
                cx,
            ),
        };
        // `ensure_webview` creates children hidden; make the owning
        // conversation visible only after the current layout owns it.
        self.sync_mounted_visibility(cx);
        if let Some(error) =
            active.and_then(|key| self.lifecycle_entity().read(cx).load_error(key, cx))
        {
            return v_flex()
                .size_full()
                .child(
                    div()
                        .flex_none()
                        .px(cx.theme().tokens.spacing.sm)
                        .py(cx.theme().tokens.spacing.xs)
                        .text_token(&cx.theme().tokens.typography.xs)
                        .text_color(cx.theme().danger)
                        .child(
                            crate::tr!(
                                "preview.load_error",
                                url = error.url,
                                message = error.message,
                                code = error.code
                            )
                            .into_owned(),
                        ),
                )
                .child(body)
                .into_any_element();
        }
        #[cfg(any(target_os = "macos", target_os = "ios"))]
        if self.store.read(cx).is_remote() {
            return v_flex()
                .size_full()
                .child(
                    div()
                        .flex_none()
                        .px(cx.theme().tokens.spacing.sm)
                        .py(cx.theme().tokens.spacing.xs)
                        .text_secondary(false, cx)
                        .child(crate::tr!("preview.remote_forwarding").into_owned()),
                )
                .child(body)
                .into_any_element();
        }
        body
    }

    /// Resolve one automation op from the MCP server against the active WebView.
    /// Answers `reply` immediately for actions, or from the JS callback for
    /// value-returning ops.
    pub fn handle_op(
        &mut self,
        session_id: String,
        op: PreviewRequest,
        reply: ReplyTx,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let key = self.routed_key(&session_id, cx);
        log::info!("preview: handling op {op:?} for session {session_id}");

        // Gate on the Browser settings: a disabled browser rejects every op;
        // `allow_evaluate` gates only `preview_evaluate`.
        let browser = self.store.read(cx).preview_browser_settings();
        if !browser.enabled {
            let _ = reply.try_send(Err(crate::tr!("browser.disabled_error").into_owned()));
            return;
        }
        if matches!(&op, PreviewRequest::Evaluate { .. }) && !browser.allow_evaluate {
            let _ = reply.try_send(Err(
                crate::tr!("browser.evaluate_disabled_error").into_owned()
            ));
            return;
        }

        // The conversation may be one nobody is looking at: size its browser
        // as the panel would before the op drives the page. A resize only
        // changes that size, so it creates no browser.
        if !matches!(&op, PreviewRequest::Resize { .. }) {
            self.ensure_webview(&key, window, cx);
        }
        self.stage_hidden(&key, window, cx);

        match op {
            PreviewRequest::Open { url } => {
                self.store.update(cx, |store, cx| {
                    store.open_tab_for(&session_id, tcode_core::ui::RightTab::Preview, cx);
                });
                if let Some(url) = url.as_deref() {
                    self.navigate(&key, url, window, cx);
                } else if let Some(home) = browser
                    .home_url
                    .as_deref()
                    .map(str::trim)
                    .filter(|home| !home.is_empty())
                {
                    // No explicit target: fall back to the configured home URL.
                    self.navigate(&key, home, window, cx);
                } else {
                    self.ensure_webview(&key, window, cx);
                    self.sync_visibility(cx);
                }
                if let Some(error) = self
                    .lifecycle_entity()
                    .read(cx)
                    .unavailable_error()
                    .map(str::to_string)
                {
                    let _ = reply.try_send(Err(unavailable_message(&error)));
                    return;
                }
                let payload = serde_json::json!({
                    "ok": true,
                    "url": self.store.read(cx).preview_url(&key),
                    "note": "call preview_status for live page state once loaded; \
                             it reports load_error when the page failed to load",
                });
                let _ = reply.try_send(Ok(PreviewResponse::Json(payload)));
            }
            PreviewRequest::Navigate { url } => {
                self.store.update(cx, |store, cx| {
                    store.open_tab_for(&session_id, tcode_core::ui::RightTab::Preview, cx);
                });
                self.navigate(&key, &url, window, cx);
                if let Some(error) = self
                    .lifecycle_entity()
                    .read(cx)
                    .unavailable_error()
                    .map(str::to_string)
                {
                    let _ = reply.try_send(Err(unavailable_message(&error)));
                    return;
                }
                let payload = serde_json::json!({
                    "ok": true,
                    "url": self.store.read(cx).preview_url(&key),
                    "note": "page is loading; call preview_status for live state, \
                             which reports load_error when the page failed to load",
                });
                let _ = reply.try_send(Ok(PreviewResponse::Json(payload)));
            }
            PreviewRequest::Status => self.status(&key, reply, window, cx),
            PreviewRequest::Snapshot => self.eval_json(&key, js::SNAPSHOT, reply, window, cx),
            PreviewRequest::Evaluate { js: expr } => {
                self.eval_json(&key, &js::evaluate(&expr), reply, window, cx)
            }
            PreviewRequest::Click { selector } => {
                self.eval_json(&key, &js::click(&selector), reply, window, cx)
            }
            PreviewRequest::Type { selector, text } => {
                self.eval_json(&key, &js::type_text(&selector, &text), reply, window, cx)
            }
            PreviewRequest::Resize { width, height } => {
                self.store.update(cx, |store, cx| {
                    store.open_tab_for(&session_id, tcode_core::ui::RightTab::Preview, cx);
                });
                let payload = match (width, height) {
                    (Some(width), Some(height)) => {
                        self.store.update(cx, |store, cx| {
                            store.set_preview_canvas(&key, Some((width, height)), cx);
                        });
                        serde_json::json!({
                            "ok": true,
                            "mode": "fixed",
                            "width": width,
                            "height": height,
                            "note": "fixed canvas is clamped to the panel if larger",
                        })
                    }
                    _ => {
                        self.store.update(cx, |store, cx| {
                            store.set_preview_canvas(&key, None, cx);
                        });
                        serde_json::json!({
                            "ok": true,
                            "mode": "fill",
                            "note": "preview fills the available panel",
                        })
                    }
                };
                self.stage_hidden(&key, window, cx);
                self.sync_visibility(cx);
                cx.notify();
                let _ = reply.try_send(Ok(PreviewResponse::Json(payload)));
            }
            // `key` is the routed conversation key; bind the keyboard key
            // apart so it cannot shadow it into `eval_json`'s session slot.
            PreviewRequest::Press {
                key: pressed,
                modifiers,
            } => self.eval_json(&key, &js::press(&pressed, &modifiers), reply, window, cx),
            PreviewRequest::Scroll {
                delta_x,
                delta_y,
                selector,
            } => self.eval_json(
                &key,
                &js::scroll(delta_x, delta_y, selector.as_deref()),
                reply,
                window,
                cx,
            ),
            PreviewRequest::WaitFor {
                selector,
                text,
                url_includes,
                timeout_ms,
            } => self.wait_for(
                &key,
                selector,
                text,
                url_includes,
                timeout_ms,
                reply,
                window,
                cx,
            ),
            PreviewRequest::Screenshot => self.screenshot(&session_id, &key, reply, window, cx),
        }
    }

    /// Add this conversation's canvas setting to the otherwise opaque page
    /// status object returned by JavaScript.
    fn status(&mut self, key: &str, reply: ReplyTx, window: &mut Window, cx: &mut Context<Self>) {
        let canvas = self
            .store
            .read(cx)
            .preview_canvas(key)
            .map(|(width, height)| {
                serde_json::json!({
                    "mode": "fixed",
                    "width": width,
                    "height": height,
                })
            })
            .unwrap_or_else(|| serde_json::json!({ "mode": "fill" }));
        // The page cannot see a failed navigation, so the JS probe would
        // keep describing whatever was on screen before it.
        let load_error = self.lifecycle_entity().read(cx).load_error(key, cx);
        #[cfg(target_os = "android")]
        let native_metadata = self
            .lifecycle_entity()
            .read(cx)
            .ready_view(key)
            .map(|view| view.read(cx).metadata());
        let (status_reply, status_result) = async_channel::bounded(1);
        let lifecycle = self.lifecycle_entity().downgrade();
        let status_key = key.to_owned();
        cx.spawn(async move |_, cx| {
            let result = match status_result.recv().await {
                Ok(Ok(PreviewResponse::Json(mut value))) => {
                    if let Some(object) = value.as_object_mut() {
                        object.insert("canvas".into(), canvas);
                        if let Some(actual) = object
                            .get("url")
                            .and_then(serde_json::Value::as_str)
                            .map(str::to_owned)
                            && let Ok(logical) = lifecycle.update(cx, |lifecycle, _| {
                                lifecycle.logical_url(&status_key, &actual)
                            })
                            && logical != actual
                        {
                            object.insert("actual_url".into(), actual.into());
                            object.insert("url".into(), logical.into());
                        }
                        #[cfg(target_os = "android")]
                        if let Some((url, title)) = native_metadata {
                            object.insert("url".into(), url.into());
                            object.insert("title".into(), title.into());
                        }
                        if let Some(load_error) = load_error {
                            object.insert("load_error".into(), load_error.to_json());
                        }
                        Ok(PreviewResponse::Json(value))
                    } else {
                        Err("preview status returned a non-object value".into())
                    }
                }
                Ok(result) => result,
                Err(_) => Err("preview status evaluation was dropped".into()),
            };
            let _ = reply.send(result).await;
        })
        .detach();
        self.eval_json(key, js::STATUS, status_reply, window, cx);
    }

    /// Poll a one-shot page probe every 250ms until it matches or reaches
    /// its deadline. Each evaluation has its own watchdog because native
    /// WebViews can drop callbacks during navigation.
    #[allow(clippy::too_many_arguments)]
    fn wait_for(
        &mut self,
        key: &str,
        selector: Option<String>,
        text: Option<String>,
        url_includes: Option<String>,
        timeout_ms: u64,
        reply: ReplyTx,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match self.ensure_webview(key, window, cx) {
            Availability::Ready(_) => {}
            Availability::Starting(_) => {
                let _ = reply.try_send(Err(STARTING_MESSAGE.into()));
                return;
            }
            Availability::Unavailable => {
                let error = self
                    .lifecycle_entity()
                    .read(cx)
                    .unavailable_error()
                    .unwrap_or_default()
                    .to_string();
                let _ = reply.try_send(Err(unavailable_message(&error)));
                return;
            }
        }
        let cold = !self.lifecycle_entity().read(cx).is_warm(key);
        let key = key.to_string();
        let probe = js::wait_for_probe(selector.as_deref(), text.as_deref(), None);
        let mut pending = Vec::new();
        if selector.is_some() {
            pending.push("selector".to_string());
        }
        if text.is_some() {
            pending.push("text".to_string());
        }
        if url_includes.is_some() {
            pending.push("urlIncludes".to_string());
        }
        cx.spawn(async move |this, cx| {
            let deadline = std::time::Instant::now() + Duration::from_millis(timeout_ms);
            if cold {
                cx.background_executor()
                    .timer(Duration::from_millis(700))
                    .await;
                if this
                    .update(cx, |panel, cx| {
                        panel.lifecycle_entity().update(cx, |lifecycle, _| {
                            lifecycle.mark_warm(&key);
                        });
                    })
                    .is_err()
                {
                    let _ = reply
                        .send(Err("preview panel was dropped while waiting".into()))
                        .await;
                    return;
                }
            }
            loop {
                let remaining = deadline.saturating_duration_since(std::time::Instant::now());
                if remaining.is_zero() {
                    let _ = reply.send(Err(wait_timeout_message(&pending))).await;
                    return;
                }
                let (probe_reply, probe_result) = async_channel::bounded(1);
                // A failed navigation never changes the page, so waiting on
                // it can only time out; report the platform error instead.
                let Ok(failure) = this.update(cx, |panel, cx| {
                    let failure = panel.lifecycle_entity().read(cx).load_error(&key, cx);
                    if failure.is_none() {
                        panel.lifecycle_entity().update(cx, |lifecycle, cx| {
                            lifecycle.evaluate_ready(&key, &probe, probe_reply.clone(), cx);
                        });
                    }
                    failure
                }) else {
                    let _ = reply
                        .send(Err("preview panel was dropped while waiting".into()))
                        .await;
                    return;
                };
                if let Some(failure) = failure {
                    let _ = reply.send(Err(failure.describe())).await;
                    return;
                }

                let watchdog_delay = remaining.min(Duration::from_secs(5));
                let watchdog_reply = probe_reply;
                let watchdog_error = if remaining <= Duration::from_secs(5) {
                    wait_timeout_message(&pending)
                } else {
                    "preview wait probe evaluation timed out".into()
                };
                let watchdog_timer = cx.background_executor().timer(watchdog_delay);
                cx.background_executor()
                    .spawn(async move {
                        watchdog_timer.await;
                        let _ = watchdog_reply.try_send(Err(watchdog_error));
                    })
                    .detach();

                let mut value = match probe_result.recv().await {
                    Ok(Ok(PreviewResponse::Json(value))) => value,
                    Ok(Ok(PreviewResponse::Image { .. })) => {
                        let _ = reply
                            .send(Err("preview wait probe returned an image".into()))
                            .await;
                        return;
                    }
                    Ok(Err(error)) => {
                        let _ = reply.send(Err(error)).await;
                        return;
                    }
                    Err(_) => {
                        let _ = reply
                            .send(Err("preview wait probe evaluation was dropped".into()))
                            .await;
                        return;
                    }
                };
                if let Some(actual) = value
                    .get("url")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_owned)
                    && let Ok(logical) = this.update(cx, |panel, cx| {
                        panel.lifecycle_entity().read(cx).logical_url(&key, &actual)
                    })
                {
                    if logical != actual {
                        value["actual_url"] = actual.into();
                    }
                    value["url"] = logical.clone().into();
                    if url_includes
                        .as_ref()
                        .is_some_and(|needle| !logical.contains(needle))
                    {
                        value["matched"] = false.into();
                        if let Some(pending) = value["pending"].as_array_mut() {
                            pending.push("urlIncludes".into());
                        }
                    }
                }
                if value.get("matched").and_then(serde_json::Value::as_bool) == Some(true) {
                    let _ = reply.send(Ok(PreviewResponse::Json(value))).await;
                    return;
                }
                pending = value
                    .get("pending")
                    .and_then(serde_json::Value::as_array)
                    .map(|items| {
                        items
                            .iter()
                            .filter_map(serde_json::Value::as_str)
                            .map(str::to_string)
                            .collect()
                    })
                    .unwrap_or_else(|| pending.clone());
                cx.background_executor()
                    .timer(Duration::from_millis(250))
                    .await;
            }
        })
        .detach();
    }

    fn stage_hidden(&mut self, key: &str, window: &mut Window, cx: &mut Context<Self>) {
        #[cfg(not(target_os = "android"))]
        {
            let canvas = self
                .store
                .read(cx)
                .preview_canvas(key)
                .map(|(width, height)| gpui::size(px(width as f32), px(height as f32)));
            let fallback = window.viewport_size();
            self.lifecycle_entity()
                .read(cx)
                .stage_hidden(key, canvas, fallback, cx);
        }
        #[cfg(target_os = "android")]
        let _ = (key, window, cx);
    }

    /// Delegate value-returning evaluation to the lifecycle, which owns the
    /// ready/warm ordering and native callback.
    fn eval_json(
        &mut self,
        key: &str,
        script: &str,
        reply: ReplyTx,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let initial_url = self.store.read(cx).preview_url(key);
        self.lifecycle_entity().update(cx, |lifecycle, cx| {
            lifecycle.evaluate_json(key, initial_url.as_deref(), script, reply, window, cx);
        });
    }

    /// Snapshot the native WKWebView in-process and answer with a base64 PNG.
    #[cfg(any(target_os = "macos", target_os = "ios"))]
    fn screenshot(
        &mut self,
        _session_id: &str,
        key: &str,
        reply: ReplyTx,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        use block2::RcBlock;
        #[cfg(target_os = "macos")]
        use objc2_app_kit::NSImage as Image;
        use objc2_foundation::NSError;
        #[cfg(target_os = "ios")]
        use objc2_ui_kit::UIImage as Image;
        #[cfg(target_os = "ios")]
        use wry::WebViewExtIOS as _;
        #[cfg(target_os = "macos")]
        use wry::WebViewExtMacOS as _;

        let Some(view) = self.lifecycle_entity().read(cx).ready_view(key) else {
            let _ = reply.try_send(Err("preview browser is not open".into()));
            return;
        };
        let webview = view.read(cx).raw().webview();
        // The native frame, not the GPUI bounds: a hidden browser has never
        // been laid out by GPUI, only staged offscreen.
        let frame = webview.frame();
        if frame.size.width <= 0. || frame.size.height <= 0. {
            let _ = reply.try_send(Err("preview browser has no visible area".into()));
            return;
        }
        let callback_reply = reply.clone();
        let handler = RcBlock::new(move |image: *mut Image, error: *mut NSError| {
            let result = if let Some(image) = unsafe { image.as_ref() } {
                snapshot_reply(image)
            } else if !error.is_null() {
                Err("WKWebView snapshot failed".into())
            } else {
                Err("WKWebView snapshot returned no image".into())
            };
            let _ = callback_reply.try_send(result);
        });
        #[cfg(target_os = "macos")]
        unsafe {
            webview.takeSnapshotWithConfiguration_completionHandler(None, &handler);
        }
        // lb-wry's iOS WKWebView binding declares no snapshot method.
        #[cfg(target_os = "ios")]
        unsafe {
            let () = objc2::msg_send![
                &*webview,
                takeSnapshotWithConfiguration: std::ptr::null::<objc2::runtime::AnyObject>(),
                completionHandler: &*handler
            ];
        }

        cx.spawn(async move |_, cx| {
            cx.background_executor().timer(Duration::from_secs(5)).await;
            let _ = reply.try_send(Err("WKWebView snapshot timed out after 5 seconds".into()));
        })
        .detach();
    }

    #[cfg(target_os = "android")]
    fn screenshot(
        &mut self,
        session_id: &str,
        key: &str,
        reply: ReplyTx,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let state = self.window_state.read(cx);
        if self.store.read(cx).active_session_id().as_deref() != Some(session_id)
            || !self.store.read(cx).preview_panel_showing()
            || state.palette_open
            || state.route() != crate::window_state::Route::Chat
            || (state.compact
                && (state.destination() != crate::window_state::Destination::Panel
                    || !self.backend.compact_panel_selected))
        {
            let _ = reply.try_send(Err(
                "preview is not visible; open the Preview panel before taking a screenshot".into(),
            ));
            return;
        }
        let Some(view) = self.lifecycle_entity().read(cx).ready_view(key) else {
            let _ = reply.try_send(Err("preview browser is not open".into()));
            return;
        };
        let request = view.read(cx).raw().screenshot();
        cx.spawn(async move |_, cx| {
            use base64::Engine as _;
            let result = futures::future::select(
                Box::pin(request),
                Box::pin(cx.background_executor().timer(Duration::from_secs(5))),
            )
            .await;
            let result = match result {
                futures::future::Either::Left((
                    Ok(gpui_android::webview::Reply::Png(bytes)),
                    _,
                )) => Ok(PreviewResponse::Image {
                    mime: "image/png".into(),
                    data_base64: base64::engine::general_purpose::STANDARD.encode(bytes),
                }),
                futures::future::Either::Left((Err(error), _)) => Err(error),
                futures::future::Either::Left(_) => {
                    Err("preview screenshot returned no image".into())
                }
                futures::future::Either::Right(_) => {
                    Err("preview screenshot timed out after 5 seconds".into())
                }
            };
            let _ = reply.try_send(result);
        })
        .detach();
    }

    /// WebView2 capture is not implemented, so this is a plain tool error.
    #[cfg(not(any(target_os = "macos", target_os = "ios", target_os = "android")))]
    fn screenshot(
        &mut self,
        _session_id: &str,
        _key: &str,
        reply: ReplyTx,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
        let _ = reply.try_send(Err(SCREENSHOT_UNSUPPORTED.into()));
    }
}

#[cfg(target_os = "macos")]
fn snapshot_reply(image: &objc2_app_kit::NSImage) -> Result<PreviewResponse, String> {
    let cg_image =
        unsafe { image.CGImageForProposedRect_context_hints(std::ptr::null_mut(), None, None) }
            .ok_or_else(|| "failed to obtain CGImage from WKWebView snapshot".to_string())?;
    png_reply(&cg_image)
}

#[cfg(target_os = "ios")]
fn snapshot_reply(image: &objc2_ui_kit::UIImage) -> Result<PreviewResponse, String> {
    let cg_image = unsafe { image.CGImage() }
        .ok_or_else(|| "failed to obtain CGImage from WKWebView snapshot".to_string())?;
    png_reply(&cg_image)
}

#[cfg(any(target_os = "macos", target_os = "ios"))]
fn png_reply(cg_image: &objc2_core_graphics::CGImage) -> Result<PreviewResponse, String> {
    use base64::Engine as _;
    use objc2_core_foundation::{CFMutableData, CFString};
    use objc2_image_io::CGImageDestination;

    let data = CFMutableData::new(None, 0)
        .ok_or_else(|| "failed to allocate PNG destination data".to_string())?;
    let png_type = CFString::from_static_str("public.png");
    let destination = unsafe { CGImageDestination::with_data(&data, &png_type, 1, None) }
        .ok_or_else(|| "failed to create PNG image destination".to_string())?;
    unsafe {
        destination.add_image(cg_image, None);
        if !destination.finalize() {
            return Err("failed to finalize WKWebView snapshot PNG".into());
        }
    }
    Ok(PreviewResponse::Image {
        mime: "image/png".into(),
        data_base64: base64::engine::general_purpose::STANDARD.encode(data.to_vec()),
    })
}
