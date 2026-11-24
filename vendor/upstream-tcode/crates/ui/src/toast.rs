use crate::scroll::ScrollableElement as _;
use std::rc::Rc;

use crate::overlay::{Notification, NotificationType};
use crate::theme::ActiveTheme as _;
use crate::typography::TextRole as _;
use crate::widgets::button::{Button, ButtonVariants as _};
use crate::widgets::spinner::Spinner;
use crate::{icon::IconName, sizing::Sizable as _};
use gpui::{
    App, ClipboardItem, InteractiveElement as _, IntoElement, ParentElement as _, SharedString,
    Styled as _, Window, div, prelude::FluentBuilder as _,
};
use gpui_base::{h_flex, v_flex};

pub type ToastId = u64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToastKind {
    Success,
    Info,
    Warning,
    Error,
    Loading,
}

pub type ToastActionHandler = Rc<dyn Fn(&mut Window, &mut App)>;

pub struct ToastAction {
    pub label: SharedString,
    pub handler: ToastActionHandler,
}

pub struct RuntimeToastNotification;

pub fn notification(
    id: ToastId,
    kind: ToastKind,
    title: impl Into<SharedString>,
    detail: Option<SharedString>,
    action: Option<ToastAction>,
    progress: Option<f32>,
) -> Notification {
    let title: SharedString = title.into();
    let loading_title = (kind == ToastKind::Loading).then(|| title.clone());
    let mut notification = Notification::new()
        .id1::<RuntimeToastNotification>(id as usize)
        .autohide(false);
    notification = notification.compact_message(title.clone());
    if kind != ToastKind::Loading {
        notification = notification.message(title);
    }
    notification = match kind {
        ToastKind::Success => notification.with_type(NotificationType::Success),
        ToastKind::Info => notification.with_type(NotificationType::Info),
        ToastKind::Warning => notification.with_type(NotificationType::Warning),
        ToastKind::Error => notification.with_type(NotificationType::Error),
        ToastKind::Loading => notification,
    };

    if loading_title.is_some() || detail.is_some() || progress.is_some() {
        notification = notification.content(move |_, window, cx| {
            let has_loading_title = loading_title.is_some();
            let spacing = &cx.theme().tokens.spacing;
            let mut content = v_flex()
                .gap(spacing.sm)
                .when(!has_loading_title, |content| content.mt(spacing.sm))
                .when_some(loading_title.clone(), |content, title| {
                    content.child(
                        h_flex()
                            .gap(spacing.md)
                            .text_body(false, cx)
                            .child(Spinner::new())
                            .child(title),
                    )
                });
            if let Some(value) = progress {
                content = content.child(
                    crate::widgets::progress::Progress::new(("toast-progress", id as usize))
                        .value(value),
                );
            }
            if let Some(detail) = detail.clone() {
                let expanded =
                    window
                        .use_keyed_state(("toast-detail-expanded", id as usize), cx, |_, _| false);
                let is_expanded = *expanded.read(cx);
                let toggle_label = if is_expanded {
                    crate::tr!("toast.hide_details")
                } else {
                    crate::tr!("toast.show_details")
                };
                content = content.child(
                    Button::new(("toast-detail-toggle", id as usize))
                        .ghost()
                        .xsmall()
                        .label(toggle_label)
                        .on_click(window.listener_for(&expanded, |expanded, _, _, cx| {
                            *expanded = !*expanded;
                            cx.notify();
                        })),
                );
                if is_expanded {
                    content = content.child(
                        div()
                            .id(("toast-detail-scroll", id as usize))
                            .max_h_40()
                            .p(cx.theme().tokens.spacing.sm)
                            .rounded(crate::material::radius_input(cx))
                            .bg(cx.theme().muted)
                            .text_code(cx)
                            .overflow_y_scroll_area()
                            .child(detail.clone()),
                    );
                }
                if kind == ToastKind::Error {
                    content = content.child(
                        h_flex().justify_end().child(
                            Button::new(("toast-copy", id as usize))
                                .ghost()
                                .xsmall()
                                .icon(IconName::Copy)
                                .label(crate::tr!("toast.copy_error"))
                                .on_click(move |_, _, cx| {
                                    cx.write_to_clipboard(ClipboardItem::new_string(
                                        detail.to_string(),
                                    ));
                                }),
                        ),
                    );
                }
            }
            content.into_any_element()
        });
    }

    if let Some(action) = action {
        let handler = action.handler;
        let label = action.label;
        notification = notification.action(move |_, _, _| {
            let handler = handler.clone();
            Button::new(("toast-action", id as usize))
                .outline()
                .label(label.clone())
                .on_click(move |_, window, cx| handler(window, cx))
        });
    }

    notification
}
