//! Reviewers and labels: the block at the top of the conversation and the one picker both are
//! changed through. A picker stages its changes and applies them together.

use std::collections::BTreeMap;

use gpui::{
    AnyElement, App, AppContext as _, Context, Entity, InteractiveElement as _, IntoElement,
    ParentElement as _, SharedString, StatefulInteractiveElement as _, Styled as _, Subscription,
    Window, div, prelude::FluentBuilder as _, px,
};
use gpui_base::{h_flex, v_flex};
use tcode_protocol::{
    ProtocolError, PullRequestAction, PullRequestActionResult, PullRequestLabelCandidates,
    PullRequestRead, PullRequestReadResponse, PullRequestReviewState, PullRequestReviewer,
    PullRequestReviewerCandidates, PullRequestReviewerKind,
};

use super::compose::{Sheet, Write};
use super::detail::{PullRequestView, reason};
use crate::{
    icon::{Icon, IconName},
    material,
    sizing::Sizable as _,
    theme::ActiveTheme as _,
    typography::TextRole as _,
    widgets::{
        Popover,
        button::{Button, ButtonVariants as _},
        checkbox::Checkbox,
        input::{Input, InputEvent, InputState},
        spinner::Spinner,
        tooltip::Tooltip,
    },
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PickerKind {
    Labels,
    Reviewers,
}

/// One row a picker offers: its name, whether the pull request has it, and how it is drawn.
#[derive(Clone)]
struct Row {
    /// The host's id, which the write names it by.
    id: String,
    key: String,
    reviewer: Option<PullRequestReviewer>,
    applied: bool,
    color: Option<String>,
    avatar: Option<String>,
    secondary: Option<String>,
}

pub(super) struct Picker {
    kind: PickerKind,
    filter: Entity<InputState>,
    _filter: Subscription,
    rows: Option<Vec<Row>>,
    complete: bool,
    error: Option<ProtocolError>,
    /// Rows whose check the reader changed, by key, with the check they now have.
    staged: BTreeMap<String, bool>,
    applying: bool,
}

/// A GitHub label colour as the dot shows it.
fn label_color(hex: Option<&str>, cx: &App) -> gpui::Hsla {
    hex.and_then(|hex| u32::from_str_radix(hex.trim_start_matches('#'), 16).ok())
        .map(|value| gpui::rgb(value).into())
        .unwrap_or(cx.theme().muted_foreground)
}

/// A label's colour swatch; no token sizes a swatch.
fn dot(color: gpui::Hsla) -> impl IntoElement {
    div().flex_none().size(px(10.)).rounded_full().bg(color)
}

impl PullRequestView {
    fn picker(&self) -> Option<&Picker> {
        self.writes()?.picker.as_ref()
    }

    fn picker_mut(&mut self) -> Option<&mut Picker> {
        self.writes_mut()?.picker.as_mut()
    }

    /// Opens a picker; its candidates are read now, never before.
    fn open_picker(&mut self, kind: PickerKind, window: &mut Window, cx: &mut Context<Self>) {
        let filter = cx.new(|cx| {
            InputState::new(window, cx).placeholder(crate::tr!(match kind {
                PickerKind::Labels => "pull_requests.meta.labels_search",
                PickerKind::Reviewers => "pull_requests.meta.reviewers_search",
            }))
        });
        let subscription = cx.subscribe(&filter, |_, _, event: &InputEvent, cx| {
            if matches!(event, InputEvent::Change) {
                cx.notify();
            }
        });
        filter.update(cx, |filter, cx| filter.focus(window, cx));
        if let Some(writes) = self.writes_mut() {
            writes.picker = Some(Picker {
                kind,
                filter,
                _filter: subscription,
                rows: None,
                complete: true,
                error: None,
                staged: BTreeMap::new(),
                applying: false,
            });
            writes.sheet = Some(match kind {
                PickerKind::Labels => Sheet::Labels,
                PickerKind::Reviewers => Sheet::Reviewers,
            });
        }
        let read = match kind {
            PickerKind::Labels => PullRequestRead::LabelCandidates,
            PickerKind::Reviewers => PullRequestRead::ReviewerCandidates,
        };
        self.read(read, cx, move |page, result| {
            let Some(picker) = page
                .writes
                .picker
                .as_mut()
                .filter(|picker| picker.kind == kind)
            else {
                return;
            };
            match result {
                Ok((PullRequestReadResponse::LabelCandidates(labels), _)) => {
                    picker.complete = labels.complete;
                    picker.rows = Some(label_rows(labels));
                }
                Ok((PullRequestReadResponse::ReviewerCandidates(reviewers), _)) => {
                    picker.complete = reviewers.complete;
                    picker.rows = Some(reviewer_rows(reviewers));
                }
                Ok(_) => {}
                Err(error) => picker.error = Some(error),
            }
        });
        cx.notify();
    }

    fn close_picker(&mut self, cx: &mut Context<Self>) {
        if let Some(writes) = self.writes_mut()
            && writes.picker.as_ref().is_none_or(|picker| !picker.applying)
        {
            writes.picker = None;
            writes.sheet = None;
        }
        cx.notify();
    }

    fn toggle_staged(&mut self, key: String, applied: bool, cx: &mut Context<Self>) {
        if let Some(picker) = self.picker_mut() {
            match picker.staged.get(&key).copied() {
                Some(_) => {
                    picker.staged.remove(&key);
                }
                None => {
                    picker.staged.insert(key, !applied);
                }
            }
        }
        cx.notify();
    }

    /// The staged change as one write; the host answers for all of it.
    fn apply_picker(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(current) = self.current.clone() else {
            return;
        };
        let Some(picker) = self.picker_mut() else {
            return;
        };
        if picker.applying || picker.staged.is_empty() {
            return;
        }
        picker.applying = true;
        let kind = picker.kind;
        let rows = picker.rows.clone().unwrap_or_default();
        let (adds, removes): (Vec<_>, Vec<_>) = picker
            .staged
            .iter()
            .map(|(key, checked)| (key.clone(), *checked))
            .partition(|(_, checked)| *checked);
        let names =
            |list: Vec<(String, bool)>| list.into_iter().map(|(key, _)| key).collect::<Vec<_>>();
        let (adds, removes) = (names(adds), names(removes));
        let reviewers = |keys: &[String]| -> Vec<PullRequestReviewer> {
            keys.iter()
                .filter_map(|id| rows.iter().find(|row| row.id == *id))
                .filter_map(|row| row.reviewer.clone())
                .collect()
        };
        let action = match kind {
            PickerKind::Labels => PullRequestAction::SetLabels {
                add: adds,
                remove: removes,
            },
            PickerKind::Reviewers => PullRequestAction::SetReviewers {
                add: reviewers(&adds),
                remove: reviewers(&removes),
            },
        };
        let write = match kind {
            PickerKind::Labels => Write::Labels,
            PickerKind::Reviewers => Write::Reviewers,
        };
        let task = self.command_write(action, cx);
        cx.notify();
        cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.written(&current, write, &result, window, cx);
                if let Some(page) = this.pages.get_mut(&current) {
                    if let Some(picker) = page.writes.picker.as_mut() {
                        picker.applying = false;
                    }
                    if !matches!(result, PullRequestActionResult::Rejected(_)) {
                        page.writes.picker = None;
                        page.writes.sheet = None;
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// The first row of the conversation: who is asked to review and what labels it wears.
    pub(super) fn meta_block(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let conversation = self.page()?.conversation.data.as_ref()?;
        let permissions = conversation.permissions.clone();
        let read_only = self.read_only(cx);
        let labelled = conversation.capabilities.labels;
        let (can_review, can_label) = (
            permissions.request_reviewers && !read_only,
            labelled && permissions.label && !read_only,
        );
        if conversation.reviewers.is_empty()
            && conversation.labels.is_empty()
            && !can_review
            && !can_label
        {
            return None;
        }
        let muted = cx.theme().muted_foreground;
        let compact = self.compact(cx);
        let reviewers: Vec<AnyElement> = conversation
            .reviewers
            .iter()
            .map(|state| {
                let (icon, color, word) = match state.verdict {
                    Some(PullRequestReviewState::Approved) => (
                        IconName::BadgeCheck,
                        cx.theme().success,
                        "pull_requests.meta.state_approved",
                    ),
                    Some(PullRequestReviewState::ChangesRequested) => (
                        IconName::MessageSquareWarning,
                        cx.theme().danger,
                        "pull_requests.meta.state_changes_requested",
                    ),
                    Some(_) => (
                        IconName::MessageSquare,
                        muted,
                        "pull_requests.meta.state_commented",
                    ),
                    None => (
                        IconName::CircleDot,
                        muted,
                        "pull_requests.meta.state_requested",
                    ),
                };
                let team = state.reviewer.kind == PullRequestReviewerKind::Team;
                let name = if team {
                    let owner = self
                        .current
                        .as_ref()
                        .and_then(|(_, key)| key.repository.split('/').next().map(str::to_owned))
                        .unwrap_or_default();
                    format!("@{owner}/{}", state.reviewer.login)
                } else {
                    state.reviewer.login.clone()
                };
                let tooltip = crate::tr!(
                    "pull_requests.meta.reviewer_state",
                    login = name.clone(),
                    state = crate::tr!(word).into_owned()
                )
                .into_owned();
                h_flex()
                    .id(SharedString::from(format!("pr-reviewer-{name}")))
                    .gap_1()
                    .items_center()
                    .when(!team, |chip| {
                        chip.child(self.avatar(
                            &state.reviewer.login,
                            state.avatar_url.as_deref(),
                            16.,
                            cx,
                        ))
                    })
                    .child(name)
                    .child(Icon::new(icon).text_color(color))
                    .tooltip(move |window, cx| Tooltip::new(tooltip.clone()).build(window, cx))
                    .into_any_element()
            })
            .collect();
        let labels: Vec<AnyElement> = conversation
            .labels
            .iter()
            .map(|label| {
                let description = label.description.clone();
                h_flex()
                    .id(SharedString::from(format!("pr-label-{}", label.name)))
                    .px(cx.theme().tokens.spacing.sm)
                    .gap_1()
                    .items_center()
                    .rounded(material::radius_chip(cx))
                    .border_1()
                    .border_color(cx.theme().border)
                    .bg(cx.theme().secondary)
                    .text_token(&cx.theme().tokens.typography.xs)
                    .child(dot(label_color(label.color.as_deref(), cx)))
                    .child(label.name.clone())
                    .when_some(description, |chip, description| {
                        chip.tooltip(move |window, cx| {
                            Tooltip::new(description.clone()).build(window, cx)
                        })
                    })
                    .into_any_element()
            })
            .collect();
        let row = |selector: &'static str,
                   icon: IconName,
                   title: &'static str,
                   empty: &'static str,
                   chips: Vec<AnyElement>,
                   edit: Option<AnyElement>,
                   cx: &App| {
            h_flex()
                .debug_selector(|| selector.into())
                .gap_2()
                .items_center()
                .flex_wrap()
                .child(
                    // The caption column is as wide as the longer of the two captions; no
                    // token names a label column.
                    h_flex()
                        .min_w(px(72.))
                        .gap_1()
                        .items_center()
                        .text_color(muted)
                        .child(Icon::new(icon))
                        .child(crate::tr!(title)),
                )
                .when(chips.is_empty(), |row| {
                    row.child(div().text_color(muted).child(crate::tr!(empty)))
                })
                .children(chips)
                .children(edit)
                .text_color(cx.theme().foreground)
        };
        let reviewers_edit = can_review.then(|| self.picker_popover(PickerKind::Reviewers, cx));
        let labels_edit = can_label.then(|| self.picker_popover(PickerKind::Labels, cx));
        Some(
            v_flex()
                .py_2()
                .gap_1()
                .border_b_1()
                .border_color(cx.theme().border)
                .text_secondary(compact, cx)
                .child(row(
                    "pr-meta-reviewers",
                    IconName::Users,
                    "pull_requests.meta.reviewers",
                    "pull_requests.meta.no_reviewers",
                    reviewers,
                    reviewers_edit,
                    cx,
                ))
                // A host without labels may still report some, which show read-only.
                .when(labelled || !conversation.labels.is_empty(), |block| {
                    block.child(row(
                        "pr-meta-labels",
                        IconName::Tag,
                        "pull_requests.meta.labels",
                        "pull_requests.meta.no_labels",
                        labels,
                        labels_edit,
                        cx,
                    ))
                })
                .into_any_element(),
        )
    }

    /// The picker around its "Edit" trigger.
    fn picker_popover(&self, kind: PickerKind, cx: &mut Context<Self>) -> AnyElement {
        let view = cx.entity();
        let sheet = match kind {
            PickerKind::Labels => Sheet::Labels,
            PickerKind::Reviewers => Sheet::Reviewers,
        };
        let title = crate::tr!(match kind {
            PickerKind::Labels => "pull_requests.meta.labels_title",
            PickerKind::Reviewers => "pull_requests.meta.reviewers_title",
        })
        .into_owned();
        let id = match kind {
            PickerKind::Labels => "pr-labels-edit",
            PickerKind::Reviewers => "pr-reviewers-edit",
        };
        let popover = Popover::new(SharedString::from(format!("{id}-picker")))
            .open(self.sheet_open(&sheet))
            .on_open_change({
                let view = view.clone();
                move |open, _, cx| {
                    if !*open {
                        view.update(cx, |view, cx| view.close_picker(cx));
                    }
                }
            })
            .trigger(
                Button::new(id)
                    .ghost()
                    .xsmall()
                    .label(crate::tr!("pull_requests.meta.edit"))
                    .on_click(
                        cx.listener(move |this, _, window, cx| this.open_picker(kind, window, cx)),
                    ),
            );
        let popover = if self.compact(cx) {
            popover.bottom_sheet(title)
        } else {
            popover
        };
        popover
            .content(move |_, _, cx| Self::picker_body(&view, cx))
            .into_any_element()
    }

    fn picker_body(view: &Entity<Self>, cx: &mut App) -> AnyElement {
        let this = view.read(cx);
        let compact = this.compact(cx);
        let host_name = this.host_name(cx);
        let Some(picker) = this.picker() else {
            return div().into_any_element();
        };
        let kind = picker.kind;
        let filter = picker.filter.read(cx).value().to_lowercase();
        let row_height = px(material::selectable_row_height(compact));
        let body: AnyElement = match (&picker.rows, &picker.error) {
            (_, Some(error)) => div()
                .px_2()
                .py_2()
                .text_secondary(compact, cx)
                .text_color(cx.theme().warning)
                .child(
                    crate::tr!("pull_requests.meta.read_failed", reason = reason(error))
                        .into_owned(),
                )
                .into_any_element(),
            (None, None) => h_flex()
                .px_2()
                .h(row_height)
                .gap_2()
                .items_center()
                .text_secondary(compact, cx)
                .child(Spinner::new())
                .child(crate::tr!("pull_requests.meta.loading"))
                .into_any_element(),
            (Some(rows), None) if rows.is_empty() => material::empty_row(
                crate::tr!(match kind {
                    PickerKind::Labels => "pull_requests.meta.labels_empty",
                    PickerKind::Reviewers => "pull_requests.meta.reviewers_empty",
                }),
                cx,
            )
            .into_any_element(),
            (Some(rows), None) => {
                let shown: Vec<_> = rows
                    .iter()
                    .filter(|row| {
                        filter.is_empty()
                            || row.key.to_lowercase().contains(&filter)
                            || row
                                .secondary
                                .as_ref()
                                .is_some_and(|text| text.to_lowercase().contains(&filter))
                    })
                    .cloned()
                    .collect();
                if shown.is_empty() {
                    material::empty_row(
                        crate::tr!(match kind {
                            PickerKind::Labels => "pull_requests.meta.labels_no_match",
                            PickerKind::Reviewers => "pull_requests.meta.reviewers_no_match",
                        }),
                        cx,
                    )
                    .into_any_element()
                } else {
                    v_flex()
                        .id("pr-picker-rows")
                        // Ten rows, then the list scrolls inside the popover.
                        .max_h(px(280.))
                        .overflow_y_scroll()
                        .children(shown.into_iter().enumerate().map(|(index, row)| {
                            let checked =
                                picker.staged.get(&row.id).copied().unwrap_or(row.applied);
                            let lead = match kind {
                                PickerKind::Labels => {
                                    dot(label_color(row.color.as_deref(), cx)).into_any_element()
                                }
                                PickerKind::Reviewers => {
                                    this.avatar(&row.key, row.avatar.as_deref(), 20., cx)
                                }
                            };
                            let toggle_view = view.clone();
                            let (id, applied) = (row.id.clone(), row.applied);
                            Checkbox::new(("pr-picker-row", index))
                                .aria_label(row.key.clone())
                                .checked(checked)
                                .disabled(picker.applying)
                                .h(row_height)
                                .px_2()
                                .gap_2()
                                .items_center()
                                .child(lead)
                                .child(div().text_body(compact, cx).child(row.key.clone()))
                                .children(row.secondary.clone().map(|secondary| {
                                    div()
                                        .flex_1()
                                        .min_w_0()
                                        .overflow_hidden()
                                        .text_secondary(compact, cx)
                                        .child(crate::widgets::fade_text(secondary))
                                }))
                                .on_click(move |_, _, cx| {
                                    toggle_view.update(cx, |view, cx| {
                                        view.toggle_staged(id.clone(), applied, cx)
                                    })
                                })
                                .into_any_element()
                        }))
                        .into_any_element()
                }
            }
        };
        let changes = picker.staged.len();
        let applying = picker.applying;
        let (cancel_view, apply_view) = (view.clone(), view.clone());
        v_flex()
            // A desktop popover sizes to its content; the picker's list has no intrinsic width.
            .when(!compact, |body| body.w(px(320.)))
            .when(compact, |body| body.w_full())
            .p_1()
            .gap_1()
            .font_family(cx.theme().font_family.clone())
            .child(div().p_1().child(Input::new(&picker.filter)))
            .child(body)
            .when(!picker.complete, |body| {
                body.child(div().px_2().text_secondary(compact, cx).child(crate::tr!(
                    match kind {
                        PickerKind::Labels => "pull_requests.meta.labels_truncated",
                        PickerKind::Reviewers => "pull_requests.meta.reviewers_truncated",
                    },
                    host_name = host_name.clone()
                )))
            })
            .child(
                h_flex()
                    .justify_end()
                    .gap_2()
                    .px_2()
                    .py_2()
                    .border_t_1()
                    .border_color(cx.theme().border)
                    .child(
                        Button::new("pr-picker-cancel")
                            .ghost()
                            .xsmall()
                            .disabled(applying)
                            .label(crate::tr!("pull_requests.meta.cancel"))
                            .on_click(move |_, _, cx| {
                                cancel_view.update(cx, |view, cx| view.close_picker(cx))
                            }),
                    )
                    .child(
                        Button::new("pr-picker-apply")
                            .primary()
                            .xsmall()
                            .loading(applying)
                            .disabled(changes == 0)
                            .label(match changes {
                                0 => crate::tr!("pull_requests.meta.apply").into_owned(),
                                1 => crate::tr!("pull_requests.meta.apply_count_one").into_owned(),
                                _ => crate::tr!(
                                    "pull_requests.meta.apply_count",
                                    count = changes.to_string()
                                )
                                .into_owned(),
                            })
                            .on_click(move |_, window, cx| {
                                apply_view.update(cx, |view, cx| view.apply_picker(window, cx))
                            }),
                    ),
            )
            .into_any_element()
    }
}

fn label_rows(labels: PullRequestLabelCandidates) -> Vec<Row> {
    // Applied labels lead, as they are what the reader most often takes off.
    let mut rows: Vec<Row> = labels
        .labels
        .into_iter()
        .map(|label| Row {
            id: label.id,
            key: label.name,
            reviewer: None,
            applied: label.applied,
            color: label.color,
            avatar: None,
            secondary: label.description,
        })
        .collect();
    rows.sort_by_key(|row| !row.applied);
    rows
}

fn reviewer_rows(reviewers: PullRequestReviewerCandidates) -> Vec<Row> {
    reviewers
        .reviewers
        .into_iter()
        .map(|candidate| Row {
            id: candidate.reviewer.id.clone(),
            key: candidate.reviewer.login.clone(),
            applied: candidate.requested,
            color: None,
            avatar: candidate.avatar_url,
            secondary: candidate.name,
            reviewer: Some(candidate.reviewer),
        })
        .collect()
}
