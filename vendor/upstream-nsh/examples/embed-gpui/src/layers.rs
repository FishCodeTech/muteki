//! "Show layers": outlines and tags that tell what the host draws with GPUI
//! Kit, what an NSH surface (a TSX app) draws, and which GPUI widgets sit
//! inside an NSH layout. Off, the window looks seamless.
//!
//! The setting is a GPUI global, so the host's shell and the native
//! components inside the surfaces read the same value.

use gpui_kit::{
    App, Div, Global, Hsla, IntoElement, ParentElement, Styled, component::ActiveTheme, div, px,
};

/// Whether the layers show (on by default).
pub struct Layers(pub bool);

impl Global for Layers {}

/// Whether the layers show.
pub fn shown(cx: &App) -> bool {
    cx.try_global::<Layers>().is_none_or(|l| l.0)
}

/// The color of NSH surfaces' outlines and tags, and their tags' text.
pub fn nsh_colors(cx: &App) -> (Hsla, Hsla) {
    let theme = cx.theme();
    (theme.info, on(theme.info_foreground, cx))
}

/// The color of GPUI widgets' outlines and tags inside an NSH app, and
/// their tags' text.
pub fn widget_colors(cx: &App) -> (Hsla, Hsla) {
    let theme = cx.theme();
    (theme.warning, on(theme.warning_foreground, cx))
}

/// A tag's text: `light` in light mode; in dark mode, where the status
/// colors are bright, the window's background.
fn on(light: Hsla, cx: &App) -> Hsla {
    let theme = cx.theme();
    if theme.mode.is_dark() {
        theme.background
    } else {
        light
    }
}

/// The width a widget's tag takes beside it (its box grows by it while the
/// layers show, so the tag covers nothing).
pub const WIDGET_TAG: f32 = 40.0;

/// A small tag in `color`, for a corner of what it names. It has no mouse
/// handlers, so the input goes through it to what is under it.
pub fn tag(text: &'static str, (color, foreground): (Hsla, Hsla)) -> Div {
    div()
        .px(px(5.))
        .py(px(1.))
        .rounded(px(4.))
        .bg(color)
        .text_color(foreground)
        .text_size(px(10.))
        .line_height(px(14.))
        .child(text)
}

/// The outline and tag of an NSH surface, drawn over it.
pub fn nsh_overlay(cx: &App) -> impl IntoElement {
    let colors = nsh_colors(cx);
    div()
        .absolute()
        .inset_0()
        .border_2()
        .border_color(colors.0)
        // At the bottom, clear of the pane's title bar and of the app's
        // content, which starts at the top.
        .child(
            div()
                .absolute()
                .bottom(px(4.))
                .right(px(4.))
                .child(tag("NSH · TSX app", colors)),
        )
}

/// A swatch and its label, for the legend.
pub fn legend_item(label: &'static str, color: Hsla, cx: &App) -> impl IntoElement {
    gpui_kit::component::h_flex()
        .gap_1()
        .child(
            div()
                .size(px(10.))
                .rounded(px(2.))
                .border_2()
                .border_color(color),
        )
        .child(
            div()
                .text_xs()
                .text_color(cx.theme().muted_foreground)
                .child(label),
        )
}

/// A GPUI widget inside an NSH app, framed while the layers show: an
/// outline around it and a tag beside it, [`WIDGET_TAG`] plus
/// [`WIDGET_BORDER`] on each side wider than the widget.
pub fn widget_frame(widget: impl IntoElement, cx: &App) -> Div {
    let row = gpui_kit::component::h_flex().size_full();
    if !shown(cx) {
        return row.child(widget);
    }
    let colors = widget_colors(cx);
    row.border(px(WIDGET_BORDER))
        .border_color(colors.0)
        .rounded(px(4.))
        .child(div().flex_1().min_w_0().h_full().child(widget))
        .child(
            div()
                .flex_none()
                .w(px(WIDGET_TAG))
                .flex()
                .justify_center()
                .child(tag("GPUI", colors)),
        )
}

/// The outline's width around a framed widget.
pub const WIDGET_BORDER: f32 = 1.0;
