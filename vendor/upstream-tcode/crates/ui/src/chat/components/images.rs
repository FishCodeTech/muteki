use std::path::Path;

use agent::ItemContent;
use gpui::{
    App, InteractiveElement as _, IntoElement, ObjectFit, ParentElement as _, Role, SharedString,
    StatefulInteractiveElement as _, Styled as _, StyledImage as _, img, px,
};
use gpui_base::h_flex;
use tcode_core::session::{EntryContent, TimelineEntry};

use crate::theme::ActiveTheme as _;
use crate::widgets::menu::{ContextMenuExt as _, CopyText};

pub(crate) fn image_tiles(
    entries: &[&TimelineEntry],
    cwd: &Path,
    session_id: &str,
    cx: &App,
) -> impl IntoElement {
    h_flex()
        .w_full()
        .gap_2()
        .flex_wrap()
        .children(entries.iter().flat_map(|entry| {
            let EntryContent::Item(item) = &entry.content else {
                unreachable!()
            };
            let images = match item {
                ItemContent::ImageRead { path, .. } => {
                    let path = cwd.join(path);
                    let title = path
                        .file_name()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .into_owned();
                    vec![(
                        tcode_protocol::OriginalTarget::Item {
                            session_id: session_id.into(),
                            item_id: entry.id.clone(),
                            image_index: 0,
                        },
                        title,
                        Some(path),
                    )]
                }
                ItemContent::ToolCall {
                    image_reads, name, ..
                } => image_reads
                    .iter()
                    .enumerate()
                    .map(|(index, _)| {
                        (
                            tcode_protocol::OriginalTarget::Item {
                                session_id: session_id.into(),
                                item_id: entry.id.clone(),
                                image_index: index,
                            },
                            format!("{name} · {}", index + 1),
                            None,
                        )
                    })
                    .collect(),
                _ => unreachable!(),
            };
            images
                .into_iter()
                .enumerate()
                .map(move |(index, (source, title, path))| {
                    let copy_items = super::activity::activity_copy_items(entry);
                    let preview = crate::store::item_image_preview(
                        session_id.into(),
                        entry.id.clone(),
                        index,
                        120.,
                        80.,
                    );
                    // The image is the control; a `Button` would add chrome
                    // around a picture.
                    crate::material::accessible_clickable(
                        gpui::div()
                            .w(px(120.))
                            .h(px(80.))
                            .flex_shrink_0()
                            .cursor_pointer()
                            .rounded(crate::material::radius_card(cx))
                            .overflow_hidden()
                            .bg(cx.theme().muted)
                            .child(
                                img(crate::store::item_image_preview(
                                    session_id.into(),
                                    entry.id.clone(),
                                    index,
                                    120.,
                                    80.,
                                ))
                                .id("tool-image")
                                .w_full()
                                .h_full()
                                .object_fit(ObjectFit::Cover)
                                .with_fallback(crate::image_viewer::unpreviewed(true, false, cx)),
                            ),
                        SharedString::from(format!("image-read-{}-{index}", entry.id)),
                        Role::Button,
                        title.clone(),
                        cx,
                    )
                    .on_click(move |_, window, cx| {
                        crate::image_viewer::open(
                            crate::image_viewer::ViewerSource::original(
                                source.clone(),
                                preview.clone(),
                            ),
                            title.clone(),
                            window,
                            cx,
                        );
                    })
                    .context_menu(move |mut menu, _, _| {
                        if let Some(path) = &path {
                            menu = menu.path_items(&path.to_string_lossy(), None);
                        }
                        copy_items
                            .iter()
                            .cloned()
                            .fold(menu, |menu, (label, text)| {
                                menu.menu(label, Box::new(CopyText(text)))
                            })
                    })
                })
        }))
}
