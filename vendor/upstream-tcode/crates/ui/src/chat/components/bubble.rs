use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::icon::{Icon, IconName};
use crate::theme::ActiveTheme as _;
use crate::typography::TextRole as _;
use crate::widgets::menu::{ContextMenuExt as _, PopupMenu};
use agent::RewindMode;
use gpui::{
    Anchor, AnyElement, App, ClickEvent, Entity, InteractiveElement as _, IntoElement as _,
    ObjectFit, ParentElement as _, Role, SharedString, StatefulInteractiveElement as _,
    Styled as _, StyledImage as _, Window, div, img, prelude::FluentBuilder as _, px,
};
use gpui_base::{h_flex, v_flex};
use std::rc::Rc;

use tcode_core::session::SteeringStatus;

use super::super::RewindTurn;
use super::assistant;
use crate::markdown::{MarkdownState, MarkdownView, MenuExtension};
use crate::widgets::copy::{action_button, copy_button};

pub(crate) type ClickHandler = Box<dyn Fn(&ClickEvent, &mut Window, &mut App) + 'static>;
pub(crate) type SharedClickHandler = Arc<dyn Fn(&ClickEvent, &mut Window, &mut App) + 'static>;
pub(crate) type UserMessageArgs<'a> = (
    usize,
    &'a str,
    &'a str,
    &'a Path,
    Option<usize>,
    &'a [String],
    Option<SteeringStatus>,
    bool,
);

pub(crate) struct BubbleData<'a> {
    pub(crate) entry_id: &'a str,
    pub(crate) visible: &'a str,
    pub(crate) cwd: &'a Path,
    pub(crate) attachments: &'a [String],
    pub(crate) steering: Option<SteeringStatus>,
    pub(crate) pinned: bool,
    pub(crate) compact: bool,
    pub(crate) copied: bool,
    pub(crate) markdown: Option<Entity<MarkdownState>>,
    pub(crate) rewind: Option<AnyElement>,
    /// The rewind the context menu offers, when the harness has one: the
    /// turn, whether it is blocked right now, and whether a conversation
    /// rewind is possible (there is none for the first turn).
    pub(crate) rewind_menu: Option<RewindMenu>,
}

#[derive(Clone, Copy)]
pub(crate) struct RewindMenu {
    pub(crate) turn: usize,
    pub(crate) disabled: bool,
    pub(crate) conversation_available: bool,
}

/// The bubble's context-menu items: copy the message, then the rewind modes
/// the popover button offers. Shared by the Markdown inside the bubble and
/// the bubble's own margin.
fn bubble_menu_items(copy_text: Arc<str>, rewind: Option<RewindMenu>) -> MenuExtension {
    Rc::new(move |menu: PopupMenu, window: &mut Window, cx: &mut App| {
        let menu = if copy_text.trim().is_empty() {
            menu
        } else {
            assistant::copy_message_items(copy_text.clone())(menu, window, cx)
        };
        let Some(RewindMenu {
            turn,
            disabled,
            conversation_available,
        }) = rewind
        else {
            return menu;
        };
        let mut modes = Vec::new();
        if conversation_available {
            modes.push((
                crate::tr!("chat.rewind_all").into_owned(),
                RewindMode::FilesAndConversation,
            ));
            modes.push((
                crate::tr!("chat.rewind_conversation").into_owned(),
                RewindMode::Conversation,
            ));
        }
        modes.push((
            crate::tr!("chat.rewind_files").into_owned(),
            RewindMode::Files,
        ));
        modes
            .into_iter()
            .fold(menu.separator(), |menu, (label, mode)| {
                menu.menu_with_enable(label, Box::new(RewindTurn { turn, mode }), !disabled)
            })
    })
}

pub(crate) struct BubbleHandlers {
    pub(crate) copy: ClickHandler,
    pub(crate) images: Vec<ClickHandler>,
}

pub(crate) struct RewindHandlers {
    pub(crate) files_and_conversation: SharedClickHandler,
    pub(crate) conversation: SharedClickHandler,
    pub(crate) files: SharedClickHandler,
}

pub(crate) fn native_rewind_button(
    turn: usize,
    conversation_available: bool,
    (state, compact): (Option<(bool, bool)>, bool),
    handlers: RewindHandlers,
    cx: &App,
) -> Option<AnyElement> {
    let (available, disabled) = state?;
    if !available {
        return None;
    }

    let trigger = action_button(
        SharedString::from(format!("rewind-{turn}")),
        IconName::Undo,
        if disabled {
            crate::tr!("chat.rewind_blocked").into_owned()
        } else {
            crate::tr!("chat.rewind").into_owned()
        },
        cx,
    )
    .disabled(disabled)
    .touch(compact);
    Some(
        crate::material::overlay_popover(("rewind-menu", turn), cx)
            .anchor(Anchor::TopRight)
            .when(compact, |popover| {
                popover.bottom_sheet(crate::tr!("chat.rewind"))
            })
            .trigger(trigger)
            .content(move |_state, _window, cx| {
                let muted = cx.theme().muted_foreground;
                let hover = cx.theme().muted;
                let popover = cx.entity();
                let RewindHandlers {
                    files_and_conversation,
                    conversation,
                    files,
                } = &handlers;
                let mut modes = Vec::new();
                if conversation_available {
                    modes.push((
                        crate::tr!("chat.rewind_all").into_owned(),
                        files_and_conversation.clone(),
                    ));
                    modes.push((
                        crate::tr!("chat.rewind_conversation").into_owned(),
                        conversation.clone(),
                    ));
                }
                modes.push((crate::tr!("chat.rewind_files").into_owned(), files.clone()));
                let mut menu = v_flex()
                    .w(px(240.))
                    .p_1()
                    .gap_0p5()
                    .id(("rewind-options", turn))
                    .role(Role::Menu)
                    .aria_label(crate::tr!("chat.rewind"));
                for (index, (label, on_select)) in modes.into_iter().enumerate() {
                    let popover = popover.clone();
                    menu = menu.child(
                        crate::material::accessible_clickable(
                            crate::material::menu_item_frame(compact, cx),
                            ("rewind-option", index),
                            Role::MenuItem,
                            label.clone(),
                            cx,
                        )
                        .w_full()
                        .items_center()
                        .cursor_pointer()
                        .text_body(compact, cx)
                        .hover(move |style| style.bg(hover))
                        .child(Icon::new(IconName::Undo).text_color(muted))
                        .child(label)
                        .on_click(move |event, window, cx| {
                            popover.update(cx, |state, cx| state.dismiss(window, cx));
                            on_select(event, window, cx);
                        }),
                    );
                }
                menu.into_any_element()
            })
            .into_any_element(),
    )
}

