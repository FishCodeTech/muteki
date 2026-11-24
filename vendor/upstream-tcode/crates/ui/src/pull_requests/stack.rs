//! A GitHub native stack as the thread shows it: the stack map behind the page's layer
//! selector, what Merge stack and Rebase stack are offered as, their confirmations, the rebase's
//! progress and the words every answer gets. The host owns every write and its state; the map
//! reads only the stored topology and links, and each confirmation reads GitHub fresh.

use std::rc::Rc;

use gpui::{
    AnyElement, App, AppContext as _, ClipboardItem, Context, Entity, Hsla,
    InteractiveElement as _, IntoElement, ParentElement as _, Render, SharedString,
    StatefulInteractiveElement as _, Styled as _, Window, div, prelude::FluentBuilder as _, px,
};
use gpui_base::{h_flex, v_flex};
use tcode_core::pull_request::{
    self as core_pr, GITHUB, PullRequestKey, PullRequestMergeMethod, PullRequestStackOperation,
    PullRequestState, StackBlocker, StackLayerCondition, StackLayerRole, StackMap,
    StackOperationKind, StackRebaseFailure, StackRebaseGitStep, StackRebaseStep,
    ThreadPullRequestLink,
};
use tcode_protocol::{
    Command, PullRequestActionResult, PullRequestActionState, PullRequestRead,
    PullRequestReadResponse, PullRequestRejection, PullRequestStackActionState,
    PullRequestStackHead, PullRequestStackPushAccess,
};

use super::{
    appearance,
    compose::{answer_of, rejection_reason},
    lifecycle::{Lifecycle, Target, method_label},
    row_state,
};
use crate::{
    icon::{Icon, IconName},
    material,
    overlay::{Notification, OverlayExt as _},
    sizing::Sizable as _,
    store::WorkspaceStore,
    theme::ActiveTheme as _,
    typography::TextRole as _,
    widgets::Popover,
    widgets::{
        button::{Button, ButtonVariants as _},
        checkbox::Checkbox,
        choice::{Choice, ChoiceGroup},
        spinner::Spinner,
        tooltip::Tooltip,
    },
};

/// Whether a stack write is offered, and why not when it is shown disabled.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Avail {
    Hidden,
    Disabled(String),
    Enabled,
}

/// The native stack a pull request the thread shows is a layer of, with the stack's operation.
#[derive(Clone)]
pub(super) struct StackOffer {
    pub(super) key: PullRequestKey,
    links: Rc<Vec<ThreadPullRequestLink>>,
    pub(super) operation: Option<PullRequestStackOperation>,
    /// The thread links the selected layer itself: only then do its stack's writes go from it.
    pub(super) linked: bool,
}

pub(super) type StackId = (String, String, u64);

impl StackOffer {
    pub(super) fn new(
        key: &PullRequestKey,
        links: &[ThreadPullRequestLink],
        operations: &[PullRequestStackOperation],
    ) -> Option<Self> {
        core_pr::stack_map(links, key)?;
        Some(Self {
            key: key.clone(),
            linked: links.iter().any(|link| link.visible() && link.key == *key),
            operation: core_pr::stack_operation(operations, links, key).cloned(),
            links: Rc::new(links.to_vec()),
        })
    }

    pub(super) fn map(&self) -> StackMap<'_> {
        core_pr::stack_map(&self.links, &self.key).expect("a stack offer has a map")
    }

    pub(super) fn base(&self) -> String {
        self.map().stack.base.clone()
    }

    fn running(&self) -> Option<String> {
        Some(match &self.operation.as_ref()?.kind {
            StackOperationKind::Merging { .. } => tr("disabled_merging"),
            StackOperationKind::MergeUnconfirmed { checked: false, .. } => tr("waiting_state"),
            StackOperationKind::Rebasing { .. } => tr("disabled_rebasing"),
            StackOperationKind::MergeUnconfirmed { .. }
            | StackOperationKind::RebaseEnded { .. } => {
                return None;
            }
        })
    }

    /// Merge stack: the selected layer and every unmerged layer below it, together.
    pub(super) fn merge(&self, action: Option<&PullRequestActionState>) -> Avail {
        let map = self.map();
        let selected = map.selected();
        let number = self.key.number.to_string();
        if selected.layer.state != PullRequestState::Open
            || action.is_some_and(|action| !action.can_merge || action.queued)
        {
            return Avail::Hidden;
        }
        if let Some(reason) = self.running() {
            return Avail::Disabled(reason);
        }
        if !self.linked {
            return Avail::Disabled(tr_with("disabled_not_linked", &[("number", number)]));
        }
        if action.is_some_and(|action| action.merge_methods.is_empty()) {
            return Avail::Disabled(tr("no_methods"));
        }
        if selected.draft == Some(true) {
            return Avail::Disabled(tr_with("summary_selected_draft", &[("number", number)]));
        }
        match map.blockers().first() {
            Some((blocker, kind)) => Avail::Disabled(blocked(*blocker, *kind)),
            None => Avail::Enabled,
        }
    }

    /// Rebase stack: every unmerged layer, whichever one is selected.
    pub(super) fn rebase(&self, action: Option<&PullRequestActionState>) -> Avail {
        let map = self.map();
        if map.unmerged().is_empty() || action.is_some_and(|action| !action.can_merge) {
            return Avail::Hidden;
        }
        if matches!(
            self.operation.as_ref().map(|operation| &operation.kind),
            Some(StackOperationKind::Rebasing { .. })
        ) {
            return Avail::Hidden;
        }
        if let Some(reason) = self.running() {
            return Avail::Disabled(reason);
        }
        if action.is_some_and(|action| action.queued) {
            return Avail::Disabled(tr("disabled_queued"));
        }
        if !self.linked {
            return Avail::Disabled(tr_with(
                "disabled_not_linked",
                &[("number", self.key.number.to_string())],
            ));
        }
        match map
            .rows
            .iter()
            .find(|row| row.layer.state == PullRequestState::Closed)
        {
            Some(row) => Avail::Disabled(tr_with(
                "rebase_blocked_closed",
                &[("number", row.layer.number.to_string())],
            )),
            None => Avail::Enabled,
        }
    }

    /// The stack's operation, while it is moving the selected layer.
    pub(super) fn operation_here(&self) -> Option<&PullRequestStackOperation> {
        self.operation
            .as_ref()
            .filter(|operation| operation.covers(self.key.number))
    }
}

/// A `pull_requests.stack` string.
fn tr(key: &str) -> String {
    crate::translate(format!("pull_requests.stack.{key}")).into_owned()
}

fn tr_with(key: &str, args: &[(&str, String)]) -> String {
    let names: Vec<_> = args.iter().map(|(name, _)| *name).collect();
    let values: Vec<_> = args.iter().map(|(_, value)| value.clone()).collect();
    crate::translate_with_args(format!("pull_requests.stack.{key}"), &names, &values).into_owned()
}

fn blocked(blocker: u64, kind: StackBlocker) -> String {
    tr_with(
        match kind {
            StackBlocker::Draft => "blocked_signal_draft",
            StackBlocker::Closed => "blocked_signal_closed",
        },
        &[("blocker", blocker.to_string())],
    )
}

/// "#412, #413 and #414", "#412–#418" for a consecutive run, else a count.
pub(super) fn list(numbers: &[u64]) -> String {
    let n = |index: usize| numbers[index].to_string();
    match numbers.len() {
        0 => String::new(),
        1 => format!("#{}", numbers[0]),
        2 => tr_with("list_two", &[("a", n(0)), ("b", n(1))]),
        3 => tr_with("list_three", &[("a", n(0)), ("b", n(1)), ("c", n(2))]),
        count if numbers.windows(2).all(|pair| pair[1] == pair[0] + 1) => {
            tr_with("list_range", &[("first", n(0)), ("last", n(count - 1))])
        }
        count => tr_with("list_count", &[("count", count.to_string())]),
    }
}

fn short(head: &str) -> String {
    head.chars().take(7).collect()
}

fn state_word(state: PullRequestState, draft: bool) -> String {
    crate::tr!(match (state, draft) {
        (PullRequestState::Open, true) => "pull_requests.state_draft",
        (PullRequestState::Open, false) => "pull_requests.state_open",
        (PullRequestState::Merged, _) => "pull_requests.state_merged",
        (PullRequestState::Closed, _) => "pull_requests.state_closed",
    })
    .into_owned()
}

fn state_lower(state: PullRequestState, draft: bool) -> String {
    crate::tr!(match (state, draft) {
        (PullRequestState::Open, true) => "pull_requests.state_draft_lower",
        (PullRequestState::Open, false) => "pull_requests.state_open_lower",
        (PullRequestState::Merged, _) => "pull_requests.state_merged_lower",
        (PullRequestState::Closed, _) => "pull_requests.state_closed_lower",
    })
    .into_owned()
}

/// The map's one-line reading of what merging the selected layer would do.
fn summary(map: &StackMap<'_>) -> String {
    let selected = map.selected();
    let number = selected.layer.number.to_string();
    match selected.layer.state {
        PullRequestState::Merged => return tr_with("summary_merged", &[("number", number)]),
        PullRequestState::Closed => return tr_with("summary_closed", &[("number", number)]),
        PullRequestState::Open => {}
    }
    if selected.draft == Some(true) {
        return tr_with("summary_selected_draft", &[("number", number)]);
    }
    let blockers = map.blockers();
    if let Some((blocker, kind)) = blockers.first() {
        let mut text = tr_with(
            match kind {
                StackBlocker::Draft => "summary_blocked_draft",
                StackBlocker::Closed => "summary_blocked_closed",
            },
            &[("number", number), ("blocker", blocker.to_string())],
        );
        if blockers.len() > 1 {
            text.push(' ');
            text.push_str(&tr_with(
                "summary_more",
                &[("count", (blockers.len() - 1).to_string())],
            ));
        }
        return text;
    }
    let scope = map.scope();
    if scope.len() <= 1 {
        return tr_with("summary_bottom", &[("number", number)]);
    }
    tr_with(
        "summary_scope",
        &[
            ("number", number),
            ("count", scope.len().to_string()),
            ("base", map.stack.base.clone()),
        ],
    )
}

/// The label and tooltip of the Merge stack primary.
pub(super) fn merge_tooltip(offer: &StackOffer) -> String {
    let map = offer.map();
    let scope = map.scope();
    let number = offer.key.number.to_string();
    if scope.len() <= 1 {
        return tr_with(
            "merge_tooltip_one",
            &[("number", number), ("base", map.stack.base.clone())],
        );
    }
    tr_with(
        "merge_tooltip",
        &[
            ("first", scope[0].to_string()),
            ("number", number),
            ("base", map.stack.base.clone()),
            ("count", scope.len().to_string()),
        ],
    )
}

pub(super) fn merge_count_label(offer: &StackOffer) -> String {
    tr_with(
        "merge_count",
        &[("count", offer.map().scope().len().max(1).to_string())],
    )
}

pub(super) fn rebase_menu_label() -> String {
    tr("rebase_menu")
}

