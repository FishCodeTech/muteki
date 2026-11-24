//! The review: pending comments written on the Files diff, the bar that holds them, and the
//! sheet that submits them. The draft is the host's; this view only shows and edits it.

use gpui::{
    AnyElement, App, AppContext as _, Context, Entity, InteractiveElement as _, IntoElement,
    ParentElement as _, SharedString, StatefulInteractiveElement as _, Styled as _, Subscription,
    Task, Window, div, prelude::FluentBuilder as _, px,
};
use gpui_base::{h_flex, v_flex};
use tcode_core::{
    pull_request::{
        PullRequestReviewDraft, PullRequestReviewDraftComment, PullRequestReviewDraftEdit,
        PullRequestState,
    },
    session::ReviewSide,
};
use tcode_protocol::{
    PullRequestAction, PullRequestActionResult, PullRequestCapabilities, PullRequestRejection,
    PullRequestReviewVerdict,
};

use super::compose::{EditorSpec, LineAnchor, Sheet, Slot, Write};
use super::detail::PullRequestView;
use crate::{
    icon::{Icon, IconName},
    markdown::MarkdownView,
    material,
    overlay::{Notification, OverlayExt as _},
    sizing::Sizable as _,
    theme::ActiveTheme as _,
    typography::TextRole as _,
    widgets::{
        Popover,
        button::{Button, ButtonVariants as _},
        choice::{Choice, ChoiceGroup},
        input::{InputEvent, Textarea, TextareaState},
    },
};

/// The Review sheet's client-local state: the verdict until the sheet closes, the summary being
/// typed and its pending save.
#[derive(Default)]
pub(super) struct ReviewSheet {
    verdict: Option<PullRequestReviewVerdict>,
    summary: Option<(Entity<TextareaState>, Subscription)>,
    save: Option<Task<()>>,
    /// The head a submission was refused at, until the draft moves to it.
    stale: Option<String>,
}

fn short(revision: &str) -> String {
    revision.chars().take(7).collect()
}

fn lines_label(start: u32, end: u32, side: ReviewSide) -> String {
    let lines = if start == end {
        crate::tr!("pull_requests.conversation.line", line = end.to_string())
    } else {
        crate::tr!(
            "pull_requests.conversation.lines",
            start = start.to_string(),
            end = end.to_string()
        )
    }
    .into_owned();
    match side {
        ReviewSide::Old => format!(
            "{lines} {}",
            crate::tr!("pull_requests.conversation.base_side")
        ),
        ReviewSide::New => lines,
    }
}

impl PullRequestView {
    fn files_head(&self) -> Option<String> {
        self.page()
            .and_then(|page| page.files.data.as_ref())
            .map(|files| files.head.clone())
    }

    fn verdicts(&self) -> Vec<PullRequestReviewVerdict> {
        self.page()
            .and_then(|page| page.conversation.data.as_ref())
            .map(|conversation| conversation.permissions.verdicts.clone())
            .unwrap_or_default()
    }

    fn merged(&self, cx: &App) -> bool {
        self.link(cx)
            .and_then(|link| link.snapshot)
            .is_some_and(|snapshot| snapshot.state == PullRequestState::Merged)
    }

    /// Whether this account may start a review here, from this device.
    pub(super) fn can_review(&self, cx: &App) -> bool {
        !self.verdicts().is_empty() && !self.read_only(cx) && !self.merged(cx)
    }

    /// The draft's comments are anchored at a head the pull request has moved past.
    fn stale_head(&self, draft: &PullRequestReviewDraft) -> Option<String> {
        if draft.comments.is_empty() || draft.head.is_empty() {
            return None;
        }
        let current = self
            .writes()
            .and_then(|writes| writes.review.stale.clone())
            .or_else(|| self.files_head())?;
        (current != draft.head).then_some(current)
    }

    /// Why a new pending comment cannot be added now: a draft at an older head takes none until
    /// it is moved, since the host would refuse it.
    fn line_comment_blocked(&self, cx: &App) -> Option<SharedString> {
        self.draft(cx)
            .and_then(|draft| self.stale_head(&draft))
            .map(|_| crate::tr!("pull_requests.review.move_first").into())
    }

    /// What the selection menu offers on the Files diff.
    pub(super) fn line_comment_menu(&self, cx: &App) -> Option<(bool, SharedString)> {
        self.can_review(cx).then(|| {
            let open = self
                .writes()
                .is_some_and(|writes| writes.editors.contains_key(&Slot::Line));
            (
                !open && self.line_comment_blocked(cx).is_none(),
                crate::tr!("pull_requests.review.add_review_comment").into(),
            )
        })
    }

