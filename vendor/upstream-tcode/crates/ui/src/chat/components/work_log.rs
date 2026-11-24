use std::path::Path;

use crate::icon::{Icon, IconName};
use crate::material::{Bar, TOUCH_TARGET, bar_height};
use crate::theme::ActiveTheme as _;
use crate::typography::TextRole as _;
use crate::widgets::spinner::Spinner;
use agent::TurnStatus;
use gpui::{
    AnyElement, App, ClickEvent, Div, InteractiveElement as _, IntoElement as _,
    ParentElement as _, Role, SharedString, StatefulInteractiveElement as _, Styled as _, Window,
    div, prelude::FluentBuilder as _, px,
};
use gpui_base::{h_flex, v_flex};

use tcode_core::session::{TimelineEntry, TurnMeta};

use super::super::model::RowPart;

pub(crate) type WorkLogArgs<'a> = (
    usize,
    &'a RowPart,
    &'a TurnMeta,
    &'a Path,
    &'a [&'a TimelineEntry],
    bool,
);

pub(crate) struct WorkLogData {
    pub(crate) index: usize,
    pub(crate) compact: bool,
    pub(crate) segment_id: String,
    pub(crate) capsule_label: String,
    pub(crate) duration: String,
    pub(crate) outcome: TurnStatus,
    pub(crate) expanded: bool,
    pub(crate) running: bool,
    pub(crate) rows: Vec<AnyElement>,
}

/// One work log as a naked disclosure: a hugging header row, then the trace
/// itself in the flow. No capsule — execution traces carry no surface.
pub(crate) fn work_log(
    data: WorkLogData,
    on_toggle: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    cx: &App,
) -> AnyElement {
    let WorkLogData {
        index,
        compact,
        segment_id,
        capsule_label,
        duration,
        outcome,
        expanded,
        running,
        rows,
    } = data;
    let muted = cx.theme().muted_foreground;
    // Not a `Button`: on a touch layout the label wraps, and `Button` lays its
    // content out on one line.
    let header = crate::material::accessible_clickable(
        h_flex(),
        SharedString::from(format!("worklog-header-{index}-{segment_id}")),
        Role::Button,
        capsule_label.clone(),
        cx,
    )
    .debug_selector({
        let selector = format!("worklog-header-{index}-{segment_id}");
        move || selector
    })
    .aria_expanded(expanded)
    .self_start()
    .h(px(bar_height(Bar::Section, false)))
    .when(compact, |row| row.h_auto().min_h(px(TOUCH_TARGET)))
    .when(compact, |row| row.max_w_full().flex_wrap())
    .px_1p5()
    .gap_1p5()
    .items_center()
    .rounded(crate::material::radius_button(cx))
    .text_body(false, cx)
    .text_color(muted)
    .cursor_pointer()
    .hover(|row| row.bg(cx.theme().accent))
    .on_click(on_toggle)
    .child(Icon::new(chevron(expanded)).text_color(muted))
    .child(capsule_label)
    .when(running, |row| {
        row.child(Spinner::new().color(cx.theme().primary))
    })
    .when(!running, |row| {
        // A settled run needs no badge; only a bad ending is called out.
        let failure = match outcome {
            TurnStatus::Completed => None,
            TurnStatus::Failed => Some(crate::tr!("chat.work_log_failed")),
            TurnStatus::Interrupted => Some(crate::tr!("chat.work_log_interrupted")),
        };
        row.when_some(failure, |row, label| {
            row.child(crate::material::semantic_chip(
                label,
                cx.theme().danger.opacity(0.12),
                cx.theme().danger,
                cx,
            ))
        })
        .child(
            div()
                .flex_none()
                .text_meta_mono(false, cx)
                // Baseline compensation: flex can't baseline-align text boxes,
                // and the smaller digits sit visibly high beside the label.
                .relative()
                .top(px(1.))
                .child(duration),
        )
    });

    v_flex()
        .w_full()
        .gap_1()
        .child(header)
        .when(expanded && !rows.is_empty(), |flow| {
            flow.child(work_log_body(rows))
        })
        .into_any_element()
}

/// Folded activity rows under an expanded header.
pub(crate) fn work_log_body(rows: Vec<AnyElement>) -> Div {
    // Rows line up under the header label: 6px header padding + 14px chevron
    // (the body text size) + 6px gap, minus the 4px an activity row keeps for
    // its hover pill.
    v_flex().w_full().gap_1().pl(px(22.)).children(rows)
}

fn chevron(open: bool) -> IconName {
    if open {
        IconName::ChevronDown
    } else {
        IconName::ChevronRight
    }
}