pub(super) fn rebase_tooltip(offer: &StackOffer) -> String {
    tr_with(
        "rebase_tooltip",
        &[
            ("stack", offer.map().stack.number.to_string()),
            ("base", offer.base()),
        ],
    )
}

pub(super) fn merge_primary_label() -> String {
    tr("merge")
}

pub(super) fn merge_primary_accessible(number: u64) -> String {
    tr_with("merge_label", &[("number", number.to_string())])
}

/// The operation's chip: the header's primary slot, the map and the linked rows' caption. A
/// merge's chip opens a popover with its scope and GitHub's operation id; a rebase's opens its
/// progress where this device may act on the stack, and is its tooltip alone elsewhere.
pub(super) fn operation_chip(
    target: Option<Target>,
    operation: &PullRequestStackOperation,
    links: &[ThreadPullRequestLink],
    compact: bool,
    cx: &App,
) -> AnyElement {
    let (icon, color, label, tooltip) = chip_words(operation, cx);
    let chip = || {
        h_flex()
            .id(SharedString::from(format!(
                "pr-stack-operation-{}",
                operation.stack
            )))
            .flex_none()
            .gap_1()
            .items_center()
            .px_2()
            .py_0p5()
            .rounded(material::radius_chip(cx))
            .bg(color.opacity(0.1))
            .text_secondary(compact, cx)
            .text_color(color)
            .child(match operation.kind {
                StackOperationKind::Rebasing { .. } => Spinner::new().into_any_element(),
                _ => Icon::new(icon).into_any_element(),
            })
            .child(label.clone())
    };
    let (StackOperationKind::Merging {
        id,
        target: merged,
        layers,
        ..
    }
    | StackOperationKind::MergeUnconfirmed {
        id,
        target: merged,
        layers,
        ..
    }) = &operation.kind
    else {
        return match target {
            Some(target) => Button::new(SharedString::from(format!(
                "pr-stack-operation-progress-{}",
                operation.stack
            )))
            .text()
            .rounded(material::radius_chip(cx))
            .aria_label(label.clone())
            .tooltip(tooltip.clone())
            .on_click(move |_, window, cx| open_progress(target.clone(), window, cx))
            .child(chip())
            .into_any_element(),
            None => chip()
                .tooltip(move |window, cx| Tooltip::new(tooltip.clone()).build(window, cx))
                .into_any_element(),
        };
    };
    let id = id.clone();
    let url = core_pr::stack_map(
        links,
        &PullRequestKey::new(&operation.host, &operation.repository, *merged),
    )
    .map(|map| map.stack.url.clone());
    let titles: Vec<(u64, String)> = layers
        .iter()
        .map(|&number| {
            let title = links
                .iter()
                .find(|link| {
                    link.key.host == operation.host
                        && link.key.repository == operation.repository
                        && link.key.number == number
                })
                .and_then(|link| link.snapshot.as_ref())
                .map(|snapshot| snapshot.title.clone())
                .unwrap_or_default();
            (number, title)
        })
        .collect();
    let popover = Popover::new(SharedString::from(format!(
        "pr-stack-operation-popover-{}",
        operation.stack
    )))
    .trigger(
        Button::new(SharedString::from(format!(
            "pr-stack-operation-trigger-{}",
            operation.stack
        )))
        .text()
        .rounded(material::radius_chip(cx))
        .aria_label(format!(
            "{label}, {}",
            tr_with("operation_id", &[("id", id.clone())])
        ))
        .tooltip(tooltip.clone())
        .child(chip()),
    );
    let popover = if compact {
        popover.bottom_sheet(tr("popover_title"))
    } else {
        popover
    };
    popover
        .content(move |_, _, cx| {
            let muted = cx.theme().muted_foreground;
            let mono = cx.theme().mono_font_family.clone();
            v_flex()
                // Popover sizes to its content; the titles truncate, so the width is fixed here.
                .when(!compact, |content| content.w(px(340.)))
                .p_3()
                .gap_2()
                .text_token(&cx.theme().tokens.typography.xs)
                .child(div().child(tooltip.clone()))
                .child(
                    v_flex()
                        .gap_0p5()
                        .children(titles.iter().map(|(number, title)| {
                            h_flex()
                                .gap_2()
                                .child(div().font_family(mono.clone()).child(format!("#{number}")))
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w_0()
                                        .overflow_hidden()
                                        .child(crate::widgets::fade_text(title.clone())),
                                )
                        })),
                )
                .child(
                    h_flex()
                        .gap_1()
                        .items_center()
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .overflow_hidden()
                                .font_family(mono.clone())
                                .text_color(muted)
                                .child(crate::widgets::fade_text(tr_with(
                                    "operation_id",
                                    &[("id", id.clone())],
                                ))),
                        )
                        .child(
                            Button::new("pr-stack-copy-id")
                                .ghost()
                                .xsmall()
                                .compact()
                                .icon(IconName::Copy)
                                .tooltip(tr("copy_id"))
                                .on_click({
                                    let id = id.clone();
                                    move |_, _, cx| {
                                        cx.write_to_clipboard(ClipboardItem::new_string(id.clone()))
                                    }
                                }),
                        ),
                )
                .children(url.clone().map(|url| {
                    Button::new("pr-stack-operation-open")
                        .ghost()
                        .xsmall()
                        .icon(IconName::ExternalLink)
                        .label(crate::tr!(
                            "pull_requests.open_on_host",
                            host_name = tcode_core::pull_request::GITHUB.name
                        ))
                        .on_click(move |_, _, cx| cx.open_url(&url))
                }))
        })
        .into_any_element()
}

/// The chip's glyph, colour, label and long form.
pub(super) fn chip_words(
    operation: &PullRequestStackOperation,
    cx: &App,
) -> (IconName, Hsla, String, String) {
    let ago = super::detail::ago(operation.started_at);
    match &operation.kind {
        StackOperationKind::Merging {
            layers, adopted, ..
        } => {
            let mut tooltip = tr_with(
                "chip_merging_tooltip",
                &[("list", list(layers)), ("ago", ago)],
            );
            if *adopted {
                tooltip.push(' ');
                tooltip.push_str(&tr("chip_adopted_tooltip"));
            }
            (
                IconName::Hourglass,
                cx.theme().warning,
                tr("chip_merging"),
                tooltip,
            )
        }
        StackOperationKind::MergeUnconfirmed { .. } => (
            IconName::CircleQuestionMark,
            cx.theme().warning,
            tr("chip_unconfirmed"),
            tr("chip_unconfirmed_tooltip"),
        ),
        StackOperationKind::Rebasing { layers } => {
            let done = layers
                .iter()
                .filter(|layer| {
                    matches!(
                        layer.step,
                        StackRebaseStep::Pushed { .. } | StackRebaseStep::AlreadyCurrent
                    )
                })
                .count();
            let label = tr_with(
                "chip_rebasing",
                &[
                    ("done", done.to_string()),
                    ("count", layers.len().to_string()),
                ],
            );
            (IconName::RefreshCw, cx.theme().info, label.clone(), label)
        }
        StackOperationKind::RebaseEnded { layers } => {
            let (icon, color, label) = match layers
                .iter()
                .find(|layer| matches!(layer.step, StackRebaseStep::Failed { .. }))
            {
                Some(failed) => (
                    IconName::CircleX,
                    cx.theme().danger,
                    tr_with(
                        "result_rebase_stopped",
                        &[("number", failed.number.to_string())],
                    ),
                ),
                None => (
                    IconName::CircleCheck,
                    cx.theme().success,
                    tr_with("result_rebased", &[("stack", operation.stack.to_string())]),
                ),
            };
            (icon, color, label.clone(), label)
        }
    }
}

/// The page's stack map: the layer selector's trigger, and the popover (a sheet on a phone)
/// with every layer, the merge scope and its blockers, the base, the note on GitHub stacks and
/// the stack's writes.
pub(super) fn map_selector(
    view: Entity<super::detail::PullRequestView>,
    offer: StackOffer,
    target: Option<Target>,
    action: Option<PullRequestActionState>,
    compact: bool,
    cx: &App,
) -> AnyElement {
    let map = offer.map();
    let index = map.selected + 1;
    let count = map.rows.len();
    let stack_number = map.stack.number;
    let muted = cx.theme().muted_foreground;
    let mut accessible = tr_with(
        "trigger_label",
        &[
            ("stack", stack_number.to_string()),
            ("index", index.to_string()),
            ("count", count.to_string()),
        ],
    );
    let status = offer
        .operation
        .as_ref()
        .and_then(|operation| match operation.kind {
            StackOperationKind::Merging { .. } => {
                Some((IconName::Hourglass, cx.theme().warning, "trigger_merging"))
            }
            StackOperationKind::MergeUnconfirmed { .. } => Some((
                IconName::CircleQuestionMark,
                cx.theme().warning,
                "trigger_unconfirmed",
            )),
            StackOperationKind::Rebasing { .. } => {
                Some((IconName::RefreshCw, cx.theme().info, "trigger_rebasing"))
            }
            StackOperationKind::RebaseEnded { .. } => None,
        });
    if let Some((_, _, suffix)) = status {
        accessible.push_str(&tr(suffix));
    }
    let trigger = Button::new("pr-layer-select")
        .ghost()
        .outline()
        .compact()
        .aria_label(accessible.clone())
        .tooltip(accessible)
        .child(
            h_flex()
                .gap_1p5()
                .items_center()
                .text_body(compact, cx)
                .child(Icon::new(IconName::Layers))
                .child(
                    crate::tr!(
                        "pull_requests.layer_position",
                        index = index.to_string(),
                        count = count.to_string()
                    )
                    .into_owned(),
                )
                .children(status.map(|(icon, color, _)| Icon::new(icon).text_color(color)))
                .child(Icon::new(IconName::ChevronDown).text_color(muted)),
        );
    let popover = Popover::new("pr-layer-popover").trigger(trigger);
    let popover = if compact {
        popover.bottom_sheet(crate::tr!("pull_requests.detail.stack_title").into_owned())
    } else {
        popover
    };
    popover
        .content(move |_, _, cx| {
            let popover = cx.entity();
            map_content(
                &view,
                &offer,
                target.as_ref(),
                action.as_ref(),
                compact,
                popover,
                cx,
            )
        })
        .into_any_element()
}