    /// Opens the line editor on the Files selection, which lies on one side of one file.
    pub(super) fn start_line_comment(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(files) = self.page().and_then(|page| page.files.data.clone()) else {
            return;
        };
        let Some(selection) = self
            .page()
            .and_then(|page| page.files_view.list.as_ref())
            .and_then(|list| list.selection.clone())
        else {
            return;
        };
        let anchor = LineAnchor {
            head: files.head.clone(),
            revision: match selection.side {
                ReviewSide::Old => files.base.clone(),
                ReviewSide::New => files.head.clone(),
            },
            path: selection.file.clone(),
            side: selection.side,
            start_line: selection.line_start.min(selection.line_end),
            end_line: selection.line_start.max(selection.line_end),
        };
        self.close_editor(&Slot::Line, cx);
        self.open_editor(
            Slot::Line,
            crate::tr!("pull_requests.review.line_placeholder").into(),
            String::new(),
            None,
            window,
            cx,
        );
        if let Some(editor) = self
            .writes_mut()
            .and_then(|writes| writes.editors.get_mut(&Slot::Line))
        {
            editor.anchor = Some(anchor);
        }
        if self.compact(cx) {
            self.open_sheet(Some(Sheet::Editor(Slot::Line)), cx);
        }
        if let Some(list) = self.page().and_then(|page| page.files_view.list.as_ref()) {
            list.remeasure();
        }
    }

    /// What Files draws under a code row: pending comments ending there, and the selection's row
    /// or its line editor.
    pub(super) fn review_row_extras(
        &self,
        path: &str,
        old: Option<u32>,
        new: Option<u32>,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let ends_here = |side: ReviewSide, line: u32| match side {
            ReviewSide::Old => old == Some(line),
            ReviewSide::New => new == Some(line),
        };
        let mut rows = Vec::new();
        if let Some(draft) = self.draft(cx)
            && self.stale_head(&draft).is_none()
            && self.files_head().as_deref() == Some(draft.head.as_str())
        {
            for comment in draft
                .comments
                .iter()
                .filter(|comment| comment.placed && comment.path == path)
                .filter(|comment| ends_here(comment.side, comment.end_line))
            {
                rows.push(self.pending_card(comment, cx));
            }
        }
        let editor = self
            .writes()
            .and_then(|writes| writes.editors.get(&Slot::Line))
            .and_then(|editor| editor.anchor.clone());
        match editor {
            Some(anchor) if anchor.path == path && ends_here(anchor.side, anchor.end_line) => {
                let context = if anchor.start_line == anchor.end_line {
                    format!(
                        "{} · {}",
                        crate::tr!(
                            "pull_requests.conversation.line",
                            line = anchor.end_line.to_string()
                        ),
                        anchor.path
                    )
                } else {
                    crate::tr!(
                        "pull_requests.review.line_context",
                        start = anchor.start_line.to_string(),
                        end = anchor.end_line.to_string(),
                        path = anchor.path.clone()
                    )
                    .into_owned()
                };
                let spec = EditorSpec {
                    context: Some(context),
                    submit: crate::tr!("pull_requests.review.add_to_review").into(),
                    aria: crate::tr!("pull_requests.review.line_placeholder").into(),
                    cancel: true,
                };
                let element = if self.compact(cx) {
                    let title: SharedString = crate::tr!(
                        "pull_requests.review.line_sheet",
                        path = anchor.path.clone()
                    )
                    .into();
                    self.editor_sheet(
                        Slot::Line,
                        crate::tr!("pull_requests.review.add_review_comment").into(),
                        title,
                        spec,
                        |this, window, cx| this.start_line_comment(window, cx),
                        cx,
                    )
                } else {
                    self.editor_element(&cx.entity(), &Slot::Line, spec, cx)
                        .unwrap_or_else(|| div().into_any_element())
                };
                rows.push(
                    material::card(cx)
                        .min_w_full()
                        .child(element)
                        .into_any_element(),
                );
            }
            Some(_) => {}
            None => {
                let selected = self
                    .page()
                    .and_then(|page| page.files_view.list.as_ref())
                    .and_then(|list| list.selection.as_ref())
                    .filter(|selection| {
                        selection.file == path && ends_here(selection.side, selection.line_end)
                    })
                    .is_some();
                if selected && self.can_review(cx) {
                    rows.push(crate::diff::list::selection_row(
                        "pr-add-review-comment",
                        crate::tr!("pull_requests.review.add_review_comment").into(),
                        self.line_comment_blocked(cx),
                        cx.listener(|this, _, window, cx| this.start_line_comment(window, cx)),
                        cx,
                    ));
                }
            }
        }
        rows
    }

