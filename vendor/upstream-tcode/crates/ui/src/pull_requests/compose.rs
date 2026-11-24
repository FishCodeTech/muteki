//! Writing to the pull request: the one editor every body is written in, the write each sends,
//! and what its answer looks like. The host makes every write once; nothing here retries one.

use std::collections::{HashMap, HashSet};

use gpui::{
    AnyElement, App, AppContext as _, Context, Entity, InteractiveElement as _, IntoElement,
    ParentElement as _, SharedString, Styled as _, Subscription, Task, Window, div,
    prelude::FluentBuilder as _, px,
};
use gpui_base::{h_flex, v_flex};
use tcode_core::pull_request::{PullRequestReviewDraft, PullRequestReviewDraftEdit};
use tcode_protocol::{
    Command, CommandResponse, ProtocolError, PullRequestAction, PullRequestActionResult,
    PullRequestReactionContent, PullRequestRejection, PullRequestReviewVerdict,
};

use super::detail::PullRequestView;
use crate::{
    material,
    overlay::{Notification, OverlayExt as _},
    sizing::Sizable as _,
    theme::ActiveTheme as _,
    typography::TextRole as _,
    widgets::{
        button::{Button, ButtonVariants as _},
        input::{InputEvent, InputState, Textarea, TextareaState},
    },
};

/// Where an editor is open. Its unsent text stays with the pull request until it is sent or
/// cancelled, across tab switches and closing the panel.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(super) enum Slot {
    Composer,
    Reply(String),
    Edit(String),
    Description,
    /// A new pending comment on the selected lines.
    Line,
    Pending(u64),
}

/// The lines a new pending comment is on, as the Files selection named them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct LineAnchor {
    pub(super) head: String,
    pub(super) revision: String,
    pub(super) path: String,
    pub(super) side: tcode_core::session::ReviewSide,
    pub(super) start_line: u32,
    pub(super) end_line: u32,
}

pub(super) struct Editor {
    pub(super) input: Entity<TextareaState>,
    initial: String,
    pub(super) submitting: bool,
    /// The comment's body when the edit began, to notice GitHub's copy changing meanwhile.
    pub(super) base: Option<String>,
    pub(super) notice: Option<String>,
    pub(super) anchor: Option<LineAnchor>,
    _subscription: Subscription,
}

/// A surface opened over the view; one at a time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Sheet {
    /// A phone editor sheet.
    Editor(Slot),
    Review,
    Reactions(String),
    Labels,
    Reviewers,
    Title,
}

/// What this client is doing to the pull request.
#[derive(Default)]
pub(super) struct Writes {
    pub(super) editors: HashMap<Slot, Editor>,
    /// Writes in flight from this client, by what sent them.
    pub(super) busy: HashSet<String>,
    /// Reactions shown before the host's next read: the account's own state by subject and
    /// content.
    pub(super) reactions: HashMap<(String, PullRequestReactionContent), bool>,
    /// A write got no answer, so its controls wait for the pull request to be read again.
    pub(super) waiting: bool,
    pub(super) sheet: Option<Sheet>,
    pub(super) title: Option<(Entity<InputState>, Subscription)>,
    pub(super) review: super::review::ReviewSheet,
    pub(super) picker: Option<super::meta::Picker>,
}

/// What a write was, for the words of its answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Write {
    Comment,
    Edit,
    Review(PullRequestReviewVerdict),
    Resolve(bool),
    Reaction,
    Labels,
    Reviewers,
}

/// One toast slot per pull request: a newer answer replaces the older one.
pub(super) struct PullRequestWrite;