fn map_content(
    view: &Entity<super::detail::PullRequestView>,
    offer: &StackOffer,
    target: Option<&Target>,
    action: Option<&PullRequestActionState>,
    compact: bool,
    popover: Entity<gpui_base::PopoverState>,
    cx: &mut Context<gpui_base::PopoverState>,
) -> AnyElement {
    let map = offer.map();
    let theme = cx.theme().clone();
    let muted = theme.muted_foreground;
    let mono = theme.mono_font_family.clone();
    let selected_number = offer.key.number;
    let store = view.read(cx).store().clone();
    let collapsed = store.read(cx).stack_note_collapsed();
    let head = h_flex()
        .px_2()
        .py_1()
        .gap_1()
        .items_center()
        .text_secondary(compact, cx)
        .child(Icon::new(IconName::Layers))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .overflow_hidden()
                .child(crate::widgets::fade_text(tr_with(
                    "head",
                    &[
                        ("stack", map.stack.number.to_string()),
                        ("count", map.rows.len().to_string()),
                        ("base", map.stack.base.clone()),
                    ],
                ))),
        )
        .child(
            Button::new("pr-stack-about")
                .ghost()
                .xsmall()
                .compact()
                .icon(IconName::CircleQuestionMark)
                .aria_label(tr("about"))
                .tooltip(tr("about"))
                .on_click({
                    let store = store.clone();
                    move |_, _, cx| {
                        store.update(cx, |store, cx| {
                            let collapsed = store.stack_note_collapsed();
                            store.set_stack_note_collapsed(!collapsed, cx)
                        })
                    }
                }),
        );
    let note = (!collapsed).then(|| {
        v_flex()
            .px_2()
            .pb_1()
            .gap_1()
            .items_start()
            .text_secondary(compact, cx)
            .child(tr("note"))
            .child(
                Button::new("pr-stack-docs")
                    .ghost()
                    .xsmall()
                    .icon(IconName::ExternalLink)
                    .label(tr("docs_link"))
                    .on_click(|_, _, cx| cx.open_url(core_pr::STACKS_DOCS_URL)),
            )
    });
    let operation = offer.operation.as_ref().map(|operation| {
        h_flex().px_2().py_1().child(operation_chip(
            target.cloned(),
            operation,
            &offer.links,
            compact,
            cx,
        ))
    });
    let rows = map.rows.iter().rev().map(|row| {
        let number = row.layer.number;
        let layer_key = PullRequestKey::new(&offer.key.host, &offer.key.repository, number);
        let linked = row.condition == StackLayerCondition::Linked;
        let (glyph, color, _) = appearance(
            row_state(
                row.link.and_then(|link| link.snapshot.as_ref()),
                Some(row.layer.state),
            ),
            cx,
        );
        let title = row
            .link
            .and_then(|link| link.snapshot.as_ref())
            .map(|snapshot| snapshot.title.clone());
        let state = state_lower(row.layer.state, row.draft == Some(true));
        let (trailing, role_label): (AnyElement, String) = match row.role {
            StackLayerRole::Selected => (
                Icon::new(IconName::Check).into_any_element(),
                tr("role_label_selected"),
            ),
            StackLayerRole::Above => (
                div()
                    .text_color(muted)
                    .child(state_word(row.layer.state, row.draft == Some(true)))
                    .into_any_element(),
                tr("role_label_above"),
            ),
            StackLayerRole::InScope => (
                div()
                    .text_color(muted)
                    .child(tr_with(
                        "role_in_scope",
                        &[("number", selected_number.to_string())],
                    ))
                    .into_any_element(),
                tr_with(
                    "role_label_in_scope",
                    &[("number", selected_number.to_string())],
                ),
            ),
            StackLayerRole::Blocks(kind) => {
                let (color, label) = match kind {
                    StackBlocker::Draft => (theme.warning, tr("blocks_draft")),
                    StackBlocker::Closed => (theme.danger, tr("blocks_closed")),
                };
                (
                    h_flex()
                        .gap_1()
                        .items_center()
                        .text_color(color)
                        .child(Icon::new(IconName::Lock))
                        .child(label)
                        .into_any_element(),
                    tr_with(
                        "role_label_blocks",
                        &[("number", selected_number.to_string())],
                    ),
                )
            }
            StackLayerRole::BelowMerged => (
                div()
                    .text_color(muted)
                    .child(tr("role_merged"))
                    .into_any_element(),
                tr("role_label_merged"),
            ),
        };
        let condition = match row.condition {
            StackLayerCondition::Dismissed => Some(crate::tr!("pull_requests.source_dismissed")),
            StackLayerCondition::NotLinked => {
                Some(crate::tr!("pull_requests.detail.layer_not_linked"))
            }
            StackLayerCondition::Linked => None,
        };
        let name = title
            .clone()
            .unwrap_or_else(|| row.layer.head_branch.clone());
        let mut label = tr_with(
            "row_label",
            &[
                ("number", number.to_string()),
                ("title", name.clone()),
                ("state", state),
                ("role", role_label),
            ],
        );
        if let Some(condition) = &condition {
            label.push_str(", ");
            label.push_str(&condition.to_lowercase());
        }
        let selected = row.role == StackLayerRole::Selected;
        let rail = matches!(row.role, StackLayerRole::Selected | StackLayerRole::InScope);
        let view = view.clone();
        let popover = popover.clone();
        material::selectable_row(
            material::accessible_clickable(
                h_flex(),
                ("pr-layer", number as usize),
                gpui::Role::MenuItem,
                label,
                cx,
            ),
            selected,
            compact,
            cx,
        )
        .aria_selected(selected)
        .w_full()
        .text_secondary(compact, cx)
        .text_color(theme.foreground)
        .cursor_pointer()
        // The merge scope reads as one bar down the rows that land together; 2pt is the bar,
        // not a spacing step.
        .child(
            div()
                .w(px(2.))
                .h_full()
                .when(rail, |bar| bar.bg(theme.primary.opacity(0.6))),
        )
        .child(Icon::new(glyph).text_color(if linked { color } else { muted }))
        .child(div().text_code(cx).child(format!("#{number}")))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .overflow_hidden()
                .text_body(compact, cx)
                .when(title.is_none(), |text| {
                    text.font_family(mono.clone()).text_color(muted)
                })
                .child(crate::widgets::fade_text(name)),
        )
        .child(trailing)
        .children(condition.map(|condition| div().text_color(muted).child(condition.into_owned())))
        .on_click(move |_, window, cx| {
            view.update(cx, |view, cx| view.select_layer(layer_key.clone(), cx));
            popover.update(cx, |state, cx| state.dismiss(window, cx));
        })
    });
    let base = h_flex()
        .px_2()
        .py_1()
        .gap_1()
        .items_center()
        .text_meta_mono(compact, cx)
        .child(Icon::new(IconName::CornerDownRight))
        .child(map.stack.base.clone());
    let summary = v_flex()
        .px_2()
        .py_1()
        .gap_0p5()
        .child(div().text_body(compact, cx).child(summary(&map)))
        .child(
            div()
                .text_secondary(compact, cx)
                .child(tr("summary_source")),
        );
    let read_only = target.is_none();
    let action_button = |id: &'static str,
                         icon: IconName,
                         label: String,
                         avail: Avail,
                         kind: Lifecycle,
                         tooltip: Option<String>| {
        if read_only || avail == Avail::Hidden {
            return None;
        }
        let target = target.cloned();
        let popover = popover.clone();
        let disabled = matches!(avail, Avail::Disabled(_));
        let tooltip = match avail {
            Avail::Disabled(reason) => Some(reason),
            _ => tooltip,
        };
        let button = Button::new(id)
            .outline()
            .icon(icon)
            .label(label)
            .disabled(disabled)
            .when_some(tooltip, |button, tooltip| button.tooltip(tooltip))
            .on_click(move |_, window, cx| {
                popover.update(cx, |state, cx| state.dismiss(window, cx));
                if let Some(target) = target.clone() {
                    target.run(kind, window, cx);
                }
            });
        Some(if compact {
            button.w_full().touch(true).into_any_element()
        } else {
            button.xsmall().into_any_element()
        })
    };
    let actions: Vec<AnyElement> = [
        action_button(
            "pr-stack-merge",
            IconName::GitMerge,
            merge_count_label(offer),
            offer.merge(action),
            Lifecycle::MergeStack,
            Some(merge_tooltip(offer)),
        ),
        action_button(
            "pr-stack-rebase",
            IconName::RefreshCw,
            rebase_menu_label(),
            offer.rebase(action),
            Lifecycle::RebaseStack,
            Some(rebase_tooltip(offer)),
        ),
    ]
    .into_iter()
    .flatten()
    .collect();
    let actions = (!actions.is_empty()).then(|| {
        let row = if compact { v_flex() } else { h_flex() };
        row.gap_2()
            .px_2()
            .pt_2()
            .pb_1()
            .border_t_1()
            .border_color(theme.border)
            .children(actions)
    });
    v_flex()
        .id("pr-layer-list")
        .role(gpui::Role::Menu)
        .when(compact, |list| list.w_full())
        // Popover sizes to its content; the rows truncate, so the menu's extent is fixed here.
        .when(!compact, |list| list.w(px(380.)).max_h(px(480.)))
        .gap_0p5()
        .overflow_y_scroll()
        .child(head)
        .children(note)
        .children(operation)
        .children(rows)
        .child(base)
        .child(summary)
        .children(actions)
        .into_any_element()
}

/// The linked rows' stack caption: what the stack is, the operation running on it, and the
/// short note with GitHub's docs.
pub(super) fn caption(
    stack: &core_pr::PullRequestStack,
    operation: Option<&PullRequestStackOperation>,
    links: &[ThreadPullRequestLink],
    compact: bool,
    cx: &App,
) -> AnyElement {
    v_flex()
        .px_3()
        .py_1()
        .gap_0p5()
        .text_secondary(compact, cx)
        .child(
            h_flex()
                .text_caption(cx)
                .id(SharedString::from(format!("pr-stack-{}", stack.id)))
                .gap_2()
                .items_center()
                .tooltip(|window, cx| {
                    Tooltip::new(crate::tr!("pull_requests.stack_tooltip").into_owned())
                        .build(window, cx)
                })
                .child(Icon::new(IconName::Layers))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .overflow_hidden()
                        .child(crate::widgets::fade_text(
                            crate::tr!(
                                "pull_requests.stack_caption",
                                count = stack.layers.len().to_string(),
                                base = stack.base.clone()
                            )
                            .into_owned(),
                        )),
                )
                .children(
                    operation.map(|operation| operation_chip(None, operation, links, compact, cx)),
                ),
        )
        .child(
            h_flex()
                .flex_wrap()
                .gap_1()
                .items_center()
                .child(tr("note_short"))
                .child(
                    Button::new(SharedString::from(format!("pr-stack-docs-{}", stack.id)))
                        .ghost()
                        .xsmall()
                        .compact()
                        .icon(IconName::ExternalLink)
                        .label(tr("about"))
                        .on_click(|_, _, cx| cx.open_url(core_pr::STACKS_DOCS_URL)),
                ),
        )
        .into_any_element()
}

/// A linked layer's blocked signal: a draft or closed layer below it that it would merge with.
pub(super) fn row_blocker(links: &[ThreadPullRequestLink], key: &PullRequestKey) -> Option<String> {
    let map = core_pr::stack_map(links, key)?;
    let selected = map.selected();
    if selected.condition != StackLayerCondition::Linked
        || selected.layer.state != PullRequestState::Open
        || selected.draft == Some(true)
    {
        return None;
    }
    let (blocker, kind) = *map.blockers().first()?;
    Some(blocked(blocker, kind))
}

/// One toast slot per stack: a newer answer replaces the older one on every layer's page.
struct StackWrite;

fn slot(stack: &StackId) -> SharedString {
    SharedString::from(format!("{}/{}/{}", stack.0, stack.1, stack.2))
}