    fn pending_card(
        &self,
        comment: &PullRequestReviewDraftComment,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let compact = self.compact(cx);
        let id = comment.id;
        let slot = Slot::Pending(id);
        let editing = self
            .writes()
            .is_some_and(|writes| writes.editors.contains_key(&slot));
        let body = if editing {
            self.editor_element(
                &cx.entity(),
                &slot,
                EditorSpec {
                    context: None,
                    submit: crate::tr!("pull_requests.compose.save").into(),
                    aria: crate::tr!("pull_requests.review.edit_pending").into(),
                    cancel: true,
                },
                cx,
            )
        } else {
            let state = self.markdown(&format!("pending-{id}"), &comment.body, cx);
            Some(
                div()
                    .text_body(compact, cx)
                    .child(MarkdownView::new(&state).compact_headings(true))
                    .into_any_element(),
            )
        };
        let text = comment.body.clone();
        let removed = comment.clone();
        let label = crate::tr!(
            "pull_requests.review.pending_label",
            path = comment.path.clone(),
            line = comment.end_line.to_string()
        )
        .into_owned();
        material::card(cx)
            .id(SharedString::from(format!("pr-pending-{id}")))
            .role(gpui::Role::Group)
            .aria_label(label)
            .min_w_full()
            .my_1()
            .relative()
            .font_family(cx.theme().font_family.clone())
            .child(
                // The pending marker's edge rail; no owner draws an accent edge on a card.
                div()
                    .absolute()
                    .left(px(0.))
                    .top(px(6.))
                    .bottom(px(6.))
                    .w(px(2.))
                    .rounded_full()
                    .bg(cx.theme().info),
            )
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .text_secondary(compact, cx)
                    .child(material::semantic_chip(
                        crate::tr!("pull_requests.review.pending").into_owned(),
                        cx.theme().info.opacity(0.1),
                        cx.theme().info,
                        cx,
                    ))
                    .child(lines_label(
                        comment.start_line,
                        comment.end_line,
                        comment.side,
                    ))
                    .child(div().flex_1())
                    .when(!editing, |head| {
                        head.child(
                            Button::new(SharedString::from(format!("pr-pending-edit-{id}")))
                                .ghost()
                                .xsmall()
                                .compact()
                                .icon(IconName::Pencil)
                                .tooltip(crate::tr!("pull_requests.review.edit_pending"))
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.open_editor(
                                        Slot::Pending(id),
                                        crate::tr!("pull_requests.review.line_placeholder").into(),
                                        text.clone(),
                                        None,
                                        window,
                                        cx,
                                    )
                                })),
                        )
                    })
                    .child(
                        Button::new(SharedString::from(format!("pr-pending-remove-{id}")))
                            .ghost()
                            .xsmall()
                            .compact()
                            .icon(IconName::Trash)
                            .tooltip(crate::tr!("pull_requests.review.remove_pending"))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.remove_pending(&removed, window, cx)
                            })),
                    ),
            )
            .children(body)
            .into_any_element()
    }

    /// Removes a pending comment without asking; the toast can put a placed one back. An
    /// unplaced one cannot be: adding it again would anchor it at lines that changed.
    fn remove_pending(
        &mut self,
        comment: &PullRequestReviewDraftComment,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let head = self.draft(cx).map(|draft| draft.head).unwrap_or_default();
        let restore = comment
            .placed
            .then(|| PullRequestReviewDraftEdit::AddComment {
                head,
                revision: comment.revision.clone(),
                path: comment.path.clone(),
                side: comment.side,
                start_line: comment.start_line,
                end_line: comment.end_line,
                body: comment.body.clone(),
            });
        let view = cx.entity();
        self.edit_draft(
            PullRequestReviewDraftEdit::RemoveComment { id: comment.id },
            window,
            cx,
            move |_, result, window, cx| {
                if result.is_err() {
                    return;
                }
                let note = Notification::info(
                    crate::tr!("pull_requests.review.removed_toast").into_owned(),
                );
                let Some(restore) = restore else {
                    window.push_notification(note, cx);
                    return;
                };
                let view = view.clone();
                window.push_notification(
                    note.action(move |_, _, _| {
                        let view = view.clone();
                        let restore = restore.clone();
                        Button::new("pr-pending-undo")
                            .ghost()
                            .xsmall()
                            .label(crate::tr!("pull_requests.review.undo"))
                            .on_click(move |_, window, cx| {
                                view.update(cx, |view, cx| {
                                    view.edit_draft(restore.clone(), window, cx, |_, _, _, _| {})
                                })
                            })
                    }),
                    cx,
                );
            },
        );
    }

    /// Pending comments the diff does not draw: on files or lines not shown, and those whose lines
    /// changed when the draft moved to a new head.
    pub(super) fn off_diff_pending(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let Some(draft) = self.draft(cx) else {
            return Vec::new();
        };
        let drawn = self.stale_head(&draft).is_none()
            && self.files_head().as_deref() == Some(draft.head.as_str());
        let muted = cx.theme().muted_foreground;
        draft
            .comments
            .iter()
            .filter(|comment| !comment.placed || !drawn || !self.pending_drawn(comment))
            .map(|comment| {
                let id = comment.id;
                let removed = comment.clone();
                material::card_header_row(cx)
                    .id(SharedString::from(format!("pr-off-diff-pending-{id}")))
                    .text_secondary(false, cx)
                    .text_color(cx.theme().foreground)
                    .child(material::semantic_chip(
                        crate::tr!("pull_requests.review.pending").into_owned(),
                        cx.theme().info.opacity(0.1),
                        cx.theme().info,
                        cx,
                    ))
                    .child(
                        div()
                            .flex_none()
                            .max_w(px(220.))
                            .overflow_hidden()
                            .font_family(cx.theme().mono_font_family.clone())
                            .child(crate::widgets::fade_text(format!(
                                "{}:{}",
                                comment.path, comment.end_line
                            ))),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .overflow_hidden()
                            .text_color(muted)
                            .child(crate::widgets::fade_text(
                                comment.body.lines().next().unwrap_or_default().to_owned(),
                            )),
                    )
                    .when(!comment.placed, |row| {
                        row.child(
                            div()
                                .flex_none()
                                .text_color(cx.theme().warning)
                                .child(crate::tr!("pull_requests.review.line_changed")),
                        )
                        .child(
                            Button::new(SharedString::from(format!("pr-pending-copy-{id}")))
                                .ghost()
                                .xsmall()
                                .compact()
                                .icon(IconName::Copy)
                                .tooltip(crate::tr!("pull_requests.conversation.copy_text"))
                                .on_click({
                                    let text = comment.body.clone();
                                    move |_, _, cx| {
                                        cx.write_to_clipboard(gpui::ClipboardItem::new_string(
                                            text.clone(),
                                        ))
                                    }
                                }),
                        )
                    })
                    .child(
                        Button::new(SharedString::from(format!("pr-pending-off-remove-{id}")))
                            .ghost()
                            .xsmall()
                            .compact()
                            .icon(IconName::Trash)
                            .tooltip(crate::tr!("pull_requests.review.remove_pending"))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.remove_pending(&removed, window, cx)
                            })),
                    )
                    .into_any_element()
            })
            .collect()
    }

    /// Whether the diff draws the comment's last line, as it would draw a thread there.
    fn pending_drawn(&self, comment: &PullRequestReviewDraftComment) -> bool {
        let Some(page) = self.page() else {
            return false;
        };
        let Some(index) = self
            .files_of()
            .and_then(|files| files.iter().position(|file| file.path == comment.path))
        else {
            return false;
        };
        page.files_view
            .list
            .as_ref()
            .filter(|list| list.layout(index) == crate::diff::list::FileLayout::Rows)
            .and_then(|list| list.files.get(index))
            .is_some_and(|file| {
                file.all_rows.iter().any(|row| match comment.side {
                    ReviewSide::Old => row.old == Some(comment.end_line),
                    ReviewSide::New => row.new == Some(comment.end_line),
                })
            })
    }

    /// The Files toolbar's entry to a review while none is pending.
    pub(super) fn review_entry(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if self.verdicts().is_empty() || self.draft(cx).is_some() || self.read_only(cx) {
            return None;
        }
        let merged = self.merged(cx);
        // A phone reaches the sheet from the toolbar's overflow menu, and a bottom sheet needs no
        // trigger.
        let trigger = (!self.compact(cx)).then(|| {
            Button::new("pr-review-entry")
                .ghost()
                .small()
                .icon(IconName::MessageSquare)
                .label(crate::tr!("pull_requests.review.entry"))
                .disabled(merged)
                .when(merged, |button| {
                    button.tooltip(crate::tr!("pull_requests.review.merged_disabled"))
                })
                .on_click(cx.listener(|this, _, window, cx| this.open_review(window, cx)))
        });
        Some(self.review_popover(trigger, cx))
    }

    /// The bar under the Files toolbar while a review is pending.
    pub(super) fn review_bar(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let draft = self.draft(cx)?;
        let compact = self.compact(cx);
        let count = draft.comments.len();
        let summary = if count == 1 {
            crate::tr!("pull_requests.review.bar_one").into_owned()
        } else if count == 0 {
            crate::tr!("pull_requests.review.bar_summary_only").into_owned()
        } else {
            crate::tr!("pull_requests.review.bar", count = count.to_string()).into_owned()
        };
        let stale = self.stale_head(&draft).is_some();
        let finish = Button::new("pr-review-finish")
            .primary()
            .xsmall()
            .label(crate::tr!("pull_requests.review.finish"))
            .on_click(cx.listener(|this, _, window, cx| this.open_review(window, cx)));
        Some(
            material::bar_divider_below(
                material::bar("pr-review-bar", material::Bar::Toolbar, compact, cx),
                cx,
            )
            .aria_label(
                crate::tr!("pull_requests.review.bar_label", count = count.to_string())
                    .into_owned(),
            )
            .items_center()
            .bg(cx.theme().muted)
            .text_secondary(compact, cx)
            .text_color(cx.theme().foreground)
            .child(Icon::new(IconName::MessageSquare).text_color(cx.theme().muted_foreground))
            .child(
                div()
                    .min_w_0()
                    .overflow_hidden()
                    .child(crate::widgets::fade_text(summary)),
            )
            .when(stale, |bar| {
                bar.child(
                    h_flex()
                        .flex_none()
                        .gap_1()
                        .items_center()
                        .text_color(cx.theme().warning)
                        .child(Icon::new(IconName::TriangleAlert))
                        .child(crate::tr!("pull_requests.review.head_changed")),
                )
            })
            .child(div().flex_1())
            .when(!compact, |bar| {
                bar.child(
                    Button::new("pr-review-discard")
                        .ghost()
                        .xsmall()
                        .label(crate::tr!("pull_requests.review.discard"))
                        .on_click(
                            cx.listener(|this, _, window, cx| this.confirm_discard(window, cx)),
                        ),
                )
            })
            .child(self.review_popover(Some(finish), cx))
            .into_any_element(),
        )
    }

    /// The Review sheet around its trigger: anchored on a desktop, a bottom sheet on a phone.
    fn review_popover(&self, trigger: Option<Button>, cx: &mut Context<Self>) -> AnyElement {
        let view = cx.entity();
        let compact = self.compact(cx);
        let number = self
            .current
            .as_ref()
            .map(|(_, key)| key.number.to_string())
            .unwrap_or_default();
        let popover = Popover::new("pr-review-sheet")
            .anchor(gpui::Anchor::TopRight)
            .open(self.sheet_open(&Sheet::Review))
            .on_open_change({
                let view = view.clone();
                move |open, window, cx| {
                    view.update(cx, |view, cx| {
                        if *open {
                            view.open_review(window, cx);
                        } else {
                            view.open_sheet(None, cx);
                            if let Some(writes) = view.writes_mut() {
                                writes.review.verdict = None;
                            }
                        }
                    })
                }
            });
        let popover = match trigger {
            Some(trigger) => popover.trigger(trigger),
            None => popover,
        };
        let popover = if compact {
            popover.bottom_sheet(
                crate::tr!("pull_requests.review.sheet_title", number = number).into_owned(),
            )
        } else {
            popover
        };
        popover
            .content(move |_, _, cx| Self::review_sheet(&view, cx))
            .into_any_element()
    }

    /// Opens the Review sheet, its summary editor holding the draft's summary as it is now.
    pub(super) fn open_review(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.sheet_open(&Sheet::Review) {
            return;
        }
        self.open_sheet(Some(Sheet::Review), cx);
        let body = self.draft(cx).map(|draft| draft.body).unwrap_or_default();
        let input = cx.new(|cx| {
            TextareaState::new(window, cx)
                .auto_grow(4, 10)
                .placeholder(crate::tr!("pull_requests.review.summary_placeholder"))
        });
        if !body.is_empty() {
            input.update(cx, |input, cx| input.set_value(body, window, cx));
        }
        let subscription = cx.subscribe_in(
            &input,
            window,
            |this, input, event: &InputEvent, window, cx| match event {
                InputEvent::Change => this.save_summary_later(input.clone(), window, cx),
                InputEvent::PressEnter {
                    secondary: true, ..
                } => this.submit_review(window, cx),
                _ => {}
            },
        );
        if let Some(writes) = self.writes_mut() {
            writes.review.summary = Some((input, subscription));
        }
    }

    /// The summary is the host's as it is typed, half a second after the last key.
    fn save_summary_later(
        &mut self,
        input: Entity<TextareaState>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let task = cx.spawn_in(window, async move |this, cx| {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(500))
                .await;
            let _ = this.update_in(cx, |this, window, cx| {
                let body = input.read(cx).value().to_string();
                if let Some(writes) = this.writes_mut() {
                    writes.review.save = None;
                }
                this.edit_draft(
                    PullRequestReviewDraftEdit::SetBody { body },
                    window,
                    cx,
                    |_, _, _, _| {},
                );
            });
        });
        if let Some(writes) = self.writes_mut() {
            writes.review.save = Some(task);
        }
    }

    fn summary_text(&self, cx: &App) -> Option<String> {
        self.writes()?
            .review
            .summary
            .as_ref()
            .map(|(input, _)| input.read(cx).value().to_string())
    }

    fn verdict(&self) -> PullRequestReviewVerdict {
        self.writes()
            .and_then(|writes| writes.review.verdict)
            .unwrap_or(PullRequestReviewVerdict::Comment)
    }

    fn submittable(&self, cx: &App) -> bool {
        let draft = self.draft(cx);
        let verdict = self.verdict();
        let summary = self
            .summary_text(cx)
            .or_else(|| draft.as_ref().map(|draft| draft.body.clone()))
            .unwrap_or_default();
        let comments = draft.as_ref().map_or(0, |draft| draft.comments.len());
        let unplaced = draft
            .as_ref()
            .is_some_and(|draft| draft.comments.iter().any(|comment| !comment.placed));
        let stale = draft
            .as_ref()
            .is_some_and(|draft| self.stale_head(draft).is_some());
        let uncertain = draft.as_ref().is_some_and(|draft| draft.uncertain);
        self.verdicts().contains(&verdict)
            && !unplaced
            && !stale
            && !uncertain
            && !self.busy("review")
            && self.files_head().is_some()
            && (verdict == PullRequestReviewVerdict::Approve
                || !summary.trim().is_empty()
                || comments > 0)
    }

    /// Sends the review: the summary as typed is the host's first, then the one submission.
    pub(super) fn submit_review(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.submittable(cx) {
            return;
        }
        let Some(head) = self.files_head() else {
            return;
        };
        let verdict = self.verdict();
        let summary = self.summary_text(cx);
        let held = self.draft(cx).map(|draft| draft.body).unwrap_or_default();
        if let Some(writes) = self.writes_mut() {
            writes.review.save = None;
            writes.busy.insert("review".into());
        }
        cx.notify();
        let send = move |this: &mut Self, window: &mut Window, cx: &mut Context<Self>| {
            if let Some(writes) = this.writes_mut() {
                writes.busy.remove("review");
            }
            this.send_write(
                PullRequestAction::SubmitReview { verdict, head },
                Write::Review(verdict),
                "review".into(),
                window,
                cx,
                |this, result, _, cx| {
                    match result {
                        PullRequestActionResult::Applied => {
                            if let Some(writes) = this.writes_mut() {
                                writes.sheet = None;
                                writes.review = ReviewSheet::default();
                            }
                        }
                        PullRequestActionResult::Rejected(PullRequestRejection::StaleHead {
                            head,
                        }) => {
                            if let Some(page) = this.page_mut() {
                                page.writes.review.stale = Some(head.clone());
                                // Files reads the new head, so the reader sees what changed.
                                page.files.expires_at = 0;
                            }
                        }
                        _ => {}
                    }
                    cx.notify();
                },
            );
        };
        match summary.filter(|summary| *summary != held) {
            Some(body) => self.edit_draft(
                PullRequestReviewDraftEdit::SetBody { body },
                window,
                cx,
                move |this, result, window, cx| match result {
                    Ok(()) => send(this, window, cx),
                    Err(_) => {
                        if let Some(writes) = this.writes_mut() {
                            writes.busy.remove("review");
                        }
                    }
                },
            ),
            None => send(self, window, cx),
        }
    }

    fn move_to_head(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(writes) = self.writes_mut() {
            writes.busy.insert("move".into());
        }
        self.edit_draft(
            PullRequestReviewDraftEdit::MoveToHead,
            window,
            cx,
            |this, _, _, cx| {
                if let Some(page) = this.page_mut() {
                    page.writes.busy.remove("move");
                    page.writes.review.stale = None;
                    page.files.expires_at = 0;
                }
                cx.notify();
            },
        );
    }

    fn confirm_discard(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let count = self
            .draft(cx)
            .map_or(0, |draft| draft.comments.len())
            .to_string();
        let host_name = self.host_name(cx);
        let view = cx.entity();
        window.open_alert_dialog(cx, move |alert, _, _| {
            let view = view.clone();
            alert
                .title(crate::tr!("pull_requests.review.discard_title"))
                .description(crate::tr!(
                    "pull_requests.review.discard_desc",
                    count = count.clone(),
                    host_name = host_name.clone()
                ))
                .button_props(
                    crate::overlay::DialogButtons::default()
                        .ok_text(crate::tr!("pull_requests.review.discard_confirm"))
                        .ok_variant(crate::widgets::button::ButtonVariant::Danger)
                        .cancel_text(crate::tr!("pull_requests.compose.cancel"))
                        .show_cancel(true),
                )
                .on_ok(move |_, window, cx| {
                    view.update(cx, |view, cx| {
                        if let Some(writes) = view.writes_mut() {
                            writes.sheet = None;
                            writes.review = ReviewSheet::default();
                        }
                        view.edit_draft(
                            PullRequestReviewDraftEdit::Discard,
                            window,
                            cx,
                            |_, _, _, _| {},
                        );
                    });
                    true
                })
        });
    }

    fn review_sheet(view: &Entity<Self>, cx: &mut App) -> AnyElement {
        let this = view.read(cx);
        let compact = this.compact(cx);
        let number = this
            .current
            .as_ref()
            .map(|(_, key)| key.number.to_string())
            .unwrap_or_default();
        let draft = this.draft(cx);
        let verdict = this.verdict();
        let verdicts = this.verdicts();
        let submittable = this.submittable(cx);
        let busy = this.busy("review");
        let moving = this.busy("move");
        let comments = draft.as_ref().map_or(0, |draft| draft.comments.len());
        let unplaced = draft.as_ref().map_or(0, |draft| {
            draft
                .comments
                .iter()
                .filter(|comment| !comment.placed)
                .count()
        });
        let stale = draft
            .as_ref()
            .and_then(|draft| Some((draft.head.clone(), this.stale_head(draft)?)));
        let uncertain = draft.as_ref().is_some_and(|draft| draft.uncertain);
        let url = this.url(cx);
        let summary = this
            .writes()
            .and_then(|writes| writes.review.summary.as_ref())
            .map(|(input, _)| input.clone());
        let caption = if !verdicts.contains(&PullRequestReviewVerdict::Approve) {
            "pull_requests.review.author_only_comment"
        } else {
            match verdict {
                PullRequestReviewVerdict::Comment => "pull_requests.review.caption_comment",
                PullRequestReviewVerdict::Approve => "pull_requests.review.caption_approve",
                PullRequestReviewVerdict::RequestChanges => {
                    "pull_requests.review.caption_request_changes"
                }
            }
        };
        let segments = offered_verdicts(this.capabilities())
            .map(|segment| {
                let (id, label) = match segment {
                    PullRequestReviewVerdict::Comment => {
                        ("comment", "pull_requests.review.verdict_comment")
                    }
                    PullRequestReviewVerdict::Approve => {
                        ("approve", "pull_requests.review.verdict_approve")
                    }
                    PullRequestReviewVerdict::RequestChanges => (
                        "request-changes",
                        "pull_requests.review.verdict_request_changes",
                    ),
                };
                let view = view.clone();
                Choice::new(
                    SharedString::from(format!("pr-verdict-{id}")),
                    crate::tr!(label).into_owned(),
                )
                .selected(verdict == segment)
                .disabled(!verdicts.contains(&segment))
                .on_select(move |_, cx| {
                    view.update(cx, |view, cx| {
                        if let Some(writes) = view.writes_mut() {
                            writes.review.verdict = Some(segment);
                        }
                        cx.notify();
                    })
                })
            })
            .collect::<Vec<_>>();
        let notice = |title: Option<String>, body: String, actions: Vec<AnyElement>, cx: &App| {
            v_flex()
                .gap_1()
                .child(crate::diff::list::render_notice(
                    match title {
                        Some(title) => format!("{title} {body}"),
                        None => body,
                    },
                    cx,
                ))
                .when(!actions.is_empty(), |notice| {
                    notice.child(h_flex().gap_2().children(actions))
                })
                .into_any_element()
        };
        let host_name = view.read(cx).host_name(cx);
        let stale_notice = stale.map(|(old, new)| {
            let move_view = view.clone();
            let mut actions = vec![
                Button::new("pr-review-move")
                    .outline()
                    .xsmall()
                    .loading(moving)
                    .label(crate::tr!("pull_requests.review.move_to_head"))
                    .on_click(move |_, window, cx| {
                        move_view.update(cx, |view, cx| view.move_to_head(window, cx))
                    })
                    .into_any_element(),
            ];
            if let Some(url) = url.clone() {
                actions.push(
                    Button::new("pr-review-open")
                        .ghost()
                        .xsmall()
                        .label(crate::tr!(
                            "pull_requests.open_on_host",
                            host_name = &host_name
                        ))
                        .on_click(move |_, _, cx| cx.open_url(&url))
                        .into_any_element(),
                );
            }
            notice(
                Some(
                    crate::tr!("pull_requests.review.stale_title", number = number.clone())
                        .into_owned(),
                ),
                crate::tr!(
                    "pull_requests.review.stale_body",
                    old = short(&old),
                    new = short(&new)
                )
                .into_owned(),
                actions,
                cx,
            )
        });
        let uncertain_notice = uncertain.then(|| {
            let refresh_view = view.clone();
            notice(
                None,
                crate::tr!("pull_requests.review.uncertain_notice").into_owned(),
                vec![
                    Button::new("pr-review-refresh")
                        .ghost()
                        .xsmall()
                        .label(crate::tr!("pull_requests.review.refresh"))
                        .on_click(move |_, window, cx| {
                            refresh_view.update(cx, |view, cx| view.refresh(window, cx))
                        })
                        .into_any_element(),
                ],
                cx,
            )
        });
        let show_view = view.clone();
        let submit_view = view.clone();
        let discard_view = view.clone();
        v_flex()
            .id("pr-review-sheet-body")
            // Popover sizes to its content; the anchored sheet's width is its own.
            .when(!compact, |sheet| sheet.w(px(440.)))
            .when(compact, |sheet| sheet.w_full())
            .p_3()
            .gap_3()
            .font_family(cx.theme().font_family.clone())
            .when(!compact, |sheet| {
                sheet.child(
                    div().text_title(cx).child(
                        crate::tr!("pull_requests.review.sheet_title", number = number.clone())
                            .into_owned(),
                    ),
                )
            })
            .children(stale_notice)
            .children(uncertain_notice)
            .when(comments > 0, |sheet| {
                sheet.child(
                    h_flex()
                        .gap_2()
                        .items_center()
                        .text_secondary(compact, cx)
                        .child(if comments == 1 {
                            crate::tr!("pull_requests.review.pending_count_one").into_owned()
                        } else {
                            crate::tr!(
                                "pull_requests.review.pending_count",
                                count = comments.to_string()
                            )
                            .into_owned()
                        })
                        .child(
                            Button::new("pr-review-show")
                                .ghost()
                                .xsmall()
                                .label(crate::tr!("pull_requests.review.show"))
                                .on_click(move |_, _, cx| {
                                    show_view.update(cx, |view, cx| {
                                        if let Some(page) = view.page_mut() {
                                            page.tab = Some(super::detail::Tab::Files);
                                            page.files_view.off_diff_open = true;
                                            page.writes.sheet = None;
                                        }
                                        cx.notify();
                                    })
                                }),
                        ),
                )
            })
            .children(summary.map(|input| {
                Textarea::new(&input).aria_label(crate::tr!("pull_requests.review.summary_label"))
            }))
            .child(
                ChoiceGroup::new("pr-verdict")
                    .compact(compact)
                    .choices(segments),
            )
            .child(
                div()
                    .text_secondary(compact, cx)
                    .child(crate::tr!(caption, host_name = host_name.clone())),
            )
            .when(unplaced > 0, |sheet| {
                sheet.child(
                    div()
                        .text_secondary(compact, cx)
                        .text_color(cx.theme().warning)
                        .child(if unplaced == 1 {
                            crate::tr!("pull_requests.review.unplaced_one").into_owned()
                        } else {
                            crate::tr!(
                                "pull_requests.review.unplaced",
                                count = unplaced.to_string()
                            )
                            .into_owned()
                        }),
                )
            })
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .when(compact && draft.is_some(), |footer| {
                        footer.child(
                            Button::new("pr-review-sheet-discard")
                                .ghost()
                                .xsmall()
                                .label(crate::tr!("pull_requests.review.discard"))
                                .on_click(move |_, window, cx| {
                                    discard_view
                                        .update(cx, |view, cx| view.confirm_discard(window, cx))
                                }),
                        )
                    })
                    .child(div().flex_1())
                    .child(
                        Button::new("pr-review-submit")
                            .primary()
                            .small()
                            .loading(busy)
                            .disabled(!submittable)
                            .label(if busy {
                                crate::tr!("pull_requests.review.submitting")
                            } else {
                                crate::tr!("pull_requests.review.submit")
                            })
                            .on_click(move |_, window, cx| {
                                submit_view.update(cx, |view, cx| view.submit_review(window, cx))
                            }),
                    ),
            )
            .into_any_element()
    }

    /// The conversation's line above the composer while a review is pending.
    pub(super) fn pending_review_line(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let draft = self.draft(cx)?;
        let count = draft.comments.len();
        Some(
            h_flex()
                .py_2()
                .gap_2()
                .items_center()
                .text_secondary(self.compact(cx), cx)
                .child(Icon::new(IconName::MessageSquare))
                .child(
                    crate::tr!(
                        "pull_requests.review.pending_line",
                        count = count.to_string()
                    )
                    .into_owned(),
                )
                .child(
                    Button::new("pr-review-line-finish")
                        .ghost()
                        .xsmall()
                        .label(crate::tr!("pull_requests.review.finish"))
                        .on_click(cx.listener(|this, _, window, cx| {
                            if let Some(page) = this.page_mut() {
                                page.tab = Some(super::detail::Tab::Files);
                            }
                            this.open_review(window, cx);
                        })),
                )
                .into_any_element(),
        )
    }
}

/// The verdicts the host offers, in the picker's order. One the account may not give is drawn
/// disabled; one the host lacks is not drawn, so it never reads as a permission denied.
fn offered_verdicts(
    capabilities: PullRequestCapabilities,
) -> impl Iterator<Item = PullRequestReviewVerdict> {
    [
        PullRequestReviewVerdict::Comment,
        PullRequestReviewVerdict::Approve,
        PullRequestReviewVerdict::RequestChanges,
    ]
    .into_iter()
    .filter(move |verdict| {
        *verdict != PullRequestReviewVerdict::RequestChanges || capabilities.request_changes
    })
}
