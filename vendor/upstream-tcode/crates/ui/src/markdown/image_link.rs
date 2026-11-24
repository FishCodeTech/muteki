use std::path::Path;

use gpui::{App, Div, Entity, ParentElement as _, SharedString, Styled as _, Window, div, px};

use crate::{
    icon::Icon,
    material::{Bar, bar_height},
    sizing::Sizable as _,
    theme::ActiveTheme as _,
    typography::TextRole as _,
    widgets::button::Button,
};

use super::{
    MarkdownState,
    nodes::{InlineNode, LinkMark},
};

/// Image links are recognized by their destination, without probing the client's files.
pub(super) fn for_node(node: &InlineNode) -> Option<&LinkMark> {
    node.marks.iter().find_map(|(range, mark)| {
        let link = mark.link.as_ref()?;
        if *range != (0..node.text.len()) {
            return None;
        }
        let parsed = url::Url::parse(&link.url).ok();
        let path = match &parsed {
            Some(url) if matches!(url.scheme(), "http" | "https" | "file") => url.path(),
            Some(_) => return None,
            None => link.url.as_ref(),
        };
        let extension = Path::new(path).extension()?.to_str()?.to_ascii_lowercase();
        matches!(
            extension.as_str(),
            "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "tif" | "tiff"
        )
        .then_some(link)
    })
}

pub(super) fn badge(
    ix: usize,
    view: Entity<MarkdownState>,
    url: SharedString,
    label: SharedString,
    window: &Window,
    cx: &App,
) -> Div {
    let compact = crate::window_seam::window_is_compact(window, cx);
    let badge = Button::new(("markdown-image-link", ix))
        .small()
        .touch(compact)
        .max_w_full()
        .text_body(false, cx)
        .icon(
            Icon::default()
                .path("icons/image.svg")
                .text_color(cx.theme().link),
        )
        .aria_label(label.clone())
        .tooltip(url.clone())
        .child(
            div()
                .min_w_0()
                .overflow_hidden()
                .child(crate::widgets::fade_text(label.clone())),
        )
        .on_click(move |_, window, cx| {
            cx.stop_propagation();
            let source = view.read(cx).opened_image_source(&url.to_string().into());
            crate::image_viewer::open(source, label.to_string(), window, cx);
        });
    // Preserve the line's spacing while shortening only the visible badge.
    div()
        .flex()
        .items_center()
        .min_h(px(bar_height(Bar::Section, compact)))
        .max_w(px(360.))
        .child(badge)
}