/// Where a stack's answer was asked from and what it covered, for its words.
pub(super) struct Answer<'a> {
    pub(super) stack: StackId,
    pub(super) target: u64,
    pub(super) layers: &'a [u64],
    pub(super) base: &'a str,
    /// Each layer's head branch, for the words that name one.
    pub(super) branches: Vec<(u64, String)>,
    pub(super) url: Option<String>,
    pub(super) rebase: bool,
    pub(super) late: bool,
}

/// The words of a stack write's answer. A success goes by itself; anything else stays until
/// it is read.
pub(super) fn answer_toast(
    answer: &Answer<'_>,
    result: &PullRequestActionResult,
) -> Option<Notification> {
    let number = answer.target.to_string();
    let count = answer.layers.len().max(1);
    let open = |note: Notification| match answer.url.clone() {
        Some(url) => note.action(move |_, _, _| {
            let url = url.clone();
            Button::new("pr-stack-result-open")
                .ghost()
                .xsmall()
                .label(crate::tr!(
                    "pull_requests.open_on_host",
                    host_name = tcode_core::pull_request::GITHUB.name
                ))
                .on_click(move |_, _, cx| cx.open_url(&url))
        }),
        None => note,
    };
    let not = |title: String, body: String| Notification::error(body).title(title);
    let note = match result {
        PullRequestActionResult::Applied if answer.late => Notification::success(tr_with(
            "result_finished_late_body",
            &[
                ("list", list(answer.layers)),
                ("base", answer.base.to_owned()),
            ],
        ))
        .title(tr("result_finished_late")),
        PullRequestActionResult::Applied => Notification::success(tr_with(
            "result_merged_body",
            &[
                ("list", list(answer.layers)),
                ("base", answer.base.to_owned()),
            ],
        ))
        .title(if count == 1 {
            tr_with("result_merged_one", &[("number", number)])
        } else {
            tr_with(
                "result_merged",
                &[("count", count.to_string()), ("number", number)],
            )
        }),
        PullRequestActionResult::Queued { .. } => Notification::info(tr("result_queued_body"))
            .title(tr_with("result_queued", &[("number", number)])),
        PullRequestActionResult::Pending { adopted: false, .. } => Notification::info(tr_with(
            "result_submitted_body",
            &[("list", list(answer.layers))],
        ))
        .title(tr("result_submitted")),
        PullRequestActionResult::Pending { adopted: true, .. } => {
            Notification::info(tr("result_adopted_body")).title(tr("result_adopted"))
        }
        PullRequestActionResult::MergeUnconfirmed { .. } => {
            open(Notification::warning(tr("result_deadline_body")).title(tr("result_deadline")))
        }
        PullRequestActionResult::RebaseStarted => return None,
        PullRequestActionResult::Rebased { pushed, current } => Notification::success(tr_with(
            "result_rebased_body",
            &[
                ("pushed", pushed.len().to_string()),
                ("current", current.len().to_string()),
            ],
        ))
        .title(tr_with(
            "result_rebased",
            &[("stack", answer.stack.2.to_string())],
        )),
        PullRequestActionResult::RebaseStopped {
            pushed,
            failed,
            reason,
            untouched,
        } => {
            let stayed = if pushed.is_empty() {
                String::new()
            } else {
                tr_with("result_pushed_stay", &[("list", list(pushed))])
            };
            let failed_number = failed.to_string();
            match reason {
                StackRebaseFailure::PushUnconfirmed => open(
                    Notification::warning(tr_with(
                        "result_push_uncertain_body",
                        &[
                            ("pushed", stayed.clone()),
                            ("branch", branch_of(answer, *failed)),
                        ],
                    ))
                    .title(tr_with(
                        "result_push_uncertain",
                        &[("number", failed_number)],
                    )),
                ),
                _ => {
                    let body = match reason {
                        StackRebaseFailure::Conflict => tr_with(
                            "result_rebase_conflict_body",
                            &[
                                ("pushed", stayed.clone()),
                                ("number", failed_number.clone()),
                                (
                                    "untouched",
                                    if untouched.is_empty() {
                                        String::new()
                                    } else {
                                        tr_with("result_untouched", &[("list", list(untouched))])
                                    },
                                ),
                            ],
                        ),
                        StackRebaseFailure::LeaseRefused => tr_with(
                            "result_rebase_lease_body",
                            &[
                                ("branch", branch_of(answer, *failed)),
                                ("pushed", stayed.clone()),
                            ],
                        ),
                        StackRebaseFailure::Git { step, message } => tr_with(
                            "result_rebase_git_body",
                            &[
                                ("step", git_step(*step)),
                                ("message", message.clone()),
                                ("pushed", stayed.clone()),
                            ],
                        ),
                        StackRebaseFailure::PushUnconfirmed => unreachable!(),
                    };
                    open(Notification::error(body.trim().to_owned()).title(tr_with(
                        "result_rebase_stopped",
                        &[("number", failed_number)],
                    )))
                }
            }
        }
        // A rebase that did not start names why and that nothing moved.
        PullRequestActionResult::Rejected(rejection)
            if answer.rebase && *rejection != PullRequestRejection::OperationRunning =>
        {
            not(
                tr_with(
                    "result_not_rebased",
                    &[("reason", rejection_reason(rejection, GITHUB.name))],
                ),
                tr("nothing_changed"),
            )
        }
        PullRequestActionResult::Rejected(rejection) => {
            let titled = not;
            match rejection {
                PullRequestRejection::LayerChanged {
                    number,
                    expected,
                    actual,
                } => titled(
                    tr_with("result_layer_changed", &[("number", number.to_string())]),
                    tr_with(
                        "result_layer_changed_body",
                        &[("expected", short(expected)), ("actual", short(actual))],
                    ),
                ),
                PullRequestRejection::StackChanged => {
                    titled(tr("result_stack_changed"), tr("result_stack_changed_body"))
                }
                PullRequestRejection::LayerNotOpen { number, state } => titled(
                    tr_with(
                        "result_not_open",
                        &[
                            ("number", number.to_string()),
                            ("state", state_lower(*state, false)),
                        ],
                    ),
                    tr("result_not_open_body"),
                ),
                PullRequestRejection::LayerDraft { number } => titled(
                    tr_with(
                        "result_not_open",
                        &[
                            ("number", number.to_string()),
                            ("state", state_lower(PullRequestState::Open, true)),
                        ],
                    ),
                    tr("result_not_open_body"),
                ),
                PullRequestRejection::OperationRunning => {
                    Notification::info(tr("waiting_state")).title(tr("result_running"))
                }
                PullRequestRejection::MergeRunning => {
                    open(not(tr("result_409_no_id"), tr("result_409_no_id_body")))
                }
                PullRequestRejection::NoPushAccess { numbers } => not(
                    tr_with(
                        "result_not_rebased",
                        &[(
                            "reason",
                            tr_with("result_no_push", &[("list", list(numbers))]),
                        )],
                    ),
                    tr("nothing_changed"),
                ),
                PullRequestRejection::NoGitIdentity => not(
                    tr_with(
                        "result_not_rebased",
                        &[("reason", tr("result_no_identity"))],
                    ),
                    tr("nothing_changed"),
                ),
                PullRequestRejection::NotLinked => {
                    titled(tr("result_refused"), tr("result_not_linked"))
                }
                PullRequestRejection::Refused { messages } if !answer.rebase => {
                    let body = if messages.is_empty() {
                        tr("result_refused_body")
                    } else {
                        let joined = messages.join(" ");
                        if joined.chars().count() > 320 {
                            format!("{}…", joined.chars().take(320).collect::<String>())
                        } else {
                            joined
                        }
                    };
                    not(tr("result_refused"), body)
                }
                rejection => titled(
                    tr("result_refused"),
                    rejection_reason(rejection, GITHUB.name),
                ),
            }
        }
        PullRequestActionResult::Uncertain => open(
            Notification::warning(
                crate::tr!(
                    "pull_requests.result.uncertain_body",
                    message = crate::tr!("pull_requests.result.connection_lost").into_owned(),
                    host_name = GITHUB.name
                )
                .into_owned(),
            )
            .title(tr_with("result_uncertain", &[("number", number)])),
        ),
        _ => return None,
    };
    let settled = matches!(
        result,
        PullRequestActionResult::Applied
            | PullRequestActionResult::Queued { .. }
            | PullRequestActionResult::Rebased { .. }
    );
    Some(
        note.autohide(settled)
            .id1::<StackWrite>(slot(&answer.stack)),
    )
}

fn branch_of(answer: &Answer<'_>, number: u64) -> String {
    answer
        .branches
        .iter()
        .find(|(layer, _)| *layer == number)
        .map(|(_, branch)| branch.clone())
        .unwrap_or_else(|| format!("#{number}"))
}

fn branches(links: &[ThreadPullRequestLink], key: &PullRequestKey) -> Vec<(u64, String)> {
    core_pr::native_stack(links, key)
        .map(|stack| {
            stack
                .layers
                .iter()
                .map(|layer| (layer.number, layer.head_branch.clone()))
                .collect()
        })
        .unwrap_or_default()
}

fn git_step(step: StackRebaseGitStep) -> String {
    tr(match step {
        StackRebaseGitStep::Preparing => "git_preparing",
        StackRebaseGitStep::Fetching => "git_fetching",
        StackRebaseGitStep::ForkPoint => "git_fork_point",
        StackRebaseGitStep::CheckingOut => "git_checking_out",
        StackRebaseGitStep::Rebasing => "git_rebasing",
        StackRebaseGitStep::Pushing => "git_pushing",
    })
}

/// The host's report of how a stack write ended, for this client's toast and its rebase view.
#[allow(clippy::too_many_arguments)]
pub fn present_result(
    store: &Entity<WorkspaceStore>,
    session: &str,
    target: &PullRequestKey,
    stack: u64,
    layers: &[u64],
    base: &str,
    result: &PullRequestActionResult,
    late: bool,
    window: &mut Window,
    cx: &mut App,
) {
    let id = (target.host.clone(), target.repository.clone(), stack);
    let rebase = matches!(
        result,
        PullRequestActionResult::Rebased { .. } | PullRequestActionResult::RebaseStopped { .. }
    );
    let answer = Answer {
        stack: id,
        target: target.number,
        layers,
        base,
        branches: branches(store.read(cx).pull_requests(session), target),
        url: Some(format!(
            "https://{}/{}/pull/{}",
            target.host, target.repository, target.number
        )),
        rebase,
        late,
    };
    if let Some(note) = answer_toast(&answer, result) {
        window.push_notification(note, cx);
    }
}

fn send(
    target: &Target,
    action: tcode_protocol::PullRequestAction,
    cx: &mut App,
) -> gpui::Task<PullRequestActionResult> {
    (target.started)(cx);
    let task = target.store.update(cx, |store, cx| {
        store.command(
            Command::RunPullRequestAction {
                session_id: target.session.clone(),
                key: target.offer.key.clone(),
                action,
            },
            cx,
        )
    });
    cx.spawn(async move |_| match answer_of(task.await) {
        Ok(result) => result,
        Err(error) => PullRequestActionResult::Rejected(PullRequestRejection::Refused {
            messages: vec![super::detail::reason(&error)],
        }),
    })
}

