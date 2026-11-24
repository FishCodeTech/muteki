use crate::icon::{Icon, IconName};
use crate::theme::ActiveTheme as _;
use crate::typography::TextRole as _;
use crate::widgets::menu::{ContextMenuExt as _, CopyText};
use crate::widgets::spinner::Spinner;
use gpui::{
    AnyElement, App, ClickEvent, Div, HighlightStyle, InteractiveElement as _, IntoElement,
    ParentElement as _, Role, SharedString, StatefulInteractiveElement as _, Styled as _, Window,
    div, prelude::FluentBuilder as _, px,
};
use gpui_base::{StyledExt as _, h_flex, v_flex};

use agent::{ItemContent, ItemStatus};
use tcode_core::session::{EntryContent, TimelineEntry};

use super::super::model::{one_line, one_line_with_break_markers, tool_brief};
use super::changed_files::quiet_control;
use super::command_panel::CommandPanelCache;
use super::indicator;
use super::subagent::ClickHandler;

/// Characters of a fetched whole output the detail lays out.
const FULL_OUTPUT_CHARS: usize = 256 * 1024;

/// A tool output that reached this client as a preview, and how far the
/// reader has gone in fetching the rest.
pub(crate) enum ElidedOutput {
    Preview {
        full_bytes: u64,
        on_load: ClickHandler,
    },
    Loading,
    Loaded(SharedString),
    Failed {
        error: String,
        on_load: ClickHandler,
    },
}

/// One Work Log activity row: a muted status icon + a one-line summary.
#[allow(clippy::too_many_arguments)]
pub(crate) fn activity_row(
    entry: &TimelineEntry,
    compact: bool,
    live_reasoning: bool,
    expanded: bool,
    command_detail: Option<AnyElement>,
    elided_output: Option<ElidedOutput>,
    on_toggle: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    cx: &App,
) -> AnyElement {
    let muted = cx.theme().muted_foreground;
    let expandable = matches!(
        entry.content,
        EntryContent::Item(ItemContent::CommandExecution { .. })
            | EntryContent::Item(ItemContent::ToolCall { .. })
            | EntryContent::Item(ItemContent::Reasoning { .. })
    ) && !live_reasoning;
    // The icon says WHAT the row did — success or failure is carried by the
    // badge in `activity_status_icon`, never by the glyph itself.
    let (status, icon, summary): (ItemStatus, Icon, AnyElement) = match &entry.content {
        EntryContent::Item(ItemContent::CommandExecution {
            command, status, ..
        }) => {
            let (preview, breaks) = one_line_with_break_markers(command);
            let marker_style = HighlightStyle {
                color: Some(muted.opacity(0.45)),
                ..Default::default()
            };
            let summary = activity_summary(
                binary_name(command),
                Some(
                    crate::widgets::fade_text(preview)
                        .with_highlights(breaks.into_iter().map(|range| (range, marker_style))),
                ),
                true,
                cx,
            );
            (*status, Icon::new(IconName::SquareTerminal), summary)
        }
        EntryContent::Item(ItemContent::ToolCall {
            name,
            input,
            output,
            status,
            ..
        }) => {
            let mut brief = tool_brief(input);
            if brief.is_empty()
                && let Some(output) = output
            {
                brief = one_line(output);
            }
            let summary = activity_summary(name.clone(), argument(brief), true, cx);
            (*status, Icon::empty().path("icons/wrench.svg"), summary)
        }
        EntryContent::Item(ItemContent::WebSearch { query }) => {
            let summary = activity_summary("web_search", argument(one_line(query)), false, cx);
            (ItemStatus::Completed, Icon::new(IconName::Globe), summary)
        }
        EntryContent::Item(ItemContent::Other {
            provider_kind,
            summary: item_summary,
        }) => {
            let summary = activity_summary(
                provider_kind.clone(),
                argument(one_line(item_summary)),
                false,
                cx,
            );
            (ItemStatus::Completed, Icon::new(IconName::Bot), summary)
        }
        EntryContent::Item(ItemContent::Reasoning { .. }) => {
            if live_reasoning {
                let summary = indicator::shimmer_label(
                    SharedString::from(format!("thinking-{}", entry.id)),
                    crate::tr!("chat.thinking_live"),
                    cx,
                );
                (ItemStatus::InProgress, Icon::new(IconName::Loader), summary)
            } else {
                let summary = div()
                    .min_w_0()
                    .overflow_hidden()
                    .child(crate::widgets::fade_text(crate::tr!("chat.thinking_done")))
                    .into_any_element();
                (
                    ItemStatus::Completed,
                    Icon::empty().path("icons/sparkles.svg"),
                    summary,
                )
            }
        }
        _ => (
            ItemStatus::Completed,
            Icon::new(IconName::Info),
            div().into_any_element(),
        ),
    };

    let row = h_flex()
        .w_full()
        .min_h(px(crate::material::bar_height(
            crate::material::Bar::Section,
            false,
        )))
        .px_1()
        .gap_2()
        .items_center()
        .when(!compact, |row| row.py_0p5())
        .text_body(false, cx)
        .child(activity_status_icon(icon, status, cx))
        .child(summary)
        .when(expandable, |row| {
            row.child(Icon::new(chevron(expanded)).text_color(muted))
        });

    let row: AnyElement = if expandable {
        crate::material::accessible_clickable(
            row,
            SharedString::from(format!("activity-row-{}", entry.id)),
            Role::Button,
            crate::tr!("chat.activity_details"),
            cx,
        )
        .debug_selector({
            let id = entry.id.clone();
            move || format!("activity-row-{id}")
        })
        .aria_expanded(expanded)
        .rounded(crate::material::radius_chip(cx))
        .cursor_pointer()
        .hover(|row| row.bg(cx.theme().accent))
        .on_click(on_toggle)
        .into_any_element()
    } else {
        row.into_any_element()
    };

    let mut block = v_flex().w_full().gap_1().child(row);
    if expanded && expandable {
        block = block.child(activity_detail(entry, command_detail, elided_output, cx));
    }
    let copy_items = activity_copy_items(entry);
    block
        .context_menu(move |menu, _, _| {
            copy_items
                .iter()
                .cloned()
                .fold(menu, |menu, (label, text)| {
                    menu.menu(label, Box::new(CopyText(text)))
                })
        })
        .into_any_element()
}