/// The host's reason a write was not done, in words.
pub(super) fn rejection_reason(rejection: &PullRequestRejection, host_name: &str) -> String {
    match rejection {
        PullRequestRejection::StaleHead { .. } => crate::tr!("pull_requests.result.reason_stale"),
        PullRequestRejection::ForeignSubject => crate::tr!("pull_requests.result.reason_foreign"),
        PullRequestRejection::Invalid => {
            crate::tr!("pull_requests.result.reason_invalid", host_name = host_name)
        }
        PullRequestRejection::Unsupported => {
            crate::tr!(
                "pull_requests.result.reason_unsupported",
                host_name = host_name
            )
        }
        PullRequestRejection::NoCredential => {
            crate::tr!(
                "pull_requests.result.reason_no_credential",
                host_name = host_name
            )
        }
        PullRequestRejection::HostDisabled => {
            crate::tr!("pull_requests.result.reason_host_disabled")
        }
        PullRequestRejection::RateLimited { .. } => {
            crate::tr!(
                "pull_requests.result.reason_rate_limited",
                host_name = host_name
            )
        }
        PullRequestRejection::NotFound => {
            crate::tr!(
                "pull_requests.result.reason_not_found",
                host_name = host_name
            )
        }
        PullRequestRejection::Refused { messages } if !messages.is_empty() => {
            return messages.join(" ");
        }
        PullRequestRejection::Refused { .. } => {
            crate::tr!("pull_requests.result.reason_invalid", host_name = host_name)
        }
        PullRequestRejection::Failed => {
            crate::tr!("pull_requests.result.reason_failed", host_name = host_name)
        }
        PullRequestRejection::StackUnknown => {
            crate::tr!("pull_requests.result.reason_stack_unknown")
        }
        PullRequestRejection::StackChanged => crate::tr!("pull_requests.stack.changed_layers"),
        PullRequestRejection::LayerChanged { number, actual, .. } => crate::tr!(
            "pull_requests.stack.changed_head",
            number = number,
            new = actual.chars().take(7).collect::<String>()
        ),
        PullRequestRejection::LayerNotOpen { number, .. }
        | PullRequestRejection::LayerDraft { number } => {
            crate::tr!("pull_requests.stack.changed_state", number = number)
        }
        PullRequestRejection::NoPushAccess { numbers } => crate::tr!(
            "pull_requests.stack.result_no_push",
            list = numbers
                .iter()
                .map(|number| format!("#{number}"))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        PullRequestRejection::OperationRunning => crate::tr!("pull_requests.stack.waiting_state"),
        PullRequestRejection::MergeRunning => {
            crate::tr!("pull_requests.stack.result_409_no_id_body")
        }
        PullRequestRejection::NoGitIdentity => crate::tr!("pull_requests.stack.result_no_identity"),
        PullRequestRejection::NotLinked => crate::tr!("pull_requests.stack.result_not_linked"),
    }
    .into_owned()
}

/// The toast an answer gets, or none where its effect shows in place.
fn toast(
    write: Write,
    result: &PullRequestActionResult,
    number: u64,
    host_name: &str,
) -> Option<Notification> {
    let number = number.to_string();
    let note = match result {
        PullRequestActionResult::Applied => match write {
            Write::Review(verdict) => Notification::success(
                match verdict {
                    PullRequestReviewVerdict::Comment => {
                        crate::tr!("pull_requests.result.review_submitted")
                    }
                    PullRequestReviewVerdict::Approve => {
                        crate::tr!("pull_requests.result.review_approved", number = number)
                    }
                    PullRequestReviewVerdict::RequestChanges => {
                        crate::tr!("pull_requests.result.review_changes", number = number)
                    }
                }
                .into_owned(),
            ),
            _ => return None,
        },
        PullRequestActionResult::Rejected(rejection) => {
            let reason = rejection_reason(rejection, host_name);
            let message = match write {
                Write::Comment => {
                    crate::tr!("pull_requests.result.comment_failed", reason = reason)
                }
                Write::Edit => crate::tr!("pull_requests.result.edit_failed", reason = reason),
                Write::Review(_) => match rejection {
                    PullRequestRejection::StaleHead { .. } => {
                        crate::tr!("pull_requests.result.review_stale", number = number)
                    }
                    _ => crate::tr!("pull_requests.result.review_failed", reason = reason),
                },
                Write::Resolve(true) => {
                    crate::tr!("pull_requests.result.resolve_failed", reason = reason)
                }
                Write::Resolve(false) => {
                    crate::tr!("pull_requests.result.unresolve_failed", reason = reason)
                }
                Write::Reaction => {
                    crate::tr!("pull_requests.result.reaction_failed", reason = reason)
                }
                Write::Labels => crate::tr!("pull_requests.result.labels_failed", reason = reason),
                Write::Reviewers => {
                    crate::tr!("pull_requests.result.reviewers_failed", reason = reason)
                }
            };
            Notification::error(message.into_owned())
        }
        PullRequestActionResult::Partial {
            applied,
            unapplied,
            failure,
        } => {
            let reason = match &**failure {
                PullRequestActionResult::Rejected(rejection) => {
                    rejection_reason(rejection, host_name)
                }
                _ => crate::tr!("pull_requests.result.connection_lost").into_owned(),
            };
            // A host that sends a review as several requests stopped part way through it.
            if let Write::Review(_) = write {
                return Some(
                    Notification::error(
                        crate::tr!(
                            "pull_requests.result.review_partial_body",
                            posted = applied.join(", "),
                            not_posted = unapplied.join(", "),
                            reason = reason
                        )
                        .into_owned(),
                    )
                    .title(
                        crate::tr!("pull_requests.result.review_partial", number = number)
                            .into_owned(),
                    ),
                );
            }
            Notification::warning(
                crate::tr!(
                    "pull_requests.result.partial_body",
                    names = unapplied.join(", "),
                    reason = reason
                )
                .into_owned(),
            )
            .title(
                crate::tr!(
                    if write == Write::Reviewers {
                        "pull_requests.result.reviewers_partial"
                    } else {
                        "pull_requests.result.labels_partial"
                    },
                    number = number
                )
                .into_owned(),
            )
        }
        // Lifecycle answers, which no conversation write gets.
        PullRequestActionResult::UpToDate
        | PullRequestActionResult::Queued { .. }
        | PullRequestActionResult::AutoMergeEnabled { .. }
        | PullRequestActionResult::Opened { .. }
        | PullRequestActionResult::Pending { .. }
        | PullRequestActionResult::MergeUnconfirmed { .. }
        | PullRequestActionResult::RebaseStarted
        | PullRequestActionResult::Rebased { .. }
        | PullRequestActionResult::RebaseStopped { .. } => return None,
        PullRequestActionResult::Uncertain => match write {
            Write::Comment | Write::Edit => Notification::warning(
                crate::tr!("pull_requests.result.uncertain_comment").into_owned(),
            ),
            Write::Review(_) => Notification::warning(
                crate::tr!("pull_requests.result.uncertain_review").into_owned(),
            ),
            // The next read shows whichever it was.
            Write::Reaction => return None,
            Write::Resolve(_) | Write::Labels | Write::Reviewers => Notification::warning(
                crate::tr!(
                    "pull_requests.result.uncertain_body",
                    message = crate::tr!("pull_requests.result.connection_lost").into_owned(),
                    host_name = host_name
                )
                .into_owned(),
            )
            .title(
                crate::tr!(
                    "pull_requests.result.uncertain",
                    action = crate::tr!(match write {
                        Write::Labels => "pull_requests.result.action_labels",
                        Write::Reviewers => "pull_requests.result.action_reviewers",
                        _ => "pull_requests.result.action_resolve",
                    })
                    .into_owned(),
                    number = number
                )
                .into_owned(),
            ),
        },
    };
    // Only a success goes by itself; a refusal or a doubt stays until it is read.
    Some(note.autohide(*result == PullRequestActionResult::Applied))
}

/// A command that never reached an answer may still have reached GitHub.
pub(super) fn answer_of(
    result: Result<CommandResponse, ProtocolError>,
) -> Result<PullRequestActionResult, ProtocolError> {
    match result {
        Ok(CommandResponse::PullRequestAction(result)) => Ok(result),
        Ok(_) => Ok(PullRequestActionResult::Uncertain),
        Err(error) if matches!(error.code.as_str(), "disconnected" | "timeout") => {
            Ok(PullRequestActionResult::Uncertain)
        }
        Err(error) => Err(error),
    }
}

impl PullRequestView {
    pub(super) fn read_only(&self, cx: &App) -> bool {
        self.store
            .read(cx)
            .session_status()
            .is_none_or(|status| status.conversation_read_only)
    }

    pub(super) fn writes(&self) -> Option<&Writes> {
        self.page().map(|page| &page.writes)
    }

    pub(super) fn writes_mut(&mut self) -> Option<&mut Writes> {
        self.page_mut().map(|page| &mut page.writes)
    }

    pub(super) fn busy(&self, what: &str) -> bool {
        self.writes()
            .is_some_and(|writes| writes.waiting || writes.busy.contains(what))
    }

    /// The thread's review draft of this pull request, as the host keeps it, when the account
    /// the conversation was read as wrote it.
    pub(super) fn draft(&self, cx: &App) -> Option<PullRequestReviewDraft> {
        let (session, key) = self.current.as_ref()?;
        let account = &self.page()?.conversation.data.as_ref()?.account;
        tcode_core::pull_request::review_draft(
            &self
                .store
                .read(cx)
                .thread_meta(session)?
                .pull_request_reviews,
            key,
            account,
        )
        .cloned()
    }

    pub(super) fn open_sheet(&mut self, sheet: Option<Sheet>, cx: &mut Context<Self>) {
        if let Some(writes) = self.writes_mut() {
            writes.sheet = sheet;
        }
        cx.notify();
    }

    pub(super) fn sheet_open(&self, sheet: &Sheet) -> bool {
        self.writes()
            .is_some_and(|writes| writes.sheet.as_ref() == Some(sheet))
    }

    /// Sends one write to the host. Whatever the answer, it is told as §7 of the design says
    /// and handed to `then`; anything GitHub may have changed is read again.
    pub(super) fn send_write(
        &mut self,
        action: PullRequestAction,
        write: Write,
        busy: String,
        window: &mut Window,
        cx: &mut Context<Self>,
        then: impl FnOnce(&mut Self, &PullRequestActionResult, &mut Window, &mut Context<Self>)
        + 'static,
    ) {
        let Some(current) = self.current.clone() else {
            return;
        };
        if let Some(writes) = self.writes_mut() {
            writes.busy.insert(busy.clone());
        }
        cx.notify();
        let task = self.command_write(action, cx);
        cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            let _ = this.update_in(cx, |this, window, cx| {
                if let Some(page) = this.pages.get_mut(&current) {
                    page.writes.busy.remove(&busy);
                }
                this.written(&current, write, &result, window, cx);
                if this.current.as_ref() == Some(&current) {
                    then(this, &result, window, cx);
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// One write to the pull request on view, answered in the domain's terms.
    pub(super) fn command_write(
        &mut self,
        action: PullRequestAction,
        cx: &mut Context<Self>,
    ) -> Task<PullRequestActionResult> {
        let Some((session, key)) = self.current.clone() else {
            return Task::ready(PullRequestActionResult::Rejected(
                PullRequestRejection::Invalid,
            ));
        };
        let task = self.store.update(cx, |store, cx| {
            store.command(
                Command::RunPullRequestAction {
                    session_id: session,
                    key,
                    action,
                },
                cx,
            )
        });
        cx.spawn(async move |_, _| match answer_of(task.await) {
            Ok(result) => result,
            Err(error) => PullRequestActionResult::Rejected(PullRequestRejection::Refused {
                messages: vec![super::detail::reason(&error)],
            }),
        })
    }

    /// After an answer: what GitHub may have changed is read again, and the answer is told.
    pub(super) fn written(
        &mut self,
        current: &(String, tcode_core::pull_request::PullRequestKey),
        write: Write,
        result: &PullRequestActionResult,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let key = &current.1;
        if let Some(page) = self.pages.get_mut(current)
            && !matches!(result, PullRequestActionResult::Rejected(_))
        {
            page.conversation.expires_at = 0;
            if *result == PullRequestActionResult::Uncertain {
                page.writes.waiting = true;
            }
        }
        let host_name = super::host_name(self.store.read(cx), &key.host);
        if let Some(note) = toast(write, result, key.number, &host_name) {
            window.push_notification(
                note.id1::<PullRequestWrite>(SharedString::from(format!(
                    "{}/{}#{}",
                    key.host, key.repository, key.number
                ))),
                cx,
            );
        }
    }

    /// An edit of the host's review draft; a refusal other than the anchor checks toasts.
    pub(super) fn edit_draft(
        &mut self,
        edit: PullRequestReviewDraftEdit,
        window: &mut Window,
        cx: &mut Context<Self>,
        then: impl FnOnce(&mut Self, Result<(), ProtocolError>, &mut Window, &mut Context<Self>)
        + 'static,
    ) {
        let Some((session, key)) = self.current.clone() else {
            return;
        };
        let task = self.store.update(cx, |store, cx| {
            store.command(
                Command::EditPullRequestReviewDraft {
                    session_id: session,
                    key,
                    edit,
                },
                cx,
            )
        });
        cx.spawn_in(window, async move |this, cx| {
            let result = task.await.map(|_| ());
            let _ = this.update_in(cx, |this, window, cx| {
                if let Err(error) = &result
                    && !matches!(
                        error.code.as_str(),
                        "pull_request_not_in_diff" | "pull_request_head_changed"
                    )
                {
                    window.push_notification(
                        Notification::error(
                            crate::tr!(
                                "pull_requests.result.draft_failed",
                                reason = super::detail::reason(error)
                            )
                            .into_owned(),
                        ),
                        cx,
                    );
                }
                then(this, result, window, cx);
                cx.notify();
            });
        })
        .detach();
    }

    /// Opens the editor at `slot`, or focuses it if it is open already.
    pub(super) fn open_editor(
        &mut self,
        slot: Slot,
        placeholder: SharedString,
        text: String,
        base: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(editor) = self.writes().and_then(|writes| writes.editors.get(&slot)) {
            let input = editor.input.clone();
            input.update(cx, |input, cx| input.focus(window, cx));
            return;
        }
        let input = cx.new(|cx| {
            TextareaState::new(window, cx)
                .auto_grow(3, 12)
                .placeholder(placeholder)
        });
        if !text.is_empty() {
            input.update(cx, |input, cx| input.set_value(text.clone(), window, cx));
        }
        let submit_slot = slot.clone();
        let subscription = cx.subscribe_in(
            &input,
            window,
            move |this, _, event: &InputEvent, window, cx| match event {
                InputEvent::PressEnter {
                    secondary: true, ..
                } => this.submit_editor(submit_slot.clone(), window, cx),
                InputEvent::Change => cx.notify(),
                _ => {}
            },
        );
        input.update(cx, |input, cx| input.focus(window, cx));
        if let Some(writes) = self.writes_mut() {
            writes.editors.insert(
                slot,
                Editor {
                    input,
                    initial: text,
                    submitting: false,
                    base,
                    notice: None,
                    anchor: None,
                    _subscription: subscription,
                },
            );
        }
        cx.notify();
    }

    pub(super) fn close_editor(&mut self, slot: &Slot, cx: &mut Context<Self>) {
        if let Some(writes) = self.writes_mut() {
            writes.editors.remove(slot);
            if writes.sheet == Some(Sheet::Editor(slot.clone())) {
                writes.sheet = None;
            }
        }
        if let Some(page) = self.page() {
            page.conversation_view.list.remeasure();
            if let Some(list) = page.files_view.list.as_ref() {
                list.remeasure();
            }
        }
        cx.notify();
    }

    fn editor_text(&self, slot: &Slot, cx: &App) -> Option<String> {
        Some(
            self.writes()?
                .editors
                .get(slot)?
                .input
                .read(cx)
                .value()
                .to_string(),
        )
    }

    fn set_submitting(&mut self, slot: &Slot, submitting: bool) {
        if let Some(editor) = self
            .writes_mut()
            .and_then(|writes| writes.editors.get_mut(slot))
        {
            editor.submitting = submitting;
        }
    }

    /// Sends what the editor at `slot` holds, to wherever that editor writes.
    pub(super) fn submit_editor(
        &mut self,
        slot: Slot,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(text) = self.editor_text(&slot, cx) else {
            return;
        };
        if text.trim().is_empty()
            || self
                .writes()
                .and_then(|writes| writes.editors.get(&slot))
                .is_some_and(|editor| editor.submitting)
        {
            return;
        }
        self.set_submitting(&slot, true);
        cx.notify();
        let done = {
            let slot = slot.clone();
            move |this: &mut Self,
                  result: &PullRequestActionResult,
                  window: &mut Window,
                  cx: &mut Context<Self>| {
                this.set_submitting(&slot, false);
                if *result != PullRequestActionResult::Applied {
                    return;
                }
                match &slot {
                    // The composer stays where it is, emptied, with the focus kept.
                    Slot::Composer => {
                        if let Some(input) = this
                            .writes()
                            .and_then(|writes| writes.editors.get(&slot))
                            .map(|editor| editor.input.clone())
                        {
                            input.update(cx, |input, cx| input.set_value("", window, cx));
                        }
                        if let Some(writes) = this.writes_mut()
                            && writes.sheet == Some(Sheet::Editor(Slot::Composer))
                        {
                            writes.sheet = None;
                        }
                        this.scroll_conversation_to_end(cx);
                    }
                    Slot::Reply(thread) => {
                        // The reply is read with the thread again, past the pages read so far.
                        if let Some(page) = this.page_mut() {
                            page.conversation_view.replies.remove(thread);
                        }
                        this.close_editor(&slot, cx);
                    }
                    _ => this.close_editor(&slot, cx),
                }
            }
        };
        match slot.clone() {
            Slot::Composer => self.send_write(
                PullRequestAction::Comment { body: text },
                Write::Comment,
                "composer".into(),
                window,
                cx,
                done,
            ),
            Slot::Reply(thread_id) => self.send_write(
                PullRequestAction::ReplyToThread {
                    thread_id: thread_id.clone(),
                    body: text,
                },
                Write::Comment,
                format!("reply {thread_id}"),
                window,
                cx,
                done,
            ),
            Slot::Edit(comment_id) => self.send_write(
                PullRequestAction::EditComment {
                    comment_id: comment_id.clone(),
                    body: text,
                },
                Write::Edit,
                format!("edit {comment_id}"),
                window,
                cx,
                done,
            ),
            Slot::Description => self.send_write(
                PullRequestAction::Edit {
                    title: None,
                    body: Some(text),
                },
                Write::Edit,
                "description".into(),
                window,
                cx,
                done,
            ),
            Slot::Line => {
                let Some(anchor) = self
                    .writes()
                    .and_then(|writes| writes.editors.get(&slot))
                    .and_then(|editor| editor.anchor.clone())
                else {
                    self.set_submitting(&slot, false);
                    return;
                };
                self.edit_draft(
                    PullRequestReviewDraftEdit::AddComment {
                        head: anchor.head,
                        revision: anchor.revision,
                        path: anchor.path,
                        side: anchor.side,
                        start_line: anchor.start_line,
                        end_line: anchor.end_line,
                        body: text,
                    },
                    window,
                    cx,
                    move |this, result, _, cx| this.line_comment_added(result, cx),
                );
            }
            Slot::Pending(id) => self.edit_draft(
                PullRequestReviewDraftEdit::EditComment { id, body: text },
                window,
                cx,
                move |this, result, _, cx| {
                    this.set_submitting(&slot, false);
                    if result.is_ok() {
                        this.close_editor(&slot, cx);
                    }
                },
            ),
        }
    }

    fn line_comment_added(&mut self, result: Result<(), ProtocolError>, cx: &mut Context<Self>) {
        self.set_submitting(&Slot::Line, false);
        match result {
            Ok(()) => {
                self.close_editor(&Slot::Line, cx);
                if let Some(list) = self.diff_list_mut_for_review() {
                    list.selection = None;
                    list.remeasure();
                }
            }
            Err(error) => {
                let notice = match error.code.as_str() {
                    "pull_request_not_in_diff" => Some(
                        crate::tr!(
                            "pull_requests.review.not_in_diff",
                            host_name = self.host_name(cx)
                        )
                        .into_owned(),
                    ),
                    "pull_request_head_changed" => Some(
                        crate::tr!("pull_requests.review.head_changed").into_owned()
                            + " · "
                            + &crate::tr!(
                                "pull_requests.review.stale_title",
                                number = self
                                    .current
                                    .as_ref()
                                    .map(|(_, key)| key.number.to_string())
                                    .unwrap_or_default()
                            ),
                    ),
                    _ => None,
                };
                if let Some(editor) = self
                    .writes_mut()
                    .and_then(|writes| writes.editors.get_mut(&Slot::Line))
                {
                    editor.notice = notice;
                }
            }
        }
        cx.notify();
    }

    fn diff_list_mut_for_review(&mut self) -> Option<&mut crate::diff::list::DiffList> {
        self.page_mut()?.files_view.list.as_mut()
    }

    /// The editor at `slot` with its buttons, or nothing while it is closed. It renders the same
    /// inline and inside a phone sheet.
    pub(super) fn editor_element(
        &self,
        view: &Entity<Self>,
        slot: &Slot,
        spec: EditorSpec,
        cx: &App,
    ) -> Option<AnyElement> {
        let this = self;
        let editor = this.writes()?.editors.get(slot)?;
        let text = editor.input.read(cx).value().to_string();
        let changed = text != editor.initial;
        let blank = text.trim().is_empty();
        let submitting = editor.submitting;
        let waiting = this.writes().is_some_and(|writes| writes.waiting);
        let conflict = editor.base.as_ref().is_some_and(|base| {
            // GitHub keeps no version of a comment, so a change is only seen by reading again.
            match slot {
                Slot::Edit(id) => this
                    .comment_body(id)
                    .is_some_and(|current| current != *base),
                Slot::Description => this
                    .page()
                    .and_then(|page| page.conversation.data.as_ref())
                    .is_some_and(|conversation| conversation.description.body != *base),
                _ => false,
            }
        });
        let input = editor.input.clone();
        let notice = editor.notice.clone();
        let (cancel_view, cancel_slot) = (view.clone(), slot.clone());
        let (submit_view, submit_slot) = (view.clone(), slot.clone());
        let (escape_view, escape_slot) = (view.clone(), slot.clone());
        let cancellable = spec.cancel;
        let compact = this.compact(cx);
        Some(
            v_flex()
                .w_full()
                .gap_2()
                .text_token(&cx.theme().tokens.typography.xs)
                .font_family(cx.theme().font_family.clone())
                .on_action(move |_: &gpui_base::actions::Cancel, _, cx| {
                    // Escape cancels only an editor with nothing new in it.
                    if cancellable && !changed {
                        escape_view.update(cx, |view, cx| view.close_editor(&escape_slot, cx));
                    }
                })
                .children(spec.context.map(|context| {
                    div()
                        .text_secondary(compact, cx)
                        .overflow_hidden()
                        .child(crate::widgets::fade_text(context))
                }))
                .child(
                    Textarea::new(&input)
                        .disabled(submitting)
                        .aria_label(spec.aria),
                )
                .when(conflict, |editor| {
                    editor.child(
                        div()
                            .text_secondary(compact, cx)
                            .text_color(cx.theme().warning)
                            .child(crate::tr!(
                                "pull_requests.compose.edit_conflict",
                                host_name = this.host_name(cx)
                            )),
                    )
                })
                .children(notice.map(|notice| {
                    div()
                        .text_secondary(compact, cx)
                        .text_color(cx.theme().warning)
                        .child(notice)
                }))
                .child(
                    h_flex()
                        .justify_end()
                        .gap_2()
                        .when(spec.cancel, |row| {
                            row.child(
                                Button::new(SharedString::from(format!(
                                    "pr-editor-cancel-{slot:?}"
                                )))
                                .ghost()
                                .small()
                                .disabled(submitting)
                                .label(crate::tr!("pull_requests.compose.cancel"))
                                .on_click(move |_, _, cx| {
                                    cancel_view
                                        .update(cx, |view, cx| view.close_editor(&cancel_slot, cx))
                                }),
                            )
                        })
                        .child(
                            Button::new(SharedString::from(format!("pr-editor-submit-{slot:?}")))
                                .primary()
                                .small()
                                .loading(submitting)
                                .disabled(blank || waiting)
                                .label(spec.submit)
                                .on_click(move |_, window, cx| {
                                    submit_view.update(cx, |view, cx| {
                                        view.submit_editor(submit_slot.clone(), window, cx)
                                    })
                                }),
                        ),
                )
                .into_any_element(),
        )
    }

    /// The current body of a comment, wherever the conversation holds it.
    pub(super) fn comment_body(&self, id: &str) -> Option<String> {
        let page = self.page()?;
        let conversation = page.conversation.data.as_ref()?;
        std::iter::once(&conversation.description)
            .chain(&conversation.comments)
            .chain(
                conversation
                    .threads
                    .iter()
                    .flat_map(|thread| &thread.comments),
            )
            .chain(
                page.conversation_view
                    .replies
                    .values()
                    .flat_map(|replies| &replies.comments),
            )
            .find(|comment| comment.id == id)
            .map(|comment| comment.body.clone())
    }

    /// A 44pt row standing where an editor would be on a phone; a tap opens it in a sheet.
    pub(super) fn editor_sheet(
        &self,
        slot: Slot,
        row_label: SharedString,
        sheet_title: SharedString,
        spec: EditorSpec,
        open_editor: impl Fn(&mut Self, &mut Window, &mut Context<Self>) + 'static,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let view = cx.entity();
        let sheet = Sheet::Editor(slot.clone());
        let open = self.sheet_open(&sheet);
        let read_only = self.read_only(cx);
        let close_view = view.clone();
        let open_slot = slot.clone();
        crate::widgets::Popover::new(SharedString::from(format!("pr-editor-sheet-{slot:?}")))
            .bottom_sheet(sheet_title)
            .open(open)
            .on_open_change(move |open, _, cx| {
                if !*open {
                    close_view.update(cx, |view, cx| view.open_sheet(None, cx));
                }
            })
            .trigger(
                Button::new(SharedString::from(format!("pr-editor-row-{slot:?}")))
                    .outline()
                    .w_full()
                    .with_size(px(material::TOUCH_TARGET))
                    .disabled(read_only)
                    .label(if read_only {
                        crate::tr!("pull_requests.compose.read_only").into_owned()
                    } else {
                        row_label.to_string()
                    })
                    .on_click(cx.listener(move |this, _, window, cx| {
                        open_editor(this, window, cx);
                        this.open_sheet(Some(Sheet::Editor(open_slot.clone())), cx);
                        // The sheet takes the focus as it opens; the editor takes it back after.
                        let slot = open_slot.clone();
                        cx.defer_in(window, move |this, window, cx| {
                            if let Some(input) = this
                                .writes()
                                .and_then(|writes| writes.editors.get(&slot))
                                .map(|editor| editor.input.clone())
                            {
                                input.update(cx, |input, cx| input.focus(window, cx));
                            }
                        });
                    })),
            )
            .content(move |_, _, cx| {
                v_flex()
                    .w_full()
                    .p_3()
                    .children(view.read(cx).editor_element(&view, &slot, spec.clone(), cx))
            })
            .into_any_element()
    }
}

/// How an editor reads at one place.
#[derive(Clone)]
pub(super) struct EditorSpec {
    pub(super) context: Option<String>,
    pub(super) submit: SharedString,
    pub(super) aria: SharedString,
    pub(super) cancel: bool,
}
