use crate::scroll::ScrollableElement as _;
use std::borrow::Cow;

use crate::icon::{Icon, IconName};
use crate::sizing::Sizable as _;
use crate::theme::ActiveTheme as _;
use crate::typography::TextRole as _;
use crate::widgets::button::{Button, ButtonVariants as _};
use gpui::{
    AnyElement, App, ClickEvent, Div, InteractiveElement as _, IntoElement as _,
    ParentElement as _, SharedString, StatefulInteractiveElement as _, Styled as _, Window, div,
    px,
};
use gpui_base::{Link, h_flex, v_flex};

use tcode_core::notice::{CallbackEvent, HostNotice, OrchestrateCallback};

use super::super::model::one_line;

const DISCLOSURE_CARD_MAX_HEIGHT: f32 = 320.;

/// One line naming a notice. A row per callback names only its callback.
pub(crate) fn notice_label(notice: &HostNotice) -> String {
    match notice {
        HostNotice::Orchestrate(callbacks) => callbacks
            .iter()
            .map(callback_label)
            .collect::<Vec<_>>()
            .join(" · "),
        HostNotice::PullRequest { number, notice } => crate::tr!(
            "pull_requests.update_label",
            number = number.to_string(),
            kinds = crate::pull_requests::watch_notice_kinds(notice)
        )
        .into_owned(),
        HostNotice::ReportReminder => crate::tr!("chat.report_reminder").into_owned(),
        HostNotice::Control(messages) => messages.join(" · "),
    }
}

pub(crate) fn callback_label(callback: &OrchestrateCallback) -> String {
    format!(
        "{} {}",
        one_line(&callback.title),
        callback_event_label(callback)
    )
}

/// What became of the callback's child, which its row keeps in view however
/// long the child's title is.
pub(crate) fn callback_event_label(callback: &OrchestrateCallback) -> Cow<'static, str> {
    match &callback.event {
        CallbackEvent::Ended(state) => localized_callback_state(state),
        CallbackEvent::Approval => crate::tr!("chat.orchestrate_state_approval"),
        CallbackEvent::Question => crate::tr!("chat.orchestrate_state_question"),
    }
}

/// `on_open` opens the child the callback reports on, while it still exists.
/// `event` follows the child's title and does not fade with it.
pub(crate) fn callback_row(
    key: &str,
    event: SharedString,
    callback: &OrchestrateCallback,
    expanded: bool,
    on_toggle: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    on_open: Option<super::subagent::ClickHandler>,
    cx: &App,
) -> AnyElement {
    let body = if callback.body.trim().is_empty() {
        crate::tr!("chat.orchestrate_callback_empty").into_owned()
    } else {
        callback.body.clone()
    };
    let open = on_open.map(|on_open| {
        let ring = crate::material::focus_ring(cx);
        Link::new(SharedString::from(format!("callback-open-{key}")))
            .accessibility_label(SharedString::from(crate::tr!("agents.open").into_owned()))
            .on_activate(on_open)
            .flex_none()
            .text_token(&cx.theme().tokens.typography.xs)
            .text_color(cx.theme().primary)
            .cursor_pointer()
            .hover(|link| link.underline())
            .focus_visible(move |style| style.shadow(vec![ring]))
            .child(crate::tr!("agents.open_child"))
            .into_any_element()
    });
    let title = one_line(&callback.title).into();
    disclosure_with(
        key,
        title,
        Some(event),
        &body,
        expanded,
        on_toggle,
        open,
        cx,
    )
}

/// A centered disclosure notification with a height-capped verbatim body: the
/// divider grammar's stub–label–stub row, with the label clickable to expand.
/// Orchestrate context and child-thread results read as ambient notifications
/// of the flow — the same standing as a relay or model-change divider.
pub(crate) fn disclosure(
    key: &str,
    label: SharedString,
    full_text: &str,
    expanded: bool,
    on_toggle: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    cx: &App,
) -> AnyElement {
    disclosure_with(key, label, None, full_text, expanded, on_toggle, None, cx)
}

#[allow(clippy::too_many_arguments)]
fn disclosure_with(
    key: &str,
    label: SharedString,
    suffix: Option<SharedString>,
    full_text: &str,
    expanded: bool,
    on_toggle: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    trailing: Option<AnyElement>,
    cx: &App,
) -> AnyElement {
    let muted = cx.theme().muted_foreground;
    // The label is a child rather than `.label()`: the divider grammar keeps it
    // muted, and `Button` paints its label in the variant's foreground.
    let toggle = Button::new(SharedString::from(format!("disclosure-{key}")))
        .ghost()
        .small()
        .aria_label(match &suffix {
            Some(suffix) => format!("{label} {suffix}").into(),
            None => label.clone(),
        })
        .aria_expanded(expanded)
        // A long notice (a `[control]` message is one) truncates inside the row instead of
        // spilling past both edges of the centered divider.
        .max_w_full()
        .overflow_hidden()
        .text_token(&cx.theme().tokens.typography.xs)
        .icon(Icon::new(chevron(expanded)).text_color(muted))
        .child(
            h_flex()
                .min_w_0()
                .gap_1()
                .text_color(muted)
                .child(
                    div()
                        .min_w_0()
                        .overflow_hidden()
                        .child(crate::widgets::fade_text(label)),
                )
                .children(suffix.map(|suffix| div().flex_none().child(suffix))),
        )
        .on_click(on_toggle)
        .debug_selector({
            let selector = format!("disclosure-{key}");
            move || selector
        });

    let row = h_flex()
        .w_full()
        .items_center()
        .justify_center()
        .gap_2()
        .child(super::dividers::divider_stub(cx))
        .child(toggle)
        .children(trailing)
        .child(super::dividers::divider_stub(cx));

    let mut block = v_flex().w_full().gap_1().child(row);
    if expanded {
        block = block.child(disclosure_body(key, full_text, cx));
    }
    block.into_any_element()
}

pub(crate) fn disclosure_body(key: &str, full_text: &str, cx: &App) -> Div {
    let muted = cx.theme().muted_foreground;
    crate::material::card(cx)
        .w_full()
        // The card keeps clicks to itself, but a pan that its body cannot use
        // must still reach the timeline behind it.
        .block_mouse_except_scroll()
        .child(
            div()
                .id(SharedString::from(format!("disclosure-body-{key}")))
                .w_full()
                .max_h(px(DISCLOSURE_CARD_MAX_HEIGHT))
                .overflow_y_scroll_area()
                .child(
                    div()
                        .w_full()
                        .text_body(false, cx)
                        .text_color(muted)
                        .child(full_text.to_string()),
                ),
        )
}

fn localized_callback_state(state: &str) -> Cow<'static, str> {
    match state {
        "completed" => crate::tr!("chat.orchestrate_state_completed"),
        "reported" => crate::tr!("chat.orchestrate_state_reported"),
        "updated" => crate::tr!("chat.orchestrate_state_updated"),
        "unreported" => crate::tr!("chat.orchestrate_state_unreported"),
        "failed" => crate::tr!("chat.orchestrate_state_failed"),
        other => Cow::Owned(other.to_string()),
    }
}

fn chevron(open: bool) -> IconName {
    if open {
        IconName::ChevronDown
    } else {
        IconName::ChevronRight
    }
}