/// A rejection that is about the stack the dialog shows: the dialog stays with it.
fn stays(result: &PullRequestActionResult) -> bool {
    matches!(
        result,
        PullRequestActionResult::Rejected(
            PullRequestRejection::StackChanged
                | PullRequestRejection::LayerChanged { .. }
                | PullRequestRejection::LayerNotOpen { .. }
                | PullRequestRejection::LayerDraft { .. }
                | PullRequestRejection::NoPushAccess { .. }
                | PullRequestRejection::NoGitIdentity
        )
    )
}

fn notice_text(result: &PullRequestActionResult) -> Option<String> {
    let PullRequestActionResult::Rejected(rejection) = result else {
        return None;
    };
    Some(match rejection {
        PullRequestRejection::LayerChanged { number, actual, .. } => tr_with(
            "changed_head",
            &[("number", number.to_string()), ("new", short(actual))],
        ),
        PullRequestRejection::StackChanged => tr("changed_layers"),
        PullRequestRejection::LayerNotOpen { number, .. }
        | PullRequestRejection::LayerDraft { number } => {
            tr_with("changed_state", &[("number", number.to_string())])
        }
        PullRequestRejection::NoPushAccess { numbers } => {
            tr_with("rebase_blocked_access", &[("layers", list(numbers))])
        }
        PullRequestRejection::NoGitIdentity => tr_with(
            "rebase_blocked_identity",
            &[("machine", machine_label(None))],
        ),
        _ => return None,
    })
}

fn machine_label(store: Option<&WorkspaceStore>) -> String {
    store
        .and_then(|store| store.remote_host_name().map(str::to_owned))
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| tr("this_machine"))
}

fn fact(label: String, value: impl IntoElement, cx: &App) -> AnyElement {
    h_flex()
        .gap_2()
        .items_start()
        .child(
            // One label column so the facts' values line up; no token is a column width.
            div().w(px(104.)).flex_none().text_caption(cx).child(label),
        )
        .child(div().flex_1().min_w_0().text_body(false, cx).child(value))
        .into_any_element()
}

/// One note line; the list holding it sets the text role the glyph takes its size from.
fn line(icon: IconName, color: Hsla, text: String) -> AnyElement {
    h_flex()
        .gap_1p5()
        .items_start()
        .child(div().pt_0p5().child(Icon::new(icon).text_color(color)))
        .child(div().flex_1().min_w_0().child(text))
        .into_any_element()
}

/// The text role of a confirmation's notes.
fn notes(cx: &App) -> gpui::Div {
    v_flex()
        .gap_1()
        .text_token(&cx.theme().tokens.typography.xs)
}

fn warning_wash(cx: &App) -> gpui::Div {
    material::notice(cx.theme().warning, cx)
        .flex_col()
        .items_start()
        .text_token(&cx.theme().tokens.typography.xs)
}

/// One fresh layer of a confirmation, with its tag.
fn layer_row(
    index: Option<usize>,
    layer: &tcode_protocol::PullRequestStackLayerState,
    tag: AnyElement,
    branch: bool,
    muted_row: bool,
    cx: &App,
) -> AnyElement {
    let theme = cx.theme();
    let (glyph, color, _) = appearance(
        match (layer.state, layer.draft) {
            (PullRequestState::Open, true) => core_pr::PullRequestBadgeState::Draft,
            (PullRequestState::Open, false) => core_pr::PullRequestBadgeState::Open,
            (PullRequestState::Merged, _) => core_pr::PullRequestBadgeState::Merged,
            (PullRequestState::Closed, _) => core_pr::PullRequestBadgeState::Closed,
        },
        cx,
    );
    let head = layer.head.clone();
    material::group_row(cx)
        .id(SharedString::from(format!(
            "pr-stack-layer-{}",
            layer.number
        )))
        .when(muted_row, |row| row.text_color(theme.muted_foreground))
        .children(index.map(|index| step_number(index, cx)))
        .child(Icon::new(glyph).text_color(if muted_row {
            theme.muted_foreground
        } else {
            color
        }))
        .child(div().text_code(cx).child(format!("#{}", layer.number)))
        .child(
            v_flex()
                .flex_1()
                .min_w_0()
                .child(
                    div()
                        .overflow_hidden()
                        .text_body(false, cx)
                        .child(crate::widgets::fade_text(layer.title.clone())),
                )
                .when(branch, |text| {
                    text.child(
                        div()
                            .overflow_hidden()
                            .text_meta_mono(false, cx)
                            .child(crate::widgets::fade_text(layer.head_branch.clone())),
                    )
                }),
        )
        .when(index.is_some(), |row| {
            row.child(
                div()
                    .id(SharedString::from(format!(
                        "pr-stack-head-{}",
                        layer.number
                    )))
                    .text_meta_mono(false, cx)
                    .child(head.as_deref().map(short).unwrap_or_else(|| "—".into()))
                    .when_some(head, |cell, head| {
                        cell.tooltip(move |window, cx| Tooltip::new(head.clone()).build(window, cx))
                    }),
            )
        })
        .child(div().flex_none().text_secondary(false, cx).child(tag))
        .into_any_element()
}

/// A scope row's position, in a column fixed so the numbers line up down the rows; no token
/// is a column width.
fn step_number(index: usize, cx: &App) -> gpui::Div {
    div()
        .w(px(14.))
        .flex_none()
        .text_secondary(false, cx)
        .child((index + 1).to_string())
}

/// The layers a confirmation covers, in one grouped list that scrolls past six rows or so.
fn scope_box(rows: Vec<AnyElement>, cx: &App) -> AnyElement {
    material::grouped(rows, cx)
        .id("pr-stack-scope")
        .max_h(px(240.))
        .overflow_y_scroll()
        .into_any_element()
}

/// The Merge stack confirmation: the scope GitHub holds now, each layer at the head that will be
/// sent, what stays above, and what the merge would meet.
struct MergeStackDialog {
    target: Target,
    state: Option<Result<PullRequestStackActionState, String>>,
    method: Option<PullRequestMergeMethod>,
    project: Option<(String, String)>,
    project_default: Option<PullRequestMergeMethod>,
    make_default: bool,
    sending: bool,
    /// A host answer about the stack this dialog showed; confirm waits for a new read.
    notice: Option<String>,
    _observe: gpui::Subscription,
}

impl MergeStackDialog {
    fn load(&mut self, cx: &mut Context<Self>) {
        self.state = None;
        self.notice = None;
        let task = self.target.store.update(cx, |store, cx| {
            store.read_pull_request(
                self.target.session.clone(),
                self.target.offer.key.clone(),
                PullRequestRead::StackState { rebase: false },
                cx,
            )
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                this.state = Some(match result {
                    Ok((PullRequestReadResponse::StackState(state), _)) => {
                        this.method = super::lifecycle::merge_method(
                            &state.merge_methods,
                            this.target.offer.method,
                            this.project_default,
                        );
                        Ok(state)
                    }
                    Ok(_) => Err(String::new()),
                    Err(error) => Err(super::detail::reason(&error)),
                });
                cx.notify();
            });
        })
        .detach();
    }

    /// The fresh scope: the unmerged layers from the bottom through the selected one.
    fn scope(
        state: &PullRequestStackActionState,
        number: u64,
    ) -> Vec<&tcode_protocol::PullRequestStackLayerState> {
        let Some(target) = state.layers.iter().position(|layer| layer.number == number) else {
            return Vec::new();
        };
        state.layers[..=target]
            .iter()
            .filter(|layer| layer.state != PullRequestState::Merged)
            .collect()
    }

    fn blockers(&self, state: &PullRequestStackActionState) -> Vec<String> {
        let number = self.target.offer.key.number;
        let scope = Self::scope(state, number);
        let mut blockers = Vec::new();
        if scope.is_empty() {
            blockers.push(tr("changed_layers"));
        }
        for layer in &scope {
            if layer.state != PullRequestState::Open || layer.draft {
                blockers.push(tr_with(
                    "note_blocked",
                    &[
                        ("number", layer.number.to_string()),
                        ("state", state_lower(layer.state, layer.draft)),
                    ],
                ));
            } else if layer.head.is_none() {
                blockers.push(tr_with("no_head", &[("number", layer.number.to_string())]));
            }
        }
        if state.merge_methods.is_empty() {
            blockers.push(tr("no_methods"));
        }
        if !state.can_merge {
            blockers.push(tr("no_merge_right"));
        }
        blockers
    }

    fn confirm(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(Ok(state)), Some(method)) = (&self.state, self.method) else {
            return;
        };
        if self.sending || !self.blockers(state).is_empty() || self.notice.is_some() {
            return;
        }
        let number = self.target.offer.key.number;
        let scope = Self::scope(state, number);
        let heads: Vec<_> = scope
            .iter()
            .filter_map(|layer| {
                Some(PullRequestStackHead {
                    number: layer.number,
                    head: layer.head.clone()?,
                })
            })
            .collect();
        let layers: Vec<_> = scope.iter().map(|layer| layer.number).collect();
        if self.make_default
            && let Some((project, _)) = self.project.clone()
        {
            self.target.store.update(cx, |store, _| {
                store.set_project_merge_method(project, method)
            });
        }
        self.sending = true;
        cx.notify();
        let stack = state.stack;
        let base = state.base.clone();
        let task = send(
            &self.target,
            tcode_protocol::PullRequestAction::MergeStack {
                stack,
                heads,
                method,
            },
            cx,
        );
        let target = self.target.clone();
        cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.sending = false;
                let answer = Answer {
                    stack: (
                        target.offer.key.host.clone(),
                        target.offer.key.repository.clone(),
                        stack,
                    ),
                    target: target.offer.key.number,
                    layers: &layers,
                    base: &base,
                    branches: branches(
                        target.store.read(cx).pull_requests(&target.session),
                        &target.offer.key,
                    ),
                    url: Some(target.offer.url.clone()).filter(|url| !url.is_empty()),
                    rebase: false,
                    late: false,
                };
                if let Some(note) = answer_toast(&answer, &result) {
                    window.push_notification(note, cx);
                }
                if stays(&result) {
                    this.notice = notice_text(&result);
                } else {
                    window.close_dialog(cx);
                }
                (target.done)(&result, window, cx);
                cx.notify();
            });
        })
        .detach();
    }
}