/// A user message: right-aligned bubble, attachment thumbnails, and actions.
pub(crate) fn user_bubble(
    data: BubbleData<'_>,
    handlers: BubbleHandlers,
    window: &mut Window,
    cx: &App,
) -> AnyElement {
    let BubbleData {
        entry_id,
        visible,
        cwd,
        attachments,
        steering,
        pinned,
        compact,
        copied,
        markdown,
        rewind,
        rewind_menu,
    } = data;
    let BubbleHandlers { copy, images } = handlers;
    let menu_items = bubble_menu_items(Arc::from(visible), rewind_menu);
    let prose = cx.theme().tokens.typography.md;
    let spacing = &cx.theme().tokens.spacing;
    let text_style = window.text_style();
    let text_width = visible.lines().fold(px(0.), |width, line| {
        let run = text_style.to_run(line.len());
        width.max(
            window
                .text_system()
                // Must match the bubble's rendered text size below.
                .layout_line(line, prose.size, &[run], None)
                .width,
        )
    });

    let group_key = SharedString::from(format!("user-{entry_id}"));
    let mut actions = h_flex().gap(px(2.)).items_center().justify_end();
    if !visible.trim().is_empty() {
        actions = actions.child(copy_button(
            &format!("user:{entry_id}"),
            copied,
            compact,
            copy,
            cx,
        ));
    }
    if let Some(rewind) = rewind {
        actions = actions.child(rewind);
    }

    let thumbnails = (!attachments.is_empty()).then(|| {
        h_flex().gap_2().justify_end().flex_wrap().children(
            attachments
                .iter()
                .zip(images)
                .enumerate()
                .map(|(image_index, (path, on_click))| {
                    let menu_path = path.clone();
                    let path = PathBuf::from(path);
                    // The image is the control; a `Button` would add chrome
                    // around a picture.
                    crate::material::accessible_clickable(
                        div(),
                        SharedString::from(format!("user-image-{entry_id}-{image_index}")),
                        Role::Button,
                        crate::tr!("markdown.open_image").into_owned(),
                        cx,
                    )
                    .size(px(120.))
                    .rounded(crate::material::radius_card(cx))
                    .overflow_hidden()
                    .bg(cx.theme().muted)
                    .cursor_pointer()
                    .child(
                        img(crate::store::image_preview(path, 120., true))
                            .id("attachment-image")
                            .size(px(120.))
                            .rounded(crate::material::radius_card(cx))
                            .object_fit(ObjectFit::Cover)
                            .with_fallback(crate::image_viewer::unpreviewed(true, false, cx)),
                    )
                    .on_click(on_click)
                    .context_menu(move |menu, _, _| menu.path_items(&menu_path, None))
                })
                .collect::<Vec<_>>(),
        )
    });

    let content = markdown.map_or_else(
        || div().child(visible.to_string()).into_any_element(),
        |markdown| {
            MarkdownView::new(&markdown)
                .selectable(true)
                .compact_headings(true)
                .base_dir(cwd)
                .menu_extension(menu_items.clone())
                .into_any_element()
        },
    );
    v_flex()
        .group(group_key.clone())
        .w_full()
        .items_end()
        .gap(px(2.))
        .when_some(steering, |column, steering| {
            let selector = format!("steering-{entry_id}");
            column.child(
                crate::material::semantic_chip(
                    match steering {
                        SteeringStatus::Pending => crate::tr!("chat.steering"),
                        SteeringStatus::Accepted => crate::tr!("chat.steered"),
                    },
                    cx.theme().muted,
                    cx.theme().muted_foreground,
                    cx,
                )
                .debug_selector(move || selector),
            )
        })
        .children(thumbnails)
        .when(!visible.trim().is_empty(), |column| {
            column.child({
                let pending = steering == Some(SteeringStatus::Pending);
                div()
                    .w((text_width + spacing.md * 2.).ceil() + px(3.))
                    .flex_none()
                    .max_w_3_4()
                    .px(spacing.md)
                    .py(spacing.xs)
                    .rounded(crate::material::radius_composer(cx))
                    .bg(cx.theme().foreground.opacity(0.08))
                    .when(pending, |bubble| {
                        bubble
                            .border_1()
                            .border_dashed()
                            .border_color(cx.theme().border)
                    })
                    .text_color(cx.theme().foreground)
                    .text_token(&prose)
                    .child(content)
            })
        })
        .child(assistant::reserve_action_row(
            actions, group_key, pinned, compact,
        ))
        .context_menu(move |menu, window, cx| menu_items(menu, window, cx))
        .into_any_element()
}
