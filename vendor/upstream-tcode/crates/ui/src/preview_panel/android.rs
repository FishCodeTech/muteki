//! GPUI owner for the activity's WebView. The shared lifecycle owns these
//! entities; Java owns the actual views and only sends owned events/results.
use super::{
    ReplyTx,
    load_error::{self, LoadError},
};
use futures::StreamExt as _;
use gpui::{
    Context, IntoElement, ParentElement as _, Render, Styled as _, Window, canvas, div, point,
};
use gpui_android::webview::{Browser, Event, Reply};
use std::{cell::RefCell, rc::Rc, time::Duration};

pub struct RawWebView {
    browser: Browser,
    error: RefCell<Option<LoadError>>,
    url: RefCell<String>,
    title: RefCell<String>,
}
impl RawWebView {
    pub(super) fn new(
        initial_url: &str,
        proxy: Option<&tcode_traverse::preview::ProxyEntry>,
    ) -> Result<(Self, futures::channel::mpsc::UnboundedReceiver<Event>), String> {
        let (browser, events) = Browser::new(&serde_json::json!({"url": initial_url, "proxy": proxy.map(|host| serde_json::json!({"origin": host.origin}))}).to_string())?;
        Ok((
            Self {
                browser,
                error: RefCell::new(None),
                url: RefCell::new(initial_url.to_owned()),
                title: RefCell::new(String::new()),
            },
            events,
        ))
    }
    pub(super) fn load_error(&self) -> Option<LoadError> {
        self.error.borrow().clone()
    }
    pub(super) fn clear_load_error(&self) {
        self.error.borrow_mut().take();
    }
    pub fn load_url(&self, url: &str) -> Result<(), String> {
        self.clear_load_error();
        *self.url.borrow_mut() = url.to_owned();
        self.browser.command("navigate", url, [0; 4]);
        Ok(())
    }
    pub fn set_visible(&self, visible: bool) -> Result<(), String> {
        self.browser
            .command(if visible { "show" } else { "hide" }, "", [0; 4]);
        Ok(())
    }
    pub fn focus_parent(&self) -> Result<(), String> {
        self.browser.command("blur", "", [0; 4]);
        Ok(())
    }
    pub fn evaluate_script(&self, script: &str) -> Result<(), String> {
        let operation = match script {
            "history.forward();" => "forward",
            "location.reload();" => "reload",
            "history.back();" => "back",
            _ => "evaluate",
        };
        self.browser.command(operation, script, [0; 4]);
        Ok(())
    }
    pub(super) fn evaluate_json<T: 'static>(&self, script: &str, reply: ReplyTx, cx: &Context<T>) {
        let request = self.browser.request("evaluate", script);
        cx.spawn(async move |_, cx| {
            let result = futures::future::select(
                Box::pin(request),
                Box::pin(cx.background_executor().timer(Duration::from_secs(10))),
            )
            .await;
            let result = match result {
                futures::future::Either::Left((Ok(Reply::Json(raw)), _)) => Ok(
                    tcode_protocol::PreviewResponse::Json(preview_mcp::js::parse_result(&raw)),
                ),
                futures::future::Either::Left((Err(error), _)) => Err(error),
                futures::future::Either::Left((Ok(Reply::Png(_)), _)) => {
                    Err("preview returned an image for JavaScript".into())
                }
                futures::future::Either::Right(_) => Err("preview JavaScript timed out".into()),
            };
            let _ = reply.try_send(result);
        })
        .detach();
    }
    pub(super) fn screenshot(&self) -> impl Future<Output = Result<Reply, String>> + use<> {
        self.browser.request("screenshot", "")
    }
}

pub struct WebView {
    raw: Rc<RawWebView>,
    visible: bool,
}
impl WebView {
    pub(super) fn new(
        raw: RawWebView,
        mut events: futures::channel::mpsc::UnboundedReceiver<Event>,
        cx: &mut Context<Self>,
    ) -> Self {
        cx.spawn(async move |this, cx| {
            while let Some(event) = events.next().await {
                if this
                    .update(cx, |view, cx| {
                        if !event.url.is_empty() {
                            *view.raw.url.borrow_mut() = event.url.clone();
                        }
                        *view.raw.title.borrow_mut() = event.title;
                        load_error::android_event(
                            &mut view.raw.error.borrow_mut(),
                            event.kind,
                            event.url,
                            event.code,
                            event.message,
                        );
                        cx.notify();
                    })
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
        Self {
            raw: Rc::new(raw),
            visible: false,
        }
    }
    pub fn raw(&self) -> &RawWebView {
        &self.raw
    }
    pub fn show(&mut self) {
        self.visible = true;
    }
    pub fn hide(&mut self) {
        self.visible = false;
        let _ = self.raw.set_visible(false);
    }
    pub fn back(&mut self) -> Result<(), String> {
        self.raw.evaluate_script("history.back();")
    }
    pub(super) fn metadata(&self) -> (String, String) {
        (self.url(), self.raw.title.borrow().clone())
    }
    pub(super) fn url(&self) -> String {
        self.raw.url.borrow().clone()
    }
}
impl Drop for WebView {
    fn drop(&mut self) {
        self.hide();
    }
}
impl Render for WebView {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        let raw = self.raw.clone();
        let visible = self.visible;
        div().size_full().child(
            canvas(
                move |bounds, window, _| {
                    let safe = crate::window_seam::content_insets(window);
                    let physical = super::android_geometry::physical_bounds(
                        gpui::Bounds {
                            origin: bounds.origin - point(safe.left, safe.top),
                            size: bounds.size,
                        },
                        point(safe.left, safe.top),
                        window.scale_factor(),
                    );
                    raw.browser.command("bounds", "", physical);
                    let _ = raw.set_visible(visible && physical[2] > 0 && physical[3] > 0);
                },
                |_, _, _, _| {},
            )
            .size_full(),
        )
    }
}