impl Render for MergeStackDialog {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let target = &self.target;
        let key = &target.offer.key;
        let theme = cx.theme().clone();
        let muted = theme.muted_foreground;
        let state = self.state.as_ref().and_then(|state| state.as_ref().ok());
        let view = cx.entity();
        let stored = target
            .offer
            .stack
            .as_ref()
            .map(|offer| offer.map().stack.clone());
        let base = state
            .map(|state| state.base.clone())
            .or_else(|| stored.as_ref().map(|stack| stack.base.clone()))
            .unwrap_or_default();
        let stack_number = state
            .map(|state| state.stack)
            .or_else(|| stored.as_ref().map(|stack| stack.number))
            .unwrap_or_default();
        let mut facts = v_flex().gap_1p5().child(fact(
            crate::tr!("pull_requests.merge.repository").into_owned(),
            div()
                .font_family(theme.mono_font_family.clone())
                .child(key.repository.clone()),
            cx,
        ));
        facts = facts.child(fact(
            tr("fact_stack"),
            tr_with(
                "fact_stack_value",
                &[("stack", stack_number.to_string()), ("base", base.clone())],
            ),
            cx,
        ));
        let method = match (state, self.method) {
            (Some(state), Some(chosen)) if state.merge_methods.len() >= 2 => {
                let segments = state.merge_methods.iter().map(|method| {
                    let method = *method;
                    let view = view.clone();
                    Choice::new(
                        SharedString::from(format!("pr-merge-method-{method:?}")),
                        super::lifecycle::segment_label(method),
                    )
                    .selected(chosen == method)
                    .on_select(move |_, cx| {
                        view.update(cx, |dialog, cx| {
                            dialog.method = Some(method);
                            cx.notify();
                        })
                    })
                });
                ChoiceGroup::new("pr-merge-method")
                    .compact(target.window_state.read(cx).compact)
                    .aria_label(crate::tr!("pull_requests.merge.method"))
                    .w_full()
                    .choices(segments)
                    .into_any_element()
            }
            (Some(_), Some(chosen)) => div().child(method_label(chosen)).into_any_element(),
            (Some(_), None) => div().into_any_element(),
            (None, _) => Spinner::new().into_any_element(),
        };
        let default_box = self
            .method
            .filter(|method| state.is_some() && Some(*method) != self.project_default)
            .zip(self.project.clone())
            .map(|(method, (_, project))| {
                let view = view.clone();
                Checkbox::new("pr-stack-merge-default")
                    .label(
                        crate::tr!(
                            "pull_requests.merge.make_default",
                            method = super::lifecycle::segment_label(method),
                            project = project
                        )
                        .into_owned(),
                    )
                    .checked(self.make_default)
                    .disabled(self.sending)
                    .on_click(move |checked, _, cx| {
                        let checked = *checked;
                        view.update(cx, |dialog, cx| {
                            dialog.make_default = checked;
                            cx.notify();
                        })
                    })
            });
        let mut body = v_flex().gap_3().child(facts).child(
            v_flex()
                .w_full()
                .gap_2()
                .child(
                    div()
                        .text_caption(cx)
                        .child(crate::tr!("pull_requests.merge.method").into_owned()),
                )
                .child(method)
                .children(default_box),
        );
        let mut ready = false;
        match &self.state {
            None => {
                // Until GitHub answers, the stored scope shows with no heads.
                if let Some(offer) = &target.offer.stack {
                    let map = offer.map();
                    let rows: Vec<_> = map.rows[..=map.selected]
                        .iter()
                        .filter(|row| row.layer.state != PullRequestState::Merged)
                        .enumerate()
                        .map(|(index, row)| {
                            material::group_row(cx)
                                .child(step_number(index, cx))
                                .child(div().text_code(cx).child(format!("#{}", row.layer.number)))
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w_0()
                                        .overflow_hidden()
                                        .text_body(false, cx)
                                        .child(crate::widgets::fade_text(
                                            row.link
                                                .and_then(|link| link.snapshot.as_ref())
                                                .map(|snapshot| snapshot.title.clone())
                                                .unwrap_or_else(|| row.layer.head_branch.clone()),
                                        )),
                                )
                                .child(Spinner::new())
                                .into_any_element()
                        })
                        .collect();
                    body = body.child(scope_box(rows, cx));
                }
            }
            Some(Err(reason)) => {
                body = body.child(
                    v_flex()
                        .gap_1()
                        .text_secondary(false, cx)
                        .text_color(theme.danger)
                        .child(
                            crate::tr!(
                                "pull_requests.merge.load_failed",
                                number = key.number.to_string()
                            )
                            .into_owned(),
                        )
                        .when(!reason.is_empty(), |failure| failure.child(reason.clone()))
                        .child(
                            Button::new("pr-stack-merge-retry")
                                .outline()
                                .xsmall()
                                .label(crate::tr!("pull_requests.detail.retry"))
                                .on_click(cx.listener(|this, _, _, cx| this.load(cx))),
                        ),
                );
            }
            Some(Ok(state)) => {
                let scope = Self::scope(state, key.number);
                let numbers: Vec<_> = scope.iter().map(|layer| layer.number).collect();
                let selected_index = state
                    .layers
                    .iter()
                    .position(|layer| layer.number == key.number)
                    .unwrap_or_default();
                let merged_below: Vec<_> = state.layers[..selected_index.min(state.layers.len())]
                    .iter()
                    .filter(|layer| layer.state == PullRequestState::Merged)
                    .map(|layer| layer.number)
                    .collect();
                let rows: Vec<_> = scope
                    .iter()
                    .enumerate()
                    .map(|(index, layer)| {
                        let tag = if layer.state != PullRequestState::Open || layer.draft {
                            let color = if layer.draft {
                                theme.warning
                            } else {
                                theme.danger
                            };
                            h_flex()
                                .gap_1()
                                .text_color(color)
                                .child(Icon::new(IconName::Lock))
                                .child(tr(if layer.draft {
                                    "blocks_draft"
                                } else {
                                    "blocks_closed"
                                }))
                                .into_any_element()
                        } else {
                            div()
                                .text_color(muted)
                                .child(tr("tag_ready"))
                                .into_any_element()
                        };
                        layer_row(Some(index), layer, tag, false, false, cx)
                    })
                    .collect();
                body = body.child(
                    v_flex()
                        .gap_1()
                        .child(div().text_caption(cx).child(tr_with(
                            "scope_label",
                            &[("count", scope.len().to_string())],
                        )))
                        .children(merged_below.iter().map(|number| {
                            div()
                                .text_secondary(false, cx)
                                .child(tr_with("already_merged", &[("number", number.to_string())]))
                        }))
                        .child(scope_box(rows, cx)),
                );
                let above: Vec<_> = state.layers[(selected_index + 1).min(state.layers.len())..]
                    .iter()
                    .filter(|layer| layer.state == PullRequestState::Open)
                    .collect();
                if !above.is_empty() {
                    body = body.child(
                        v_flex()
                            .gap_1()
                            .child(div().text_caption(cx).child(tr_with(
                                "above_label",
                                &[("count", above.len().to_string())],
                            )))
                            .children(above.iter().map(|layer| {
                                layer_row(
                                    None,
                                    layer,
                                    div()
                                        .child(tr_with(
                                            "above_tag",
                                            &[("base", state.base.clone())],
                                        ))
                                        .into_any_element(),
                                    false,
                                    true,
                                    cx,
                                )
                            })),
                    );
                }
                let mut notes = vec![line(
                    IconName::GitMerge,
                    theme.info,
                    tr_with(
                        "note_merge",
                        &[
                            ("count", scope.len().to_string()),
                            ("base", state.base.clone()),
                        ],
                    ),
                )];
                if state.merge_queue {
                    notes.push(line(IconName::ListOrdered, theme.info, tr("note_queue")));
                }
                let blockers = self.blockers(state);
                for blocker in &blockers {
                    notes.push(line(IconName::Lock, theme.danger, blocker.clone()));
                }
                let links = target.store.read(cx).pull_requests(&target.session);
                let snapshot = |number: u64| {
                    links
                        .iter()
                        .find(|link| {
                            link.visible()
                                && link.key.host == key.host
                                && link.key.repository == key.repository
                                && link.key.number == number
                        })
                        .and_then(|link| link.snapshot.clone())
                };
                let failing: Vec<_> = numbers
                    .iter()
                    .filter(|number| {
                        snapshot(**number).is_some_and(|snapshot| {
                            snapshot.checks_state == Some(core_pr::ChecksState::Failing)
                        })
                    })
                    .collect();
                for number in failing.iter().take(3) {
                    notes.push(line(
                        IconName::CircleX,
                        theme.danger,
                        tr_with("note_checks", &[("number", number.to_string())]),
                    ));
                }
                if failing.len() > 3 {
                    notes.push(
                        div()
                            .child(tr_with(
                                "summary_more",
                                &[("count", (failing.len() - 3).to_string())],
                            ))
                            .into_any_element(),
                    );
                }
                for number in &numbers {
                    if snapshot(*number).is_some_and(|snapshot| {
                        snapshot.review_decision == Some(core_pr::ReviewDecision::ChangesRequested)
                    }) {
                        notes.push(line(
                            IconName::MessageSquareWarning,
                            theme.danger,
                            tr_with("note_changes", &[("number", number.to_string())]),
                        ));
                    }
                }
                if let Some(PullRequestStackOperation {
                    started_at,
                    kind: StackOperationKind::MergeUnconfirmed { .. },
                    ..
                }) = target
                    .offer
                    .stack
                    .as_ref()
                    .and_then(|offer| offer.operation.as_ref())
                {
                    notes.push(line(
                        IconName::CircleQuestionMark,
                        theme.warning,
                        tr_with(
                            "previous_unconfirmed",
                            &[("ago", super::detail::ago(*started_at))],
                        ),
                    ));
                }
                body = body.child(self::notes(cx).children(notes));
                let changed = self.notice.clone();
                if let Some(changed) = &changed {
                    body = body.child(
                        warning_wash(cx)
                            .child(line(IconName::TriangleAlert, theme.warning, tr("changed")))
                            .child(div().child(changed.clone()))
                            .child(
                                Button::new("pr-stack-review-again")
                                    .outline()
                                    .xsmall()
                                    .label(tr("review_again"))
                                    .on_click(cx.listener(|this, _, _, cx| this.load(cx))),
                            ),
                    );
                }
                ready = blockers.is_empty() && changed.is_none() && self.method.is_some();
            }
        }
        let count = state
            .map(|state| Self::scope(state, key.number).len())
            .unwrap_or_default();
        let confirm_label = if self.sending {
            tr("submitting")
        } else if count <= 1 {
            tr_with("merge_confirm_one", &[("number", key.number.to_string())])
        } else {
            tr_with("merge_confirm", &[("count", count.to_string())])
        };
        body.child(
            h_flex()
                .gap_2()
                .justify_end()
                .child(
                    Button::new("pr-stack-merge-cancel")
                        .outline()
                        .small()
                        .disabled(self.sending)
                        .label(crate::tr!("pull_requests.actions.cancel"))
                        .on_click(|_, window, cx| window.close_dialog(cx)),
                )
                .child(
                    Button::new("pr-stack-merge-confirm")
                        .primary()
                        .small()
                        .loading(self.sending)
                        .disabled(!ready || self.sending)
                        .label(confirm_label)
                        .on_click(cx.listener(|this, _, window, cx| this.confirm(window, cx))),
                ),
        )
    }
}

