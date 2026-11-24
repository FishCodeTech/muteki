//! The iOS child's frame. lb-wry's `set_bounds` places a child only on macOS
//! and does nothing on iOS, and gpui-webview has no UIKit composition surface,
//! so gpui-webview never moves the WKWebView there. Tcode sets the UIKit frame
//! itself; GPUI's window coordinates are the host view's points.
use gpui::{Bounds, Entity, IntoElement, ParentElement as _, Pixels, Styled as _, canvas, div};
use gpui_webview::WebView;
use objc2_core_foundation::{CGPoint, CGRect, CGSize};
use objc2_ui_kit::UIViewAutoresizing;
use wry::WebViewExtIOS as _;

/// lb-wry sizes a new child to the whole host view and lets it stretch with
/// it; GPUI layout is the only thing that may size it.
pub(super) fn adopt(raw: &wry::WebView) {
    raw.webview().setAutoresizingMask(UIViewAutoresizing::None);
}

pub(super) fn set_frame(raw: &wry::WebView, bounds: Bounds<Pixels>) {
    raw.webview().setFrame(CGRect::new(
        CGPoint::new(f64::from(bounds.origin.x), f64::from(bounds.origin.y)),
        CGSize::new(f64::from(bounds.size.width), f64::from(bounds.size.height)),
    ));
}

/// The page, with its native frame kept on the bounds GPUI laid it out at.
pub(super) fn element(view: Entity<WebView>) -> impl IntoElement {
    let placed = view.clone();
    div().relative().size_full().child(view).child(
        canvas(
            move |bounds, _, cx| {
                let view = placed.read(cx);
                if view.visible() {
                    set_frame(view.raw(), bounds);
                }
            },
            |_, _, _, _| {},
        )
        .absolute()
        .top_0()
        .left_0()
        .size_full(),
    )
}
