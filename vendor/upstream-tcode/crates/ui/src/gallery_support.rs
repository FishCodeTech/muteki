//! Narrow rendering facade used by the standalone component gallery example.

use std::path::Path;

use agent::{ChangeCompleteness, FileChange, ProviderKind, TurnStatus};
use gpui::{AnyElement, App, Div, Entity, Window};
use tcode_core::notice::OrchestrateCallback;
use tcode_core::session::{SteeringStatus, TimelineEntry};

use crate::chat::components::{
    activity, assistant, bubble, changed_files, disclosure, dividers, error_card, indicator,
    subagent, work_log,
};
use crate::markdown::MarkdownState;

fn click() -> changed_files::ClickHandler {
    Box::new(|_, _, _| {})
}

pub fn section_label(label: &str) -> Div {
    crate::material::tracked_uppercase(label)
}

pub fn working_indicator(id: &str, started_at: u64, cx: &App) -> AnyElement {
    indicator::working_indicator(id.to_string().into(), Some(started_at), cx)
}

pub fn user_bubble(
    id: &str,
    text: &str,
    pending: bool,
    cwd: &Path,
    window: &mut Window,
    cx: &App,
) -> AnyElement {
    bubble::user_bubble(
        bubble::BubbleData {
            entry_id: id,
            visible: text,
            cwd,
            attachments: &[],
            steering: pending.then_some(SteeringStatus::Pending),
            compact: false,
            pinned: true,
            copied: false,
            markdown: None,
            rewind: None,
            rewind_menu: None,
        },
        bubble::BubbleHandlers {
            copy: click(),
            images: Vec::new(),
        },
        window,
        cx,
    )
}

pub fn activity(entry: &TimelineEntry, expanded: bool, cx: &App) -> AnyElement {
    activity::activity_row(entry, false, false, expanded, None, None, |_, _, _| {}, cx)
}

pub fn work_log(
    id: &str,
    index: usize,
    label: &str,
    duration: &str,
    outcome: TurnStatus,
    cx: &App,
) -> AnyElement {
    work_log::work_log(
        work_log::WorkLogData {
            compact: false,
            index,
            segment_id: id.to_string(),
            capsule_label: label.to_string(),
            duration: duration.to_string(),
            outcome,
            expanded: false,
            running: false,
            rows: Vec::new(),
        },
        |_, _, _| {},
        cx,
    )
}

pub fn subagent(entry: &TimelineEntry, cx: &App) -> AnyElement {
    subagent::subagent_row(entry, None, cx)
}

pub fn changed_files(index: usize, cwd: &Path, changes: &[FileChange], cx: &App) -> AnyElement {
    changed_files::changed_files(
        index,
        cwd,
        changes,
        ChangeCompleteness::Exact,
        false,
        changed_files::ChangedFilesHandlers {
            view_diff: click(),
            toggle_more: click(),
            open_files: changes.iter().map(|_| click()).collect(),
        },
        cx,
    )
}

pub fn assistant(id: &str, markdown: Entity<MarkdownState>, cwd: &Path, cx: &App) -> AnyElement {
    assistant::assistant(
        assistant::AssistantData {
            id,
            text: "",
            cwd,
            markdown: Some(markdown),
            compact: false,
            pinned: true,
            show_actions: true,
            copied: false,
        },
        |_, _, _| {},
        cx,
    )
}

pub fn error_card(id: &str, message: &str, cx: &App) -> AnyElement {
    error_card::error_card(id, message, false, None, |_, _, _| {}, cx)
}

pub fn relay_divider(id: &str, cx: &App) -> AnyElement {
    dividers::relay_divider(id, ProviderKind::ClaudeCode, ProviderKind::Codex, None, cx)
}

pub fn model_change_divider(id: &str, cx: &App) -> AnyElement {
    dividers::model_change_divider(
        id,
        Some("gpt-5.4"),
        "gpt-5.6-codex",
        Some("fallback recovered"),
        cx,
    )
}

pub fn callback(id: &str, callback: &OrchestrateCallback, expanded: bool, cx: &App) -> AnyElement {
    let event = disclosure::callback_event_label(callback)
        .into_owned()
        .into();
    disclosure::callback_row(id, event, callback, expanded, |_, _, _| {}, None, cx)
}