pub(super) fn open_merge_dialog(target: Target, window: &mut Window, cx: &mut App) {
    let workspace = target.store.read(cx);
    let project = workspace
        .thread_meta(&target.session)
        .and_then(|meta| meta.project_id.clone())
        .and_then(|id| {
            workspace
                .projects()
                .into_iter()
                .find(|project| project.id == id)
                .map(|project| (project.id, project.name))
        });
    let project_default = project
        .as_ref()
        .and_then(|(id, _)| workspace.settings().project_merge_methods.get(id).copied());
    let scope = target
        .offer
        .stack
        .as_ref()
        .map(|offer| offer.map().scope().len())
        .unwrap_or(1);
    let number = target.offer.key.number.to_string();
    let title = if scope <= 1 {
        tr_with("merge_title_one", &[("number", number)])
    } else {
        tr_with("merge_title", &[("number", number)])
    };
    let store = target.store.clone();
    let dialog = cx.new(|cx| {
        let mut dialog = MergeStackDialog {
            target,
            state: None,
            method: None,
            project,
            project_default,
            make_default: false,
            sending: false,
            notice: None,
            _observe: cx.observe(&store, |_, _, cx| cx.notify()),
        };
        dialog.load(cx);
        dialog
    });
    window.open_dialog(cx, move |base, _, cx| {
        let sending = dialog.read(cx).sending;
        base.title(title.clone())
            .keyboard(!sending)
            .close_button(!sending)
            .overlay_closable(!sending)
            .footer(crate::overlay::DialogActions::new())
            .content({
                let dialog = dialog.clone();
                move |content, _, _| content.child(dialog.clone())
            })
    });
}

/// The Rebase stack confirmation, which turns into the rebase's progress once the host started
/// it, and its end once the host reported it.
struct RebaseStackDialog {
    target: Target,
    state: Option<Result<PullRequestStackActionState, String>>,
    sending: bool,
    started: bool,
    notice: Option<String>,
    /// The host's record of the rebase as last read.
    last: Option<StackOperationKind>,
    _observe: gpui::Subscription,
}

impl RebaseStackDialog {
    fn load(&mut self, cx: &mut Context<Self>) {
        self.state = None;
        self.notice = None;
        let task = self.target.store.update(cx, |store, cx| {
            store.read_pull_request(
                self.target.session.clone(),
                self.target.offer.key.clone(),
                PullRequestRead::StackState { rebase: true },
                cx,
            )
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                this.state = Some(match result {
                    Ok((PullRequestReadResponse::StackState(state), _)) => Ok(state),
                    Ok(_) => Err(String::new()),
                    Err(error) => Err(super::detail::reason(&error)),
                });
                cx.notify();
            });
        })
        .detach();
    }

    fn unmerged(
        state: &PullRequestStackActionState,
    ) -> Vec<&tcode_protocol::PullRequestStackLayerState> {
        state
            .layers
            .iter()
            .filter(|layer| layer.state != PullRequestState::Merged)
            .collect()
    }

    fn blockers(&self, state: &PullRequestStackActionState, machine: &str) -> Vec<String> {
        let layers = Self::unmerged(state);
        let mut blockers = Vec::new();
        if layers.is_empty() {
            blockers.push(tr("rebase_nothing"));
        }
        let denied: Vec<_> = layers
            .iter()
            .filter(|layer| layer.push == Some(PullRequestStackPushAccess::Denied))
            .map(|layer| layer.number)
            .collect();
        if !denied.is_empty() {
            blockers.push(tr_with(
                "rebase_blocked_access",
                &[("layers", list(&denied))],
            ));
        }
        for layer in &layers {
            if layer.state == PullRequestState::Closed {
                blockers.push(tr_with(
                    "rebase_blocked_closed",
                    &[("number", layer.number.to_string())],
                ));
            } else if layer.head.is_none() {
                blockers.push(tr_with("no_head", &[("number", layer.number.to_string())]));
            }
        }
        if state.git_identity == Some(false) {
            blockers.push(tr_with(
                "rebase_blocked_identity",
                &[("machine", machine.to_owned())],
            ));
        }
        blockers
    }

    fn confirm(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(Ok(state)) = &self.state else { return };
        let machine = machine_label(Some(self.target.store.read(cx)));
        if self.sending || !self.blockers(state, &machine).is_empty() || self.notice.is_some() {
            return;
        }
        let heads: Vec<_> = Self::unmerged(state)
            .iter()
            .filter_map(|layer| {
                Some(PullRequestStackHead {
                    number: layer.number,
                    head: layer.head.clone()?,
                })
            })
            .collect();
        let layers: Vec<_> = heads.iter().map(|head| head.number).collect();
        let stack = state.stack;
        let base = state.base.clone();
        self.sending = true;
        cx.notify();
        let task = send(
            &self.target,
            tcode_protocol::PullRequestAction::RebaseStack { stack, heads },
            cx,
        );
        let target = self.target.clone();
        cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.sending = false;
                if result == PullRequestActionResult::RebaseStarted {
                    this.started = true;
                } else {
                    let answer = Answer {
                        stack: (
                            target.offer.key.host.clone(),
                            target.offer.key.repository.clone(),
                            stack,
                        ),
                        target: target.offer.key.number,
                        layers: &layers,
                        base: &base,
                        branches: branches(
                            target.store.read(cx).pull_requests(&target.session),
                            &target.offer.key,
                        ),
                        url: Some(target.offer.url.clone()).filter(|url| !url.is_empty()),
                        rebase: true,
                        late: false,
                    };
                    if let Some(note) = answer_toast(&answer, &result) {
                        window.push_notification(note, cx);
                    }
                    if stays(&result) {
                        this.notice = notice_text(&result);
                    } else {
                        window.close_dialog(cx);
                    }
                }
                (target.done)(&result, window, cx);
                cx.notify();
            });
        })
        .detach();
    }

    fn confirm_view(&self, cx: &mut Context<Self>) -> AnyElement {
        let target = &self.target;
        let theme = cx.theme().clone();
        let muted = theme.muted_foreground;
        let machine = machine_label(Some(target.store.read(cx)));
        let base = target
            .offer
            .stack
            .as_ref()
            .map(StackOffer::base)
            .unwrap_or_default();
        let mut body = v_flex().gap_3().child(
            v_flex()
                .gap_1p5()
                .child(fact(
                    crate::tr!("pull_requests.merge.repository").into_owned(),
                    div()
                        .font_family(theme.mono_font_family.clone())
                        .child(target.offer.key.repository.clone()),
                    cx,
                ))
                .child(fact(
                    tr("fact_onto"),
                    div().font_family(theme.mono_font_family.clone()).child(
                        self.state
                            .as_ref()
                            .and_then(|state| state.as_ref().ok())
                            .map(|state| state.base.clone())
                            .unwrap_or(base),
                    ),
                    cx,
                ))
                .child(fact(
                    tr("fact_runs_on"),
                    tr_with("runs_on_value", &[("machine", machine.clone())]),
                    cx,
                )),
        );
        let mut ready = false;
        let mut count = 0;
        match &self.state {
            None => body = body.child(h_flex().justify_center().child(Spinner::new())),
            Some(Err(reason)) => {
                body = body.child(
                    v_flex()
                        .gap_1()
                        .text_secondary(false, cx)
                        .text_color(theme.danger)
                        .child(
                            crate::tr!(
                                "pull_requests.merge.load_failed",
                                number = target.offer.key.number.to_string()
                            )
                            .into_owned(),
                        )
                        .when(!reason.is_empty(), |failure| failure.child(reason.clone()))
                        .child(
                            Button::new("pr-stack-rebase-retry")
                                .outline()
                                .xsmall()
                                .label(crate::tr!("pull_requests.detail.retry"))
                                .on_click(cx.listener(|this, _, _, cx| this.load(cx))),
                        ),
                )
            }
            Some(Ok(state)) => {
                let layers = Self::unmerged(state);
                count = layers.len();
                let rows: Vec<_> = layers
                    .iter()
                    .enumerate()
                    .map(|(index, layer)| {
                        let tag = match layer.push {
                            Some(PullRequestStackPushAccess::Write) => h_flex()
                                .gap_1()
                                .text_color(muted)
                                .child(Icon::new(IconName::Check))
                                .child(tr("access_write")),
                            Some(PullRequestStackPushAccess::MaintainerCanModify) => h_flex()
                                .gap_1()
                                .text_color(muted)
                                .child(Icon::new(IconName::Check))
                                .child(tr("access_maintainer")),
                            _ => h_flex()
                                .gap_1()
                                .text_color(theme.danger)
                                .child(Icon::new(IconName::Lock))
                                .child(tr("access_denied")),
                        };
                        layer_row(Some(index), layer, tag.into_any_element(), true, false, cx)
                    })
                    .collect();
                let merged: Vec<_> = state
                    .layers
                    .iter()
                    .filter(|layer| layer.state == PullRequestState::Merged)
                    .map(|layer| layer.number)
                    .collect();
                body = body.child(
                    v_flex()
                        .gap_1()
                        .child(div().text_caption(cx).child(tr_with(
                            "rebase_layers_label",
                            &[("count", count.to_string())],
                        )))
                        .children(merged.iter().map(|number| {
                            div()
                                .text_secondary(false, cx)
                                .child(tr_with("merged_skipped", &[("number", number.to_string())]))
                        }))
                        .child(scope_box(rows, cx)),
                );
                body = body
                    .child(
                        notes(cx)
                            .child(line(IconName::RefreshCw, muted, tr("rebase_step_own")))
                            .child(line(IconName::Upload, muted, tr("rebase_step_lease")))
                            .child(line(
                                IconName::CircleDashed,
                                muted,
                                tr("rebase_step_checks"),
                            ))
                            .child(line(IconName::GitBranch, muted, tr("rebase_step_local"))),
                    )
                    .child(div().text_secondary(false, cx).child(tr_with(
                        "rebase_no_promise",
                        &[("machine", machine.clone())],
                    )));
                let blockers = self.blockers(state, &machine);
                if !blockers.is_empty() {
                    body =
                        body.child(
                            notes(cx).children(blockers.iter().map(|blocker| {
                                line(IconName::Lock, theme.danger, blocker.clone())
                            })),
                        );
                }
                let changed = self.notice.clone();
                if let Some(changed) = &changed {
                    body = body.child(
                        warning_wash(cx)
                            .child(line(IconName::TriangleAlert, theme.warning, tr("changed")))
                            .child(div().child(changed.clone()))
                            .child(
                                Button::new("pr-stack-rebase-review-again")
                                    .outline()
                                    .xsmall()
                                    .label(tr("review_again"))
                                    .on_click(cx.listener(|this, _, _, cx| this.load(cx))),
                            ),
                    );
                }
                ready = blockers.is_empty() && changed.is_none();
            }
        }
        if self.state.is_none() {
            count = target
                .offer
                .stack
                .as_ref()
                .map_or(0, |offer| offer.map().unmerged().len());
        }
        let label = if self.sending {
            tr("starting")
        } else if count == 1 {
            tr("rebase_confirm_one")
        } else {
            tr_with("rebase_confirm", &[("count", count.to_string())])
        };
        body.child(
            h_flex()
                .gap_2()
                .justify_end()
                .child(
                    Button::new("pr-stack-rebase-cancel")
                        .outline()
                        .small()
                        .disabled(self.sending)
                        .label(crate::tr!("pull_requests.actions.cancel"))
                        .on_click(|_, window, cx| window.close_dialog(cx)),
                )
                .child(
                    Button::new("pr-stack-rebase-confirm")
                        .primary()
                        .small()
                        .icon(IconName::RefreshCw)
                        .loading(self.sending)
                        .disabled(!ready || self.sending)
                        .label(label)
                        .on_click(cx.listener(|this, _, window, cx| this.confirm(window, cx))),
                ),
        )
        .into_any_element()
    }
}