/// What the row's context menu copies: the detail's parts are plain text, so
/// a text selection cannot reach them, and a reader wants the whole
/// command or output rather than its one-line preview.
pub(super) fn activity_copy_items(entry: &TimelineEntry) -> Vec<(String, String)> {
    let mut items = Vec::new();
    let mut push = |key: &str, text: &str| {
        if !text.trim().is_empty() {
            items.push((crate::tr!(key).into_owned(), text.to_string()));
        }
    };
    match &entry.content {
        EntryContent::Item(ItemContent::CommandExecution {
            command, output, ..
        }) => {
            push("chat.copy_command", command);
            push("chat.copy_output", output);
        }
        EntryContent::Item(ItemContent::ToolCall { input, output, .. }) => {
            let input = serde_json::to_string_pretty(input).unwrap_or_else(|_| input.to_string());
            push("chat.copy_input", &input);
            push("chat.copy_output", output.as_deref().unwrap_or_default());
        }
        EntryContent::Item(ItemContent::Reasoning { text }) => push("chat.copy_text", text),
        EntryContent::Item(ItemContent::WebSearch { query }) => push("chat.copy_text", query),
        EntryContent::Item(ItemContent::Other { summary, .. }) => push("chat.copy_text", summary),
        _ => {}
    }
    items
}

/// A row summary in the tool-chips grammar: the bare verb, then its argument
/// as plain inline text. No surface of its own — the row packs left so the
/// chevron sits right after the words, and only the argument shrinks.
fn activity_summary(
    primary: impl IntoElement,
    argument: Option<crate::widgets::FadeText>,
    monospace: bool,
    cx: &App,
) -> AnyElement {
    h_flex()
        .min_w_0()
        .gap_2()
        .overflow_hidden()
        .child(
            div()
                .flex_none()
                .font_medium()
                .text_color(cx.theme().foreground)
                .child(primary),
        )
        .when_some(argument, |row, argument| {
            row.child(
                div()
                    .min_w_0()
                    .text_secondary(false, cx)
                    .when(monospace, |text| {
                        text.font_family(cx.theme().mono_font_family.clone())
                    })
                    .overflow_hidden()
                    .child(argument),
            )
        })
        .into_any_element()
}

/// The command's first token, which is what a reader scans for. Falls back to
/// the whole (whitespace-only) command so the row is never left wordless.
fn binary_name(command: &str) -> String {
    command
        .split_whitespace()
        .next()
        .unwrap_or(command)
        .to_string()
}

fn argument(text: String) -> Option<crate::widgets::FadeText> {
    (!text.is_empty()).then(|| crate::widgets::fade_text(text))
}

fn activity_detail(
    entry: &TimelineEntry,
    command_detail: Option<AnyElement>,
    elided_output: Option<ElidedOutput>,
    cx: &App,
) -> AnyElement {
    let detail = match &entry.content {
        EntryContent::Item(ItemContent::CommandExecution {
            command, output, ..
        }) => command_detail.unwrap_or_else(|| {
            CommandPanelCache::new().render(entry, command, output, Default::default(), None, cx)
        }),
        EntryContent::Item(ItemContent::ToolCall { input, output, .. }) => {
            let mut input_brief = tool_brief(input);
            if input_brief.is_empty() {
                input_brief = one_line(&input.to_string());
            }
            let input_brief = truncate_chars(&input_brief, 240);
            v_flex()
                .w_full()
                .gap_2()
                .child(activity_detail_section(
                    crate::tr!("chat.input").into_owned(),
                    input_brief,
                    true,
                    cx,
                ))
                .when_some(output.clone(), |detail, output| {
                    let full = match &elided_output {
                        Some(ElidedOutput::Loaded(full)) => Some(full.clone()),
                        _ => None,
                    };
                    detail.child(match full {
                        Some(full) => full_output_section(full, entry, cx),
                        None => activity_detail_section(
                            crate::tr!("chat.output").into_owned(),
                            truncate_chars(&output, 240),
                            false,
                            cx,
                        ),
                    })
                })
                .when_some(elided_output, |detail, elided| {
                    detail.children(elided_output_control(elided, entry, cx))
                })
                .into_any_element()
        }
        EntryContent::Item(ItemContent::Reasoning { text }) => div()
            .w_full()
            .text_secondary(false, cx)
            .whitespace_normal()
            .child(text.clone())
            .into_any_element(),
        _ => div().into_any_element(),
    };
    crate::material::rail_detail(detail, cx)
        .debug_selector(|| "activity-detail".into())
        .text_token(&cx.theme().tokens.typography.xs)
        .into_any_element()
}