impl Render for RebaseStackDialog {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if !self.started {
            return self.confirm_view(cx);
        }
        progress_view(&self.target, &mut self.last, cx)
    }
}

/// The rows of a rebase as the host runs it, or as the host keeps its end until the next sync.
fn progress_view(
    target: &Target,
    last: &mut Option<StackOperationKind>,
    cx: &mut App,
) -> AnyElement {
    let theme = cx.theme().clone();
    let muted = theme.muted_foreground;
    let store = target.store.read(cx);
    let machine = machine_label(Some(store));
    let offer = StackOffer::new(
        &target.offer.key,
        store.pull_requests(&target.session),
        store
            .thread_meta(&target.session)
            .map_or(&[][..], |meta| meta.pull_request_operations.as_slice()),
    );
    let base = offer.as_ref().map(StackOffer::base).unwrap_or_default();
    let mut started_at = None;
    if let Some(operation) = offer.and_then(|offer| offer.operation) {
        if let StackOperationKind::Rebasing { .. } = operation.kind {
            started_at = Some(operation.started_at);
        }
        if let StackOperationKind::Rebasing { .. } | StackOperationKind::RebaseEnded { .. } =
            operation.kind
        {
            *last = Some(operation.kind);
        }
    }
    // Once the sync drops the record, the view keeps the rows it last read.
    let (last, ended): (&[core_pr::StackRebaseLayer], bool) = match last.as_ref() {
        Some(StackOperationKind::Rebasing { layers }) => (layers, false),
        Some(StackOperationKind::RebaseEnded { layers }) => (layers, true),
        _ => (&[], false),
    };
    let rows: Vec<_> = last
        .iter()
        .enumerate()
        .map(|(index, layer)| {
            let below = index
                .checked_sub(1)
                .and_then(|below| last.get(below))
                .map(|below| below.number);
            let (icon, text): (AnyElement, String) = match &layer.step {
                StackRebaseStep::Waiting => (
                    Icon::new(IconName::CircleDashed)
                        .text_color(muted)
                        .into_any_element(),
                    tr("step_waiting"),
                ),
                StackRebaseStep::Rebasing => (
                    Spinner::new().into_any_element(),
                    match below {
                        Some(below) => tr_with("step_rebasing", &[("number", below.to_string())]),
                        None => tr_with("step_rebasing_base", &[("base", base.clone())]),
                    },
                ),
                StackRebaseStep::Pushing => (Spinner::new().into_any_element(), tr("step_pushing")),
                StackRebaseStep::Pushed { from, to } => (
                    Icon::new(IconName::CircleCheck)
                        .text_color(theme.success)
                        .into_any_element(),
                    tr_with("step_pushed", &[("from", short(from)), ("to", short(to))]),
                ),
                StackRebaseStep::AlreadyCurrent => (
                    Icon::new(IconName::Check)
                        .text_color(muted)
                        .into_any_element(),
                    tr("step_current"),
                ),
                StackRebaseStep::Failed { reason } => (
                    Icon::new(match reason {
                        StackRebaseFailure::PushUnconfirmed => IconName::CircleQuestionMark,
                        _ => IconName::CircleX,
                    })
                    .text_color(match reason {
                        StackRebaseFailure::PushUnconfirmed => theme.warning,
                        _ => theme.danger,
                    })
                    .into_any_element(),
                    match reason {
                        StackRebaseFailure::Conflict => tr("step_conflict"),
                        StackRebaseFailure::LeaseRefused => tr("step_lease"),
                        StackRebaseFailure::PushUnconfirmed => tr("step_unknown"),
                        StackRebaseFailure::Git { step, .. } => {
                            tr_with("step_git", &[("step", git_step(*step))])
                        }
                    },
                ),
                StackRebaseStep::NotStarted => (
                    Icon::new(IconName::Minus)
                        .text_color(muted)
                        .into_any_element(),
                    tr("step_not_started"),
                ),
            };
            material::group_row(cx)
                .id(SharedString::from(format!(
                    "pr-stack-progress-{}",
                    layer.number
                )))
                .text_token(&theme.tokens.typography.xs)
                .child(step_number(index, cx))
                .child(icon)
                .child(div().text_code(cx).child(format!("#{}", layer.number)))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .overflow_hidden()
                        .text_meta_mono(false, cx)
                        .child(crate::widgets::fade_text(layer.branch.clone())),
                )
                .child(div().flex_none().child(text))
                .into_any_element()
        })
        .collect();
    let mut body = v_flex().gap_2().child(scope_box(rows, cx));
    if let Some(started) = started_at {
        body = body.child(div().text_secondary(false, cx).child(tr_with(
            "progress_started",
            &[("ago", super::detail::ago(started)), ("machine", machine)],
        )));
    }
    let stopped = last.iter().find_map(|layer| match &layer.step {
        StackRebaseStep::Failed { reason } => Some((layer.number, reason)),
        _ => None,
    });
    if let Some((failed, reason)) = stopped.filter(|_| ended) {
        let numbers = |wanted: fn(&StackRebaseStep) -> bool| -> Vec<u64> {
            last.iter()
                .filter(|layer| wanted(&layer.step))
                .map(|layer| layer.number)
                .collect()
        };
        let pushed = numbers(|step| matches!(step, StackRebaseStep::Pushed { .. }));
        let mut rest = vec![failed];
        rest.extend(numbers(|step| matches!(step, StackRebaseStep::NotStarted)));
        let state = if pushed.is_empty() {
            tr_with("recovery_none", &[("rest", list(&rest))])
        } else {
            tr_with(
                "recovery_state",
                &[("pushed", list(&pushed)), ("rest", list(&rest))],
            )
        };
        let layer = last.iter().position(|layer| layer.number == failed);
        let branch = layer
            .map(|index| last[index].branch.clone())
            .unwrap_or_default();
        let below = layer
            .and_then(|index| index.checked_sub(1))
            .map(|index| last[index].branch.clone())
            .unwrap_or_else(|| base.clone());
        let advice = match reason {
            StackRebaseFailure::Conflict => tr_with(
                "recovery_conflict",
                &[
                    ("number", failed.to_string()),
                    ("branch", branch.clone()),
                    ("below", below.clone()),
                ],
            ),
            StackRebaseFailure::LeaseRefused | StackRebaseFailure::PushUnconfirmed => {
                tr_with("recovery_lease", &[("branch", branch.clone())])
            }
            StackRebaseFailure::Git { .. } => tr("recovery_git"),
        };
        let can_ask = !target
            .store
            .read(cx)
            .session_status()
            .is_none_or(|status| status.conversation_read_only);
        let ask = matches!(reason, StackRebaseFailure::Conflict).then(|| {
            let store = target.store.clone();
            let window_state = target.window_state.clone();
            let session = target.session.clone();
            let text = tr_with(
                "ask_rebase_conflict",
                &[
                    ("number", failed.to_string()),
                    ("branch", branch.clone()),
                    ("below", below.clone()),
                ],
            );
            Button::new("pr-stack-ask")
                .outline()
                .xsmall()
                .disabled(!can_ask)
                .label(crate::tr!(if can_ask {
                    "pull_requests.actions.ask_agent"
                } else {
                    "pull_requests.actions.thread_unavailable"
                }))
                .on_click(move |_, window, cx| {
                    store.update(cx, |store, cx| {
                        store.append_to_composer(session.clone(), text.clone(), cx)
                    });
                    window.close_dialog(cx);
                    if window_state.read(cx).destination()
                        == crate::window_state::Destination::PullRequest
                    {
                        window_state.update(cx, |state, cx| state.back(cx));
                    }
                })
        });
        let url = format!(
            "https://{}/{}/pull/{failed}",
            target.offer.key.host, target.offer.key.repository
        );
        body = body.child(
            warning_wash(cx)
                .child(div().child(state))
                .child(div().child(advice))
                .child(
                    h_flex().gap_2().items_center().children(ask).child(
                        Button::new("pr-stack-open-failed")
                            .ghost()
                            .xsmall()
                            .icon(IconName::ExternalLink)
                            .label(tr_with("open_layer", &[("number", failed.to_string())]))
                            .on_click(move |_, _, cx| cx.open_url(&url)),
                    ),
                ),
        );
    }
    body.child(
        h_flex().justify_end().child(
            Button::new("pr-stack-progress-close")
                .outline()
                .small()
                .label(tr(if ended { "done" } else { "hide" }))
                .on_click(|_, window, cx| window.close_dialog(cx)),
        ),
    )
    .into_any_element()
}

pub(super) fn open_rebase_dialog(target: Target, window: &mut Window, cx: &mut App) {
    let stack = target
        .offer
        .stack
        .as_ref()
        .map(|offer| offer.map().stack.number)
        .unwrap_or_default();
    let store = target.store.clone();
    let dialog = cx.new(|cx| {
        let mut dialog = RebaseStackDialog {
            target,
            state: None,
            sending: false,
            started: false,
            notice: None,
            last: None,
            _observe: cx.observe(&store, |_, _, cx| cx.notify()),
        };
        dialog.load(cx);
        dialog
    });
    window.open_dialog(cx, move |base, _, cx| {
        let dialog_state = dialog.read(cx);
        let sending = dialog_state.sending;
        let title = if dialog_state.started {
            tr_with("progress_title", &[("stack", stack.to_string())])
        } else {
            tr_with("rebase_title", &[("stack", stack.to_string())])
        };
        base.title(title)
            .keyboard(!sending)
            .close_button(!sending)
            .overlay_closable(!sending)
            .footer(crate::overlay::DialogActions::new())
            .content({
                let dialog = dialog.clone();
                move |content, _, _| content.child(dialog.clone())
            })
    });
}

/// The running rebase's progress, opened from its chip.
fn open_progress(target: Target, window: &mut Window, cx: &mut App) {
    let stack = target
        .offer
        .stack
        .as_ref()
        .map(|offer| offer.map().stack.number)
        .unwrap_or_default();
    let store = target.store.clone();
    let dialog = cx.new(|cx| RebaseStackDialog {
        target,
        state: None,
        sending: false,
        started: true,
        notice: None,
        last: None,
        _observe: cx.observe(&store, |_, _, cx| cx.notify()),
    });
    window.open_dialog(cx, move |base, _, _| {
        base.title(tr_with("progress_title", &[("stack", stack.to_string())]))
            .footer(crate::overlay::DialogActions::new())
            .content({
                let dialog = dialog.clone();
                move |content, _, _| content.child(dialog.clone())
            })
    });
}