/// The type icon at row scale. Failure never recolors the glyph — it hangs a
/// small red X off the bottom-right corner, on a disc of the row background so
/// the badge stays legible over whatever it overlaps.
fn activity_status_icon(icon: Icon, status: ItemStatus, cx: &App) -> AnyElement {
    if matches!(status, ItemStatus::InProgress) {
        return Spinner::new().color(cx.theme().primary).into_any_element();
    }
    let icon = icon
        .text_color(cx.theme().muted_foreground)
        .into_any_element();
    if matches!(status, ItemStatus::Completed) {
        return icon;
    }
    div()
        .relative()
        .flex_none()
        .child(icon)
        .child(
            div()
                .absolute()
                .right(px(-3.))
                .bottom(px(-3.))
                .rounded_full()
                .bg(cx.theme().background)
                .child(
                    Icon::new(IconName::CircleX)
                        .size(px(7.))
                        .text_color(cx.theme().danger),
                ),
        )
        .into_any_element()
}

fn activity_detail_section(label: String, text: String, monospace: bool, cx: &App) -> Div {
    let muted = cx.theme().muted_foreground;
    v_flex()
        .w_full()
        .gap_1()
        .child(
            div()
                .text_size(px(10.5))
                .font_medium()
                .text_color(muted)
                .child(crate::material::tracked_uppercase(&label)),
        )
        .child(
            div()
                .w_full()
                .text_secondary(false, cx)
                .when(monospace, |body| {
                    body.font_family(cx.theme().mono_font_family.clone())
                })
                .child(if text.is_empty() {
                    " ".to_string()
                } else {
                    text
                }),
        )
}

fn full_output_section(full: SharedString, entry: &TimelineEntry, cx: &App) -> Div {
    let muted = cx.theme().muted_foreground;
    let shown: SharedString = if full.chars().count() > FULL_OUTPUT_CHARS {
        let head: String = full.chars().take(FULL_OUTPUT_CHARS).collect();
        format!("{head}…").into()
    } else {
        full
    };
    v_flex()
        .w_full()
        .gap_1()
        .child(
            div()
                .text_size(px(10.5))
                .font_medium()
                .text_color(muted)
                .child(crate::material::tracked_uppercase(&crate::tr!(
                    "chat.full_output"
                ))),
        )
        .child(
            div()
                .id(SharedString::from(format!("full-output-{}", entry.id)))
                .w_full()
                .max_h(px(360.))
                .overflow_y_scroll()
                .text_secondary(false, cx)
                .font_family(cx.theme().mono_font_family.clone())
                .whitespace_normal()
                .child(shown),
        )
}

fn elided_output_control(
    elided: ElidedOutput,
    entry: &TimelineEntry,
    cx: &App,
) -> Option<AnyElement> {
    let id = SharedString::from(format!("load-output-{}", entry.id));
    let note = |text: String| {
        div()
            .text_secondary(false, cx)
            .child(text)
            .into_any_element()
    };
    Some(match elided {
        ElidedOutput::Preview {
            full_bytes,
            on_load,
        } => h_flex()
            .gap_2()
            .items_center()
            .child(note(
                crate::tr!(
                    "chat.output_elided",
                    size = crate::thread_export::format_size(full_bytes as usize)
                )
                .into_owned(),
            ))
            .child(quiet_control(
                id,
                crate::tr!("chat.load_full_output").into_owned().into(),
                on_load,
                cx,
            ))
            .into_any_element(),
        ElidedOutput::Loading => note(crate::tr!("chat.full_output_loading").into_owned()),
        ElidedOutput::Loaded(_) => return None,
        ElidedOutput::Failed { error, on_load } => h_flex()
            .gap_2()
            .items_center()
            .child(note(
                crate::tr!("chat.full_output_failed", error = error).into_owned(),
            ))
            .child(quiet_control(
                id,
                crate::tr!("chat.load_full_output").into_owned().into(),
                on_load,
                cx,
            ))
            .into_any_element(),
    })
}

fn truncate_chars(text: &str, max: usize) -> String {
    let text = one_line(text);
    if text.chars().count() <= max {
        return text;
    }
    let head: String = text.chars().take(max).collect();
    format!("{head}…")
}

fn chevron(open: bool) -> IconName {
    if open {
        IconName::ChevronDown
    } else {
        IconName::ChevronRight
    }
}
