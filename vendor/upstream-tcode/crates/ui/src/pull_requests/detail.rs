//! One pull request read through the host: the header, the Files / Conversation switch and
//! the reads behind them. The desktop draws it inside the Pull requests tab; the phone draws it
//! as a page.

use std::collections::{HashMap, HashSet};

use gpui::{
    AnyElement, App, AppContext as _, Context, Entity, InteractiveElement as _, IntoElement,
    ListAlignment, ListState, ParentElement as _, Render, ScrollHandle, SharedString,
    StatefulInteractiveElement as _, Styled as _, Subscription, Task, Window, div,
    prelude::FluentBuilder as _, px,
};
use gpui_base::{h_flex, v_flex};
use tcode_core::pull_request::{
    ChecksState, Mergeability, PullRequestKey, PullRequestSource, PullRequestState, ReviewDecision,
    ThreadPullRequestLink,
};
use tcode_protocol::{
    Command, ProtocolError, PullRequestConversation, PullRequestFile, PullRequestFiles,
    PullRequestRead, PullRequestReadResponse, PullRequestViewedFiles,
};

use super::{ChangeLink, ChangeWatch, appearance, row_menu, row_state, source};
use crate::{
    icon::{Icon, IconName},
    material,
    overlay::{Notification, OverlayExt as _},
    scroll::ScrollableElement as _,
    sizing::Sizable as _,
    store::{TopicKind, WorkspaceStore, observe_store_topics},
    theme::ActiveTheme as _,
    typography::TextRole as _,
    widgets::{
        button::{Button, ButtonVariants as _},
        input::Input,
        menu::DropdownMenu as _,
        tabs::{TabBar, TabItem},
        tooltip::Tooltip,
    },
    window_state::WindowState,
};

/// The phone ⋯ menu's title edit.
#[derive(gpui::Action, Clone, PartialEq, serde::Deserialize)]
#[action(namespace = tcode_pull_requests, no_json)]
struct EditTitle;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Tab {
    Files,
    Conversation,
}

/// One host read: what it last answered and when to ask again. A failure is kept until the
/// reader retries, so a failing read is not asked again on every frame.
pub(super) struct Read<T> {
    pub(super) data: Option<T>,
    pub(super) expires_at: u64,
    pub(super) loading: bool,
    pub(super) error: Option<ProtocolError>,
    /// When the data on hand was answered.
    pub(super) loaded_at: u64,
}

impl<T> Default for Read<T> {
    fn default() -> Self {
        Self {
            data: None,
            expires_at: 0,
            loading: false,
            error: None,
            loaded_at: 0,
        }
    }
}

impl<T> Read<T> {
    pub(super) fn due(&self, now: u64) -> bool {
        !self.loading && self.error.is_none() && (self.data.is_none() || now >= self.expires_at)
    }
}

pub(super) enum TextState {
    Loading,
    /// The file's text on both sides, the base side reconstructed when it had to be.
    Loaded {
        old: String,
        new: String,
    },
    Missing,
    Oversized,
    Unreadable,
}

/// What a page of the Files view holds beyond the host's answer.
#[derive(Default)]
pub(super) struct FilesView {
    pub(super) list: Option<crate::diff::list::DiffList>,
    /// The head, file count and options the list was rendered for.
    pub(super) rendered: Option<(String, usize, bool, bool, bool)>,
    pub(super) rendering: bool,
    pub(super) page_loading: bool,
    pub(super) page_error: Option<ProtocolError>,
    pub(super) texts: HashMap<String, TextState>,
    /// The reader's own collapse choices, by path; others follow the viewed mark.
    pub(super) collapsed: HashMap<String, bool>,
    /// A gap the reader expanded while its file's text was being read.
    pub(super) pending_expand: Option<(String, u32, crate::diff::model::ExpandDir)>,
    /// A file text read that failed, told once on the next draw.
    pub(super) text_failure: Option<String>,
    pub(super) off_diff_open: bool,
}

#[derive(Default)]
pub(super) struct Replies {
    pub(super) comments: Vec<tcode_protocol::PullRequestComment>,
    /// Where the next page carries on; `None` once every reply was read.
    pub(super) after: Option<String>,
    pub(super) loading: bool,
    pub(super) error: Option<String>,
}

pub(super) struct ConversationView {
    pub(super) list: ListState,
    /// By comment id, with the account its conversation was read as.
    pub(super) markdown:
        std::cell::RefCell<HashMap<String, (String, Entity<crate::markdown::MarkdownState>)>>,
    pub(super) replies: HashMap<String, Replies>,
    /// Resolved threads and long comments the reader opened.
    pub(super) expanded: HashSet<String>,
}

impl Default for ConversationView {
    fn default() -> Self {
        Self {
            list: ListState::new(0, ListAlignment::Top, px(200.)),
            markdown: Default::default(),
            replies: HashMap::new(),
            expanded: HashSet::new(),
        }
    }
}

#[derive(Default)]
pub(super) struct PullRequestPage {
    pub(super) tab: Option<Tab>,
    pub(super) files: Read<PullRequestFiles>,
    pub(super) files_view: FilesView,
    pub(super) conversation: Read<PullRequestConversation>,
    pub(super) conversation_view: ConversationView,
    pub(super) viewed: Read<PullRequestViewedFiles>,
    /// Marks shown before the host confirms them, by path.
    pub(super) viewed_marks: HashMap<String, bool>,
    /// Marks waiting to be sent together.
    pub(super) viewed_batch: HashMap<String, bool>,
    pub(super) viewed_flush: Option<Task<()>>,
    pub(super) refreshing: bool,
    /// A manual refresh reads Files again from page 1, whatever was paged in.
    pub(super) restart_files: bool,
    pub(super) writes: super::compose::Writes,
    /// What merging and the other lifecycle actions would meet, read while the pull request is
    /// open.
    pub(super) action_state: Read<tcode_protocol::PullRequestActionState>,
    /// The merge method this client chose for the pull request in its menu.
    pub(super) merge_method: Option<tcode_core::pull_request::PullRequestMergeMethod>,
}

pub struct PullRequestView {
    pub(super) store: Entity<WorkspaceStore>,
    pub(super) window_state: Entity<WindowState>,
    /// The thread the pull request is read through, and the pull request.
    pub(super) current: Option<(String, PullRequestKey)>,
    pub(super) pages: HashMap<(String, PullRequestKey), PullRequestPage>,
    pub(super) ignore_ws: bool,
    pub(super) show_invisibles: bool,
    pub(super) file_column: bool,
    pub(super) file_filter: Option<(Entity<crate::widgets::input::InputState>, Subscription)>,
    /// The phone header's chip row, which scrolls sideways.
    chips_scroll: ScrollHandle,
    wake: Option<(u64, Task<()>)>,
    _subscriptions: [Subscription; 2],
}

/// The busy mark of a lifecycle write in flight.
const LIFECYCLE: &str = "lifecycle";

pub(super) fn now() -> u64 {
    tcode_core::project::now_secs()
}

pub(super) fn ago(seconds: u64) -> String {
    crate::time::humanize_ago(now().saturating_sub(seconds))
}

pub(super) fn ago_rfc3339(stamp: &str) -> Option<String> {
    chrono::DateTime::parse_from_rfc3339(stamp)
        .ok()
        .map(|time| ago(time.timestamp().max(0) as u64))
}

/// The host's reason, at most 320 characters on screen.
pub(super) fn reason(error: &ProtocolError) -> String {
    let message = error.message.trim();
    if message.chars().count() > 320 {
        format!("{}…", message.chars().take(320).collect::<String>())
    } else {
        message.to_owned()
    }
}

impl PullRequestView {
    /// The open pull request's host as a person reads it.
    pub(super) fn host_name(&self, cx: &App) -> String {
        self.current
            .as_ref()
            .map(|(_, key)| super::host_name(self.store.read(cx), &key.host))
            .unwrap_or_default()
    }

    pub fn new(
        store: Entity<WorkspaceStore>,
        window_state: Entity<WindowState>,
        cx: &mut Context<Self>,
    ) -> Self {
        let subscriptions = [
            observe_store_topics(
                &store,
                &[
                    TopicKind::Index,
                    TopicKind::SessionStatus,
                    TopicKind::ActiveSession,
                ],
                cx,
            ),
            cx.observe(&window_state, |_, _, cx| cx.notify()),
        ];
        Self {
            store,
            window_state,
            current: None,
            pages: HashMap::new(),
            ignore_ws: false,
            show_invisibles: false,
            file_column: true,
            file_filter: None,
            chips_scroll: ScrollHandle::new(),
            wake: None,
            _subscriptions: subscriptions,
        }
    }

    pub fn show(&mut self, session: String, key: PullRequestKey, cx: &mut Context<Self>) {
        let current = Some((session, key));
        if self.current != current {
            self.current = current;
            cx.notify();
        }
    }

    pub(super) fn compact(&self, cx: &App) -> bool {
        self.window_state.read(cx).compact
    }

    pub(super) fn page(&self) -> Option<&PullRequestPage> {
        self.pages.get(self.current.as_ref()?)
    }

    pub(super) fn page_mut(&mut self) -> Option<&mut PullRequestPage> {
        let current = self.current.clone()?;
        Some(self.pages.entry(current).or_default())
    }

    /// The thread's link, or the stack layer the pull request was opened from.
    pub(super) fn link(&self, cx: &App) -> Option<ThreadPullRequestLink> {
        let (session, key) = self.current.as_ref()?;
        self.store
            .read(cx)
            .pull_requests(session)
            .iter()
            .find(|link| link.key == *key)
            .cloned()
    }

    fn stack(&self, cx: &App) -> Option<tcode_core::pull_request::PullRequestStack> {
        let (session, key) = self.current.as_ref()?;
        tcode_core::pull_request::native_stack(self.store.read(cx).pull_requests(session), key)
            .cloned()
    }

    fn stack_offer(&self, cx: &App) -> Option<super::stack::StackOffer> {
        let (session, key) = self.current.as_ref()?;
        let store = self.store.read(cx);
        super::stack::StackOffer::new(
            key,
            store.pull_requests(session),
            store
                .thread_meta(session)
                .map_or(&[][..], |meta| meta.pull_request_operations.as_slice()),
        )
    }

    pub(super) fn store(&self) -> &Entity<WorkspaceStore> {
        &self.store
    }

    /// Opens another layer of the stack in the thread's page, as a row of its map does.
    pub(super) fn select_layer(&mut self, key: PullRequestKey, cx: &mut Context<Self>) {
        if let Some((session, _)) = self.current.clone() {
            self.store.update(cx, |store, cx| {
                store.set_open_pull_request(&session, Some(key.clone()), cx)
            });
            self.show(session, key, cx);
        }
    }

    pub(super) fn url(&self, cx: &App) -> Option<String> {
        let (_, key) = self.current.as_ref()?;
        self.link(cx)
            .map(|link| link.url)
            .filter(|url| !url.is_empty())
            .or_else(|| {
                self.stack(cx)?
                    .layers
                    .iter()
                    .find(|layer| layer.number == key.number)
                    .map(|layer| layer.url.clone())
            })
    }

    pub(super) fn tab(&self, cx: &App) -> Tab {
        if let Some(tab) = self.page().and_then(|page| page.tab) {
            return tab;
        }
        let state = self
            .link(cx)
            .and_then(|link| link.snapshot)
            .map(|snapshot| snapshot.state);
        match state {
            Some(PullRequestState::Merged | PullRequestState::Closed) => Tab::Conversation,
            _ => Tab::Files,
        }
    }

    fn select_tab(&mut self, tab: Tab, cx: &mut Context<Self>) {
        if let Some(page) = self.page_mut() {
            page.tab = Some(tab);
        }
        cx.notify();
    }

    /// Asks the host one read of the pull request on view and hands the answer to `land`.
    pub(super) fn read(
        &mut self,
        read: PullRequestRead,
        cx: &mut Context<Self>,
        land: impl FnOnce(&mut PullRequestPage, Result<(PullRequestReadResponse, u64), ProtocolError>)
        + 'static,
    ) {
        if let Some(current) = self.current.clone() {
            self.read_of(current, read, cx, land);
        }
    }

    fn read_of(
        &mut self,
        (session, key): (String, PullRequestKey),
        read: PullRequestRead,
        cx: &mut Context<Self>,
        land: impl FnOnce(&mut PullRequestPage, Result<(PullRequestReadResponse, u64), ProtocolError>)
        + 'static,
    ) {
        let task = self.store.update(cx, |store, cx| {
            store.read_pull_request(session.clone(), key.clone(), read, cx)
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                if let Some(page) = this.pages.get_mut(&(session, key)) {
                    land(page, result);
                }
                cx.notify();
            });
        })
        .detach();
    }

    pub(super) fn read_files(&mut self, cx: &mut Context<Self>) {
        let Some(page) = self.page_mut() else { return };
        page.files.loading = true;
        self.read(
            PullRequestRead::Files { cursor: None },
            cx,
            |page, result| {
                page.files.loading = false;
                page.refreshing = false;
                match result {
                    Ok((PullRequestReadResponse::Files(files), expires_at)) => {
                        page.files.expires_at = expires_at;
                        page.files.error = None;
                        page.files.loaded_at = now();
                        // A re-read answers page 1 only: the pages read after it, the scroll and the
                        // completeness stay while the revisions and page 1 are what they were.
                        let restart = std::mem::take(&mut page.restart_files);
                        let continued = page.files.data.as_ref().is_some_and(|held| {
                            !restart
                                && held.files.len() > files.files.len()
                                && (&held.head, &held.base) == (&files.head, &files.base)
                                && held.files[..files.files.len()] == files.files[..]
                        });
                        if !continued && page.files.data.as_ref() != Some(&files) {
                            let head_moved = page
                                .files
                                .data
                                .as_ref()
                                .is_some_and(|held| held.head != files.head);
                            if head_moved {
                                page.files_view = FilesView::default();
                            } else {
                                page.files_view.list = None;
                                page.files_view.rendered = None;
                                page.files_view.page_error = None;
                            }
                            page.files.data = Some(files);
                        }
                    }
                    Ok(_) => {}
                    Err(error) => page.files.error = Some(error),
                }
            },
        );
    }

    pub(super) fn read_next_page(&mut self, next: String, cx: &mut Context<Self>) {
        let Some(page) = self.page_mut() else { return };
        page.files_view.page_loading = true;
        page.files_view.page_error = None;
        self.read(
            PullRequestRead::Files { cursor: Some(next) },
            cx,
            move |page, result| {
                page.files_view.page_loading = false;
                match result {
                    Ok((PullRequestReadResponse::Files(more), _)) => {
                        let Some(files) = page.files.data.as_mut() else {
                            return;
                        };
                        // A page of another head is no continuation of this one.
                        if more.head != files.head {
                            page.files.expires_at = 0;
                            return;
                        }
                        files.files.extend(more.files);
                        files.next_cursor = more.next_cursor;
                        files.complete = more.complete;
                    }
                    Ok(_) => {}
                    Err(error) => page.files_view.page_error = Some(error),
                }
            },
        );
    }

    pub(super) fn read_conversation(&mut self, cx: &mut Context<Self>) {
        let Some(page) = self.page_mut() else { return };
        page.conversation.loading = true;
        self.read(PullRequestRead::Conversation, cx, |page, result| {
            page.conversation.loading = false;
            page.refreshing = false;
            match result {
                Ok((PullRequestReadResponse::Conversation(conversation), expires_at)) => {
                    page.conversation.expires_at = expires_at;
                    page.conversation.error = None;
                    page.conversation.loaded_at = now();
                    let account_changed = page
                        .conversation
                        .data
                        .as_ref()
                        .is_some_and(|held| held.account != conversation.account);
                    if account_changed {
                        // What the previous account read is its own: read it all again.
                        page.files = Read::default();
                        page.files_view = FilesView::default();
                        page.viewed = Read::default();
                        page.viewed_marks.clear();
                        page.viewed_batch.clear();
                        page.viewed_flush = None;
                        page.conversation_view.replies.clear();
                    }
                    page.conversation.data = Some(*conversation);
                    // The read is GitHub's answer to whatever this client wrote before it.
                    page.writes.waiting = false;
                    page.writes.reactions.clear();
                }
                Ok(_) => {}
                Err(error) => page.conversation.error = Some(error),
            }
        });
    }

    pub(super) fn read_viewed(&mut self, cx: &mut Context<Self>) {
        let Some(page) = self.page_mut() else { return };
        page.viewed.loading = true;
        self.read(PullRequestRead::ViewedFiles, cx, |page, result| {
            page.viewed.loading = false;
            match result {
                Ok((PullRequestReadResponse::ViewedFiles(viewed), expires_at)) => {
                    page.viewed.expires_at = expires_at;
                    page.viewed.error = None;
                    page.viewed.loaded_at = now();
                    page.viewed.data = Some(viewed);
                    // The host's answer now carries every mark already sent.
                    page.viewed_marks
                        .retain(|path, _| page.viewed_batch.contains_key(path));
                }
                Ok(_) => {}
                Err(error) => page.viewed.error = Some(error),
            }
        });
    }

    pub(super) fn read_action_state(&mut self, cx: &mut Context<Self>) {
        if let Some(current) = self.current.clone() {
            self.read_action_state_of(current, cx);
        }
    }

    fn read_action_state_of(&mut self, page: (String, PullRequestKey), cx: &mut Context<Self>) {
        self.pages
            .entry(page.clone())
            .or_default()
            .action_state
            .loading = true;
        self.read_of(page, PullRequestRead::ActionState, cx, |page, result| {
            page.action_state.loading = false;
            match result {
                Ok((PullRequestReadResponse::ActionState(state), expires_at)) => {
                    page.action_state.expires_at = expires_at;
                    page.action_state.error = None;
                    page.action_state.loaded_at = now();
                    page.action_state.data = Some(state);
                    // GitHub's state as it now is answers a write that went unanswered.
                    page.writes.waiting = false;
                }
                Ok(_) => {}
                Err(error) => page.action_state.error = Some(error),
            }
        });
    }

    /// Whether a lifecycle write to the pull request is in flight, or went unanswered and
    /// GitHub has not been read since; the detail and the list rows wait alike.
    pub(super) fn lifecycle_busy(&self, session: &str, key: &PullRequestKey) -> bool {
        self.pages
            .get(&(session.to_owned(), key.clone()))
            .is_some_and(|page| page.writes.waiting || page.writes.busy.contains(LIFECYCLE))
    }

    /// Where a lifecycle action goes; its answer has the view read what it may have changed.
    /// `shown` carries what the detail has read of it; a list row offers from its link alone.
    pub(super) fn lifecycle_target(
        &self,
        session: &str,
        key: &PullRequestKey,
        shown: bool,
        cx: &mut Context<Self>,
    ) -> Option<super::lifecycle::Target> {
        let current = (session.to_owned(), key.clone());
        let page = self.pages.get(&current).filter(|_| shown);
        let mut target = super::lifecycle::Target::new(
            &self.store,
            &self.window_state,
            session,
            key,
            page.and_then(|page| page.action_state.data.clone()),
            page.and_then(|page| page.merge_method),
            cx,
        )?;
        target.offer.busy = self.lifecycle_busy(session, key);
        let view = cx.entity().downgrade();
        let sent = current.clone();
        target.started = std::rc::Rc::new({
            let view = view.clone();
            move |cx| {
                let _ = view.update(cx, |view, cx| {
                    let page = view.pages.entry(sent.clone()).or_default();
                    page.writes.busy.insert(LIFECYCLE.into());
                    cx.notify();
                });
            }
        });
        target.done = std::rc::Rc::new(move |result, _, cx| {
            let _ = view.update(cx, |view, cx| {
                let page = view.pages.entry(current.clone()).or_default();
                page.writes.busy.remove(LIFECYCLE);
                page.action_state.expires_at = 0;
                if !matches!(result, tcode_protocol::PullRequestActionResult::Rejected(_)) {
                    page.conversation.expires_at = 0;
                    page.files.expires_at = 0;
                }
                if *result == tcode_protocol::PullRequestActionResult::Uncertain {
                    page.writes.waiting = true;
                    // The detail reads it when shown; a row's pull request is read here.
                    if view.current.as_ref() != Some(&current) {
                        view.read_action_state_of(current.clone(), cx);
                    }
                }
                cx.notify();
            });
        });
        Some(target)
    }

    /// The target of the pull request on view.
    fn shown_target(&self, cx: &mut Context<Self>) -> Option<super::lifecycle::Target> {
        let (session, key) = self.current.as_ref()?;
        self.lifecycle_target(session, key, true, cx)
    }

    fn run_lifecycle(
        &mut self,
        action: &super::lifecycle::RunLifecycle,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.current.as_ref().map(|(_, key)| key) != Some(&action.key) {
            cx.propagate();
            return;
        }
        if let Some(target) = self.shown_target(cx) {
            target.run(action.kind, window, cx);
        }
    }

    fn choose_merge_method(
        &mut self,
        action: &super::lifecycle::ChooseMergeMethod,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.current.as_ref().map(|(_, key)| key) != Some(&action.key) {
            return;
        }
        if let Some(page) = self.page_mut() {
            page.merge_method = Some(action.method);
        }
        cx.notify();
    }

    /// Starts the reads the visible view needs, and wakes when the soonest of them expires.
    fn ensure_reads(&mut self, cx: &mut Context<Self>) {
        let tab = self.tab(cx);
        let Some(key) = self.current.as_ref().map(|(_, key)| key.clone()) else {
            return;
        };
        let open = self
            .link(cx)
            .and_then(|link| link.snapshot)
            .is_some_and(|snapshot| snapshot.state == PullRequestState::Open);
        let Some(page) = self.page_mut() else { return };
        let now = now();
        let action_due = open && page.action_state.due(now);
        let action_expires = page.action_state.expires_at;
        let (files_due, conversation_due, viewed_due) = (
            page.files.due(now),
            page.conversation.due(now),
            page.viewed.due(now),
        );
        let (conversation_expires, files_expires, viewed_expires) = (
            page.conversation.expires_at,
            page.files.expires_at,
            page.viewed.expires_at,
        );
        if conversation_due && page.conversation.data.is_some() {
            // Media that failed is asked again along with the conversation that names it.
            crate::store::retry_failed_pull_request_media(&key, cx);
        }
        let mut expiries = vec![conversation_expires];
        if open {
            expiries.push(action_expires);
        }
        if action_due {
            self.read_action_state(cx);
        }
        if tab == Tab::Files {
            expiries.extend([files_expires, viewed_expires]);
        }
        match tab {
            Tab::Files => {
                if files_due {
                    self.read_files(cx);
                }
                // The review threads drawn on the diff come with the conversation.
                if conversation_due {
                    self.read_conversation(cx);
                }
                if viewed_due {
                    self.read_viewed(cx);
                }
            }
            Tab::Conversation => {
                if conversation_due {
                    self.read_conversation(cx);
                }
            }
        }
        let soonest = expiries.into_iter().filter(|at| *at > now).min();
        if let Some(at) = soonest
            && self.wake.as_ref().is_none_or(|(wake, _)| *wake != at)
        {
            let delay = std::time::Duration::from_secs(at.saturating_sub(now));
            let task = cx.spawn(async move |this, cx| {
                cx.background_executor().timer(delay).await;
                let _ = this.update(cx, |this, cx| {
                    this.wake = None;
                    cx.notify();
                });
            });
            self.wake = Some((at, task));
        }
    }

    /// A manual refresh: the host drops what it holds about the pull request and the visible
    /// view reads again from the start.
    pub(super) fn refresh(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((session, key)) = self.current.clone() else {
            return;
        };
        let tab = self.tab(cx);
        crate::store::retry_failed_pull_request_media(&key, cx);
        let Some(page) = self.page_mut() else { return };
        page.refreshing = true;
        page.restart_files = true;
        page.files.error = None;
        page.conversation.error = None;
        page.viewed.error = None;
        page.action_state.error = None;
        page.action_state.expires_at = 0;
        page.conversation_view.replies.clear();
        let task = self.store.update(cx, |store, cx| {
            store.command(
                Command::RefreshPullRequest {
                    session_id: session,
                    key,
                },
                cx,
            )
        });
        cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            let _ = this.update_in(cx, |this, window, cx| match result {
                Ok(_) => {
                    match tab {
                        Tab::Files => {
                            this.read_files(cx);
                            this.read_viewed(cx);
                            this.read_conversation(cx);
                        }
                        Tab::Conversation => this.read_conversation(cx),
                    }
                    cx.notify();
                }
                Err(error) => {
                    if let Some(page) = this.page_mut() {
                        page.refreshing = false;
                    }
                    window.push_notification(
                        Notification::warning(
                            crate::tr!("pull_requests.detail.refresh_failed", ago = ago(now()))
                                .into_owned()
                                + " "
                                + &reason(&error),
                        ),
                        cx,
                    );
                    cx.notify();
                }
            });
        })
        .detach();
    }

    pub(super) fn retry(&mut self, cx: &mut Context<Self>) {
        if let Some(page) = self.page_mut() {
            page.files.error = None;
            page.conversation.error = None;
            page.viewed.error = None;
            page.action_state.error = None;
        }
        cx.notify();
    }

    fn change_link(&mut self, action: &ChangeLink, window: &mut Window, cx: &mut Context<Self>) {
        super::change_link(&self.store, action, window, cx);
    }

    fn change_watch(&mut self, action: &ChangeWatch, window: &mut Window, cx: &mut Context<Self>) {
        super::change_watch(&self.store, action, window, cx);
    }

    fn header(&self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let compact = self.compact(cx);
        let Some((session, key)) = self.current.clone() else {
            return div().into_any_element();
        };
        let link = self.link(cx);
        let stack = self.stack(cx);
        let layer = stack
            .as_ref()
            .and_then(|stack| stack.layers.iter().find(|layer| layer.number == key.number));
        let visible = link.as_ref().is_some_and(|link| link.visible());
        let snapshot = link
            .as_ref()
            .filter(|link| link.visible())
            .and_then(|link| link.snapshot.clone());
        let badge = row_state(
            link.as_ref().and_then(|link| link.snapshot.as_ref()),
            layer.map(|layer| layer.state),
        );
        let (glyph, color, state_label) = appearance(badge, cx);
        let title = snapshot
            .as_ref()
            .map(|s| s.title.clone())
            .unwrap_or_else(|| {
                layer
                    .map(|layer| layer.head_branch.clone())
                    .unwrap_or_else(|| key.repository.clone())
            });
        let muted = cx.theme().muted_foreground;
        let mut line_two = h_flex()
            .flex_wrap()
            .gap_x(cx.theme().tokens.spacing.sm)
            .gap_y(cx.theme().tokens.spacing.xxs)
            .items_center()
            .text_secondary(compact, cx)
            .child(
                div()
                    .id("pr-detail-number")
                    .font_family(cx.theme().mono_font_family.clone())
                    .child(format!("#{}", key.number))
                    .tooltip({
                        let label = crate::tr!(
                            "pull_requests.number_tooltip",
                            source = crate::tr!(source(link.as_ref())).into_owned(),
                            ago = ago(link
                                .as_ref()
                                .and_then(|link| link.linked_at)
                                .unwrap_or_default())
                        )
                        .into_owned();
                        move |window, cx| Tooltip::new(label.clone()).build(window, cx)
                    }),
            )
            .child(material::semantic_chip(
                crate::tr!(state_label).into_owned(),
                color.opacity(0.1),
                color,
                cx,
            ));
        if let Some(snapshot) = &snapshot {
            if let Some(author) = &snapshot.author {
                line_two = line_two.child(
                    h_flex()
                        .gap_1()
                        .items_center()
                        // The conversation names the avatar the host may read for this author.
                        .child(self.avatar(
                            &author.login,
                            self.author_avatar(&author.login).as_deref(),
                            16.,
                            cx,
                        ))
                        .child(author.login.clone()),
                );
            }
            let (base, head) = (snapshot.base_branch.clone(), snapshot.head_branch.clone());
            line_two = line_two
                .child(
                    div()
                        .id("pr-detail-branches")
                        .min_w_0()
                        .max_w(px(320.))
                        .overflow_hidden()
                        .font_family(cx.theme().mono_font_family.clone())
                        // gpui-base has no middle truncation; the tooltip holds the full pair.
                        .child(crate::widgets::fade_text(format!("{base} ← {head}")))
                        .tooltip(move |window, cx| {
                            Tooltip::new(
                                crate::tr!(
                                    "pull_requests.detail.merging_into",
                                    head = head.clone(),
                                    base = base.clone()
                                )
                                .into_owned(),
                            )
                            .build(window, cx)
                        }),
                )
                .children(ago_rfc3339(&snapshot.updated_at).map(|ago| {
                    crate::tr!(
                        if self
                            .store
                            .read(cx)
                            .settings()
                            .source_control
                            .kind(&key.host)
                            == tcode_core::pull_request::HostKind::Azure
                        {
                            if snapshot.merged_at.is_some() {
                                "pull_requests.conversation.merged"
                            } else if snapshot.closed_at.is_some() {
                                "pull_requests.conversation.closed"
                            } else {
                                "pull_requests.detail.created"
                            }
                        } else {
                            "pull_requests.detail.updated"
                        },
                        ago = ago
                    )
                    .into_owned()
                }));
        }
        let mut chips = h_flex()
            .gap(cx.theme().tokens.spacing.sm)
            .items_center()
            .text_secondary(compact, cx)
            .text_color(cx.theme().foreground);
        let mut has_chip = false;
        if let Some(snapshot) = snapshot
            .as_ref()
            .filter(|s| s.state == PullRequestState::Open)
        {
            let chip = |icon: IconName, color, label: String| {
                h_flex()
                    .flex_none()
                    .gap_1()
                    .items_center()
                    .child(Icon::new(icon).text_color(color))
                    .child(label)
            };
            if let Some(checks) = snapshot.checks_state {
                has_chip = true;
                let (icon, color, label) = match checks {
                    ChecksState::Passing => (
                        IconName::CircleCheck,
                        cx.theme().success,
                        "pull_requests.checks_passing",
                    ),
                    ChecksState::Failing => (
                        IconName::CircleX,
                        cx.theme().danger,
                        "pull_requests.checks_failing",
                    ),
                    ChecksState::Pending => (
                        IconName::CircleDashed,
                        cx.theme().warning,
                        "pull_requests.checks_pending",
                    ),
                };
                chips = chips.child(chip(icon, color, crate::tr!(label).into_owned()));
            }
            if let Some(review) = snapshot.review_decision {
                has_chip = true;
                let (icon, color, label) = match review {
                    ReviewDecision::Approved => (
                        IconName::BadgeCheck,
                        cx.theme().success,
                        "pull_requests.review_approved",
                    ),
                    ReviewDecision::ChangesRequested => (
                        IconName::MessageSquareWarning,
                        cx.theme().danger,
                        "pull_requests.review_changes_requested",
                    ),
                    ReviewDecision::Required => (
                        IconName::MessageSquareMore,
                        cx.theme().muted_foreground,
                        "pull_requests.review_required",
                    ),
                };
                chips = chips.child(chip(icon, color, crate::tr!(label).into_owned()));
            }
            if let Some(behind) = self.behind_chip(&key, snapshot, cx) {
                has_chip = true;
                chips = chips.child(behind);
            }
            if snapshot.mergeability == Mergeability::Conflicting {
                has_chip = true;
                chips = chips.child(chip(
                    IconName::GitMergeConflict,
                    cx.theme().warning,
                    crate::tr!(
                        "pull_requests.conflict",
                        base = snapshot.base_branch.clone()
                    )
                    .into_owned(),
                ));
            }
        }
        if let Some(offer) = self.stack_offer(cx) {
            has_chip = true;
            let target = (!self.read_only(cx))
                .then(|| self.shown_target(cx))
                .flatten();
            let action = self.page().and_then(|page| page.action_state.data.clone());
            chips = chips.child(super::stack::map_selector(
                cx.entity(),
                offer,
                target,
                action,
                compact,
                cx,
            ));
        }
        if visible
            && link.as_ref().is_some_and(|link| link.watch.is_some())
            && snapshot
                .as_ref()
                .is_none_or(|s| s.state == PullRequestState::Open)
        {
            has_chip = true;
            let paused = self
                .store
                .read(cx)
                .thread_meta(&session)
                .is_some_and(|meta| meta.pull_request_watches_paused);
            chips = chips.child(
                div()
                    .id("pr-detail-watch")
                    .child(Icon::new(IconName::Eye).text_color(muted))
                    .tooltip(move |window, cx| {
                        Tooltip::new(
                            crate::tr!(if paused {
                                "pull_requests.watching_paused_tooltip"
                            } else {
                                "pull_requests.watching_tooltip"
                            })
                            .into_owned(),
                        )
                        .build(window, cx)
                    }),
            );
        }
        let condition = match link.as_ref().map(|link| link.source) {
            Some(PullRequestSource::Dismissed) => Some("pull_requests.source_dismissed"),
            None => Some("pull_requests.source_not_linked"),
            _ => None,
        };
        let _ = (&session, window);
        v_flex()
            .flex_none()
            .px(if compact {
                px(material::COMPACT_PAGE_INSET)
            } else {
                cx.theme().tokens.spacing.sm
            })
            .pt_2()
            .pb_2()
            .gap(cx.theme().tokens.spacing.xs)
            .child(
                h_flex()
                    .items_start()
                    .gap_2()
                    .child(
                        div()
                            .pt(cx.theme().tokens.spacing.xs)
                            .text_title(cx)
                            .child(Icon::new(glyph).text_color(color)),
                    )
                    .child(self.title_line(title, snapshot.is_some(), cx)),
            )
            .child(line_two)
            .when(has_chip, |header| {
                header.child(if compact {
                    let handle = self.chips_scroll.clone();
                    let surface = material::content_surface(cx);
                    div()
                        .relative()
                        .w_full()
                        .child(
                            h_flex()
                                .id("pr-detail-chips")
                                .w_full()
                                .overflow_x_scroll_area()
                                .track_scroll(&self.chips_scroll)
                                .child(chips.flex_none()),
                        )
                        // gpui-base has no scroll-edge cue: an edge with more chips past it fades.
                        .child(
                            gpui::canvas(
                                |_, _, _| {},
                                move |bounds, _, window, _| {
                                    let (offset, max) = (handle.offset().x, handle.max_offset().x);
                                    let clear = surface.opacity(0.);
                                    let fade = px(24.).min(bounds.size.width / 2.);
                                    let edges = [
                                        (offset < px(0.), bounds.origin, 270.),
                                        (
                                            -offset < max,
                                            gpui::point(bounds.right() - fade, bounds.top()),
                                            90.,
                                        ),
                                    ];
                                    for (shown, origin, angle) in edges {
                                        if shown {
                                            window.paint_quad(gpui::fill(
                                                gpui::Bounds::new(
                                                    origin,
                                                    gpui::size(fade, bounds.size.height),
                                                ),
                                                gpui::linear_gradient(
                                                    angle,
                                                    gpui::linear_color_stop(surface, 1.),
                                                    gpui::linear_color_stop(clear, 0.),
                                                ),
                                            ));
                                        }
                                    }
                                },
                            )
                            .absolute()
                            .inset_0(),
                        )
                        .into_any_element()
                } else {
                    chips.flex_wrap().into_any_element()
                })
            })
            .children(condition.map(|condition| {
                h_flex()
                    .gap_1()
                    .items_center()
                    .text_secondary(compact, cx)
                    .child(Icon::new(IconName::Unlink))
                    .child(crate::tr!(condition))
            }))
            .into_any_element()
    }

    /// How far the head is behind its base, and the updates the account may make from it. Not on
    /// a stack layer, whose layers move together, nor while it conflicts.
    fn behind_chip(
        &self,
        key: &PullRequestKey,
        snapshot: &tcode_core::pull_request::PullRequestSnapshot,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let offer = self.shown_target(cx)?.offer;
        let state = offer.action.as_ref()?;
        let behind = state.behind_by.filter(|behind| *behind > 0)?;
        if offer.conflicting
            || offer.route != tcode_core::pull_request::PullRequestStackRoute::Single
        {
            return None;
        }
        let base = snapshot.base_branch.clone();
        let label = if behind == 1 {
            crate::tr!("pull_requests.actions.behind_one", base = base.clone()).into_owned()
        } else {
            crate::tr!(
                "pull_requests.actions.behind",
                count = behind.to_string(),
                base = base.clone()
            )
            .into_owned()
        };
        let merges = super::lifecycle::merges_base(Some(state));
        let tooltip = crate::tr!(
            if merges {
                "pull_requests.actions.update_tooltip"
            } else {
                "pull_requests.actions.rebase_tooltip"
            },
            base = base,
            head = snapshot.head_branch.clone(),
            host_name = self.host_name(cx)
        )
        .into_owned();
        let updates =
            (state.can_update_branch && state.capabilities.update_branch && !self.read_only(cx))
                .then(|| key.clone());
        let busy = offer.busy;
        let compact = self.compact(cx);
        let chip = Button::new("pr-behind")
            .ghost()
            .compact()
            .xsmall()
            .icon(IconName::ArrowUpDown)
            .label(label);
        Some(match updates {
            Some(key) => chip
                .dropdown_menu(move |menu, _, _| {
                    let item = |kind| {
                        Box::new(super::lifecycle::RunLifecycle {
                            key: key.clone(),
                            kind,
                        })
                    };
                    let menu = menu.label(tooltip.clone());
                    let menu = if merges {
                        menu.menu_with_icon_and_enable(
                            crate::tr!("pull_requests.actions.update_branch").into_owned(),
                            IconName::GitMerge,
                            item(super::lifecycle::Lifecycle::UpdateBranch),
                            !busy,
                        )
                    } else {
                        menu
                    };
                    menu.menu_with_icon_and_enable(
                        crate::tr!("pull_requests.actions.update_rebase_menu").into_owned(),
                        IconName::RefreshCw,
                        item(super::lifecycle::Lifecycle::UpdateRebase),
                        !busy,
                    )
                })
                .touch(compact)
                .into_any_element(),
            // News, not an offer, for an account that may not update the branch.
            None => chip.tooltip(tooltip).into_any_element(),
        })
    }

    /// The title, its edit affordance (`pr-title-edit`) and the title editor.
    fn title_line(&self, title: String, known: bool, cx: &mut Context<Self>) -> AnyElement {
        let compact = self.compact(cx);
        let editable = known
            && !self.read_only(cx)
            && self
                .page()
                .and_then(|page| page.conversation.data.as_ref())
                .is_some_and(|conversation| conversation.permissions.update);
        let editing = self.writes().and_then(|writes| writes.title.as_ref());
        if let Some((input, _)) = editing.filter(|_| !compact) {
            let saving = self.busy("title");
            let blank = input.read(cx).value().trim().is_empty();
            return h_flex()
                .flex_1()
                .min_w_0()
                .gap_2()
                .items_center()
                .on_action(
                    cx.listener(|this, _: &gpui_base::actions::Cancel, _, cx| {
                        this.cancel_title(cx)
                    }),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .text_title(cx)
                        .child(Input::new(input).disabled(saving)),
                )
                .child(
                    Button::new("pr-title-cancel")
                        .ghost()
                        .xsmall()
                        .disabled(saving)
                        .label(crate::tr!("pull_requests.compose.cancel"))
                        .on_click(cx.listener(|this, _, _, cx| this.cancel_title(cx))),
                )
                .child(
                    Button::new("pr-title-save")
                        .primary()
                        .xsmall()
                        .loading(saving)
                        .disabled(blank)
                        .label(crate::tr!("pull_requests.compose.save"))
                        .on_click(cx.listener(|this, _, window, cx| this.save_title(window, cx))),
                )
                .into_any_element();
        }
        let text = div()
            .flex_1()
            .min_w_0()
            .text_title(cx)
            .line_clamp(2)
            .when(!known, |title| {
                title.font_family(cx.theme().mono_font_family.clone())
            })
            .child(title.clone());
        if !editable {
            return text.into_any_element();
        }
        // A phone starts the edit from the ⋯ menu; the sheet needs no trigger.
        let affordance = if compact {
            self.title_sheet(cx)
        } else {
            div()
                .flex_none()
                .invisible()
                .group_hover("pr-header-title", |style| style.visible())
                .child(
                    Button::new("pr-title-edit")
                        .ghost()
                        .xsmall()
                        .compact()
                        .icon(IconName::Pencil)
                        .tooltip(crate::tr!("pull_requests.compose.edit_title"))
                        .on_click(
                            cx.listener(move |this, _, window, cx| this.start_title(window, cx)),
                        ),
                )
                .into_any_element()
        };
        h_flex()
            .group("pr-header-title")
            .flex_1()
            .min_w_0()
            .gap_1()
            .items_start()
            .child(text)
            .child(affordance)
            .into_any_element()
    }

    /// The phone's title editor.
    fn title_sheet(&self, cx: &mut Context<Self>) -> AnyElement {
        let view = cx.entity();
        crate::widgets::Popover::new("pr-title-sheet")
            .bottom_sheet(crate::tr!("pull_requests.compose.edit_title").into_owned())
            .open(self.sheet_open(&super::compose::Sheet::Title))
            .on_open_change({
                let view = view.clone();
                move |open, _, cx| {
                    if !*open {
                        view.update(cx, |view, cx| view.cancel_title(cx));
                    }
                }
            })
            .content(move |_, _, cx| {
                let this = view.read(cx);
                let Some((input, _)) = this.writes().and_then(|writes| writes.title.as_ref())
                else {
                    return div().into_any_element();
                };
                let saving = this.busy("title");
                let blank = input.read(cx).value().trim().is_empty();
                let save_view = view.clone();
                v_flex()
                    .w_full()
                    .p_3()
                    .gap_3()
                    .child(Input::new(input).disabled(saving))
                    .child(
                        h_flex().justify_end().child(
                            Button::new("pr-title-sheet-save")
                                .primary()
                                .small()
                                .loading(saving)
                                .disabled(blank)
                                .label(crate::tr!("pull_requests.compose.save"))
                                .on_click(move |_, window, cx| {
                                    save_view.update(cx, |view, cx| view.save_title(window, cx))
                                }),
                        ),
                    )
                    .into_any_element()
            })
            .into_any_element()
    }

    fn start_title(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let title = self
            .link(cx)
            .and_then(|link| link.snapshot)
            .map(|snapshot| snapshot.title)
            .unwrap_or_default();
        let input = cx.new(|cx| {
            let mut input = crate::widgets::input::InputState::new(window, cx);
            input.set_value(title, window, cx);
            input
        });
        let subscription = cx.subscribe_in(
            &input,
            window,
            |this, _, event: &crate::widgets::input::InputEvent, window, cx| match event {
                crate::widgets::input::InputEvent::PressEnter { .. } => this.save_title(window, cx),
                crate::widgets::input::InputEvent::Change => cx.notify(),
                _ => {}
            },
        );
        input.update(cx, |input, cx| {
            input.focus(window, cx);
            input.select_all(window, cx);
        });
        let compact = self.compact(cx);
        if let Some(writes) = self.writes_mut() {
            writes.title = Some((input, subscription));
            if compact {
                writes.sheet = Some(super::compose::Sheet::Title);
            }
        }
        cx.notify();
    }

    fn cancel_title(&mut self, cx: &mut Context<Self>) {
        if self.busy("title") {
            return;
        }
        if let Some(writes) = self.writes_mut() {
            writes.title = None;
            if writes.sheet == Some(super::compose::Sheet::Title) {
                writes.sheet = None;
            }
        }
        cx.notify();
    }

    /// Sends the title alone; the description is left out, so GitHub keeps it.
    fn save_title(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(title) = self
            .writes()
            .and_then(|writes| writes.title.as_ref())
            .map(|(input, _)| input.read(cx).value().trim().to_owned())
            .filter(|title| !title.is_empty())
        else {
            return;
        };
        if self.busy("title") {
            return;
        }
        self.send_write(
            tcode_protocol::PullRequestAction::Edit {
                title: Some(title),
                body: None,
            },
            super::compose::Write::Edit,
            "title".into(),
            window,
            cx,
            |this, result, _, cx| {
                if *result == tcode_protocol::PullRequestActionResult::Applied {
                    this.cancel_title(cx);
                }
            },
        );
    }

    /// The menu of the row this pull request was opened from.
    fn menu(&self, cx: &mut Context<Self>) -> Option<super::RowMenu> {
        let offer = (!self.read_only(cx))
            .then(|| self.shown_target(cx).map(|target| target.offer))
            .flatten();
        let (session, key) = self.current.as_ref()?;
        let link = self.link(cx);
        let watchable = self
            .store
            .read(cx)
            .thread_meta(session)
            .is_some_and(|meta| {
                !meta.is_settled() && meta.archived_at.is_none() && meta.parent_session_id.is_none()
            });
        Some(row_menu(
            key.clone(),
            self.url(cx).unwrap_or_default(),
            self.host_name(cx),
            link.as_ref(),
            watchable,
            move |_| offer.clone(),
        ))
    }

    fn sub_bar(&self, cx: &mut Context<Self>) -> AnyElement {
        let key = self.current.as_ref().map(|(_, key)| key.clone());
        let refreshing = self.page().is_some_and(|page| page.refreshing);
        let url = self.url(cx);
        let store = self.store.clone();
        let session = self.current.as_ref().map(|(session, _)| session.clone());
        material::bar_divider_below(
            material::bar("pr-detail-sub-bar", material::Bar::Toolbar, false, cx),
            cx,
        )
        .items_center()
        .child(
            Button::new("pr-detail-back")
                .ghost()
                .small()
                .icon(IconName::ChevronLeft)
                .label(crate::tr!("pull_requests.detail.back"))
                .tooltip(crate::tr!("pull_requests.detail.back_tooltip"))
                .on_click(move |_, _, cx| {
                    if let Some(session) = &session {
                        store.update(cx, |store, cx| {
                            store.set_open_pull_request(session, None, cx)
                        });
                    }
                }),
        )
        .child(div().flex_1())
        .children(self.primary(false, cx))
        .child(self.refresh_button(refreshing, false, cx))
        .when_some(url, |bar, url| {
            bar.child(
                Button::new("pr-detail-open")
                    .ghost()
                    .small()
                    .compact()
                    .icon(IconName::ExternalLink)
                    .tooltip(crate::tr!(
                        "pull_requests.open_on_host",
                        host_name = self.host_name(cx)
                    ))
                    .on_click(move |_, _, cx| cx.open_url(&url)),
            )
        })
        .when_some(self.menu(cx).zip(key), |bar, (menu, key)| {
            bar.child(
                Button::new("pr-detail-menu")
                    .ghost()
                    .small()
                    .compact()
                    .icon(IconName::Ellipsis)
                    .aria_label(
                        crate::tr!("pull_requests.actions_for", number = key.number.to_string())
                            .into_owned(),
                    )
                    .tooltip(
                        crate::tr!("pull_requests.actions_for", number = key.number.to_string())
                            .into_owned(),
                    )
                    .dropdown_menu(move |menu_state, window, cx| (menu)(menu_state, window, cx)),
            )
        })
        .into_any_element()
    }

    /// The header's primary action (`pr-primary-action`): one, by the rank, from the host's
    /// action state.
    fn primary(&self, compact: bool, cx: &mut Context<Self>) -> Option<AnyElement> {
        if self.read_only(cx) {
            return None;
        }
        let target = self.shown_target(cx)?;
        let primary = target.offer.primary()?;
        Some(super::lifecycle::primary_element(
            &target, primary, compact, cx,
        ))
    }

    pub(super) fn refresh_button(
        &self,
        refreshing: bool,
        compact: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let loaded = self.page().and_then(|page| {
            [page.files.loaded_at, page.conversation.loaded_at]
                .into_iter()
                .filter(|at| *at > 0)
                .min()
        });
        let tooltip = match (refreshing, loaded) {
            (true, _) => crate::tr!("pull_requests.detail.refreshing").into_owned(),
            (false, Some(loaded)) => {
                crate::tr!("pull_requests.detail.refresh", ago = ago(loaded)).into_owned()
            }
            (false, None) => crate::tr!("pull_requests.detail.refresh_unloaded").into_owned(),
        };
        // `toolbar_icon_button`'s touch form takes its glyph as a child, which `Button::loading`
        // cannot swap for a spinner, so this square holds the button's place while it spins.
        if refreshing {
            return div()
                .flex_none()
                .size(px(if compact { material::TOUCH_TARGET } else { 24. }))
                .flex()
                .items_center()
                .justify_center()
                .child(crate::widgets::spinner::Spinner::new().small())
                .into_any_element();
        }
        material::toolbar_icon_button("pr-detail-refresh", IconName::RefreshCw, tooltip, compact)
            .on_click(cx.listener(|this, _, window, cx| this.refresh(window, cx)))
            .into_any_element()
    }

    /// The phone nav bar's trailing actions: refresh and the row menu.
    pub fn nav_actions(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let refreshing = self.page().is_some_and(|page| page.refreshing);
        let mut actions = vec![self.refresh_button(refreshing, true, cx)];
        if let Some((menu, key)) = self
            .menu(cx)
            .zip(self.current.as_ref().map(|(_, key)| key.clone()))
        {
            let edit_title = !self.read_only(cx)
                && self
                    .page()
                    .and_then(|page| page.conversation.data.as_ref())
                    .is_some_and(|conversation| conversation.permissions.update);
            // The nav bar is a sibling of this view, so its menu needs the
            // view's action handlers on its own dispatch chain.
            actions.push(
                div()
                    .on_action(cx.listener(Self::change_link))
                    .on_action(cx.listener(Self::change_watch))
                    .on_action(cx.listener(Self::run_lifecycle))
                    .on_action(cx.listener(Self::choose_merge_method))
                    .on_action(
                        cx.listener(|this, _: &EditTitle, window, cx| this.start_title(window, cx)),
                    )
                    .child(
                        material::toolbar_icon_button(
                            "pr-detail-menu",
                            IconName::Ellipsis,
                            crate::tr!(
                                "pull_requests.actions_for",
                                number = key.number.to_string()
                            )
                            .into_owned(),
                            true,
                        )
                        .dropdown_menu(move |menu_state, window, cx| {
                            let menu_state = (menu)(menu_state, window, cx);
                            if edit_title {
                                menu_state.separator().menu_with_icon(
                                    crate::tr!("pull_requests.compose.edit_title").into_owned(),
                                    IconName::Pencil,
                                    Box::new(EditTitle),
                                )
                            } else {
                                menu_state
                            }
                        })
                        .touch(true),
                    )
                    .into_any_element(),
            );
        }
        actions
    }

    /// The nav bar title on a phone: the number only, the title is in the header.
    pub fn nav_title(&self) -> SharedString {
        self.current
            .as_ref()
            .map(|(_, key)| format!("#{}", key.number))
            .unwrap_or_default()
            .into()
    }

    /// The Files tab's label, counted by the files read once it answers and by the snapshot
    /// before that; a host that gives no count shows none rather than zero.
    fn files_label(&self, cx: &App) -> String {
        let read = self
            .page()
            .and_then(|page| page.files.data.as_ref())
            .map(|files| files.changed_files);
        let count = read.or_else(|| {
            self.link(cx)
                .and_then(|link| link.snapshot.as_ref().and_then(|s| s.changed_files))
        });
        count_label(
            "pull_requests.detail.files",
            "pull_requests.detail.files_count",
            count,
        )
    }

    fn switch(&self, cx: &mut Context<Self>) -> AnyElement {
        let compact = self.compact(cx);
        let tab = self.tab(cx);
        let page = self.page();
        let link = self.link(cx);
        let conversation_count =
            page.and_then(|page| page.conversation.data.as_ref())
                .map(|conversation| {
                    conversation
                        .comments
                        .iter()
                        .filter(|comment| {
                            comment.review_state.is_none()
                                || super::conversation::visible_body(&comment.body).is_some()
                        })
                        .count()
                });
        let segments = [
            ("files", self.files_label(cx), Tab::Files),
            (
                "conversation",
                count_label(
                    "pull_requests.detail.conversation",
                    "pull_requests.detail.conversation_count",
                    conversation_count.map(|count| count as u64),
                ),
                Tab::Conversation,
            ),
        ]
        .into_iter()
        .map(|(id, label, segment)| {
            TabItem::new(SharedString::from(format!("pr-view-{id}")), label)
                .selected(tab == segment)
                .on_click({
                    let view = cx.entity();
                    move |_, _, cx| view.update(cx, |view, cx| view.select_tab(segment, cx))
                })
        })
        .collect::<Vec<_>>();
        let stats = (tab == Tab::Files)
            .then(|| link.as_ref().and_then(|link| link.snapshot.clone()))
            .flatten()
            .and_then(|snapshot| snapshot.additions.zip(snapshot.deletions))
            .map(|(additions, deletions)| {
                h_flex()
                    .flex_none()
                    .gap_1()
                    .text_meta_mono(compact, cx)
                    .child(
                        div()
                            .text_color(cx.theme().success)
                            .child(format!("+{additions}")),
                    )
                    .child(
                        div()
                            .text_color(cx.theme().danger)
                            .child(format!("−{deletions}")),
                    )
            });
        let switch = div().flex_1().min_w_0().child(
            TabBar::new("pr-view")
                .segmented()
                .compact(compact)
                .items(segments),
        );
        if compact {
            // A touch segmented track is taller than a touch bar, so this row takes its content's
            // height at the page inset.
            let spacing = &cx.theme().tokens.spacing;
            h_flex()
                .flex_none()
                .py(spacing.sm)
                .px(px(material::COMPACT_PAGE_INSET))
                .gap(spacing.sm)
                .child(switch)
                .children(stats)
                .into_any_element()
        } else {
            material::bar("pr-view-bar", material::Bar::Toolbar, false, cx)
                .items_center()
                .child(switch)
                .children(stats)
                .into_any_element()
        }
    }

    /// Host conditions above the switch: a credential to connect, a rate limit, an unlink.
    fn notices(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let mut notices = Vec::new();
        let errors = self.page().map(|page| {
            [
                &page.files.error,
                &page.conversation.error,
                &page.viewed.error,
            ]
            .into_iter()
            .flatten()
            .map(|error| error.code.clone())
            .collect::<Vec<_>>()
        });
        let errors = errors.unwrap_or_default();
        let inset = if self.compact(cx) {
            px(material::COMPACT_PAGE_INSET)
        } else {
            cx.theme().tokens.spacing.sm
        };
        let notice = |text: String, action: Option<AnyElement>, cx: &App| {
            material::card(cx)
                .flex_row()
                .items_center()
                .mx(inset)
                .text_token(&cx.theme().tokens.typography.xs)
                .child(div().flex_1().min_w_0().child(text))
                .children(action)
                .into_any_element()
        };
        if errors.iter().any(|code| {
            code == "pull_request_no_credential" || code == "pull_request_host_disabled"
        }) {
            notices.push(notice(
                crate::tr!(
                    "pull_requests.notice_no_credential",
                    host = self
                        .current
                        .as_ref()
                        .map(|(_, key)| key.host.clone())
                        .unwrap_or_default()
                )
                .into_owned(),
                Some(
                    Button::new("pr-detail-settings")
                        .ghost()
                        .xsmall()
                        .label(crate::tr!("pull_requests.open_settings"))
                        .on_click(|_, window, cx| {
                            window.dispatch_action(Box::new(super::OpenSourceControl), cx)
                        })
                        .into_any_element(),
                ),
                cx,
            ));
        }
        if errors
            .iter()
            .any(|code| code == "pull_request_rate_limited")
        {
            notices.push(notice(
                crate::tr!(
                    "pull_requests.notice_rate_limited",
                    host_name = self.host_name(cx),
                    when = self
                        .link(cx)
                        .and_then(|link| match link.sync_error {
                            Some(tcode_core::pull_request::PullRequestSyncError::RateLimited {
                                retry_at,
                            }) => Some(super::resumes(retry_at)),
                            _ => None,
                        })
                        .unwrap_or_else(|| crate::tr!("pull_requests.notice_soon").into_owned())
                )
                .into_owned(),
                None,
                cx,
            ));
        }
        let unlinked = self
            .link(cx)
            .is_some_and(|link| !link.visible() && link.source != PullRequestSource::Stack)
            || (self.link(cx).is_none() && self.stack(cx).is_none());
        if unlinked
            && let Some((url, key)) = self
                .url(cx)
                .zip(self.current.as_ref().map(|(_, key)| key.clone()))
        {
            notices.push(notice(
                crate::tr!("pull_requests.detail.no_longer_linked").into_owned(),
                Some(
                    Button::new("pr-detail-relink")
                        .ghost()
                        .xsmall()
                        .label(crate::tr!("pull_requests.relink"))
                        .on_click(move |_, window, cx| {
                            window.dispatch_action(
                                Box::new(ChangeLink {
                                    key: key.clone(),
                                    url: url.clone(),
                                    linking: true,
                                }),
                                cx,
                            )
                        })
                        .into_any_element(),
                ),
                cx,
            ));
        }
        // A refresh that failed while content stays on screen.
        let stale = self.page().and_then(|page| {
            let files = page
                .files
                .data
                .as_ref()
                .and(page.files.error.as_ref())
                .map(|error| (error.code.clone(), page.files.loaded_at));
            let conversation = page
                .conversation
                .data
                .as_ref()
                .and(page.conversation.error.as_ref())
                .map(|error| (error.code.clone(), page.conversation.loaded_at));
            files
                .or(conversation)
                .filter(|(code, _)| code != "pull_request_rate_limited")
        });
        if let Some((_, loaded_at)) = stale {
            notices.push(notice(
                crate::tr!("pull_requests.detail.refresh_failed", ago = ago(loaded_at))
                    .into_owned(),
                Some(
                    Button::new("pr-detail-retry")
                        .ghost()
                        .xsmall()
                        .label(crate::tr!("pull_requests.detail.retry"))
                        .on_click(cx.listener(|this, _, _, cx| this.retry(cx)))
                        .into_any_element(),
                ),
                cx,
            ));
        }
        notices
    }

    /// The body when nothing could be read at all.
    pub(super) fn failure(
        &self,
        error: &ProtocolError,
        title: String,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some((_, key)) = self.current.clone() else {
            return div().into_any_element();
        };
        match error.code.as_str() {
            "pull_request_no_credential" | "pull_request_host_disabled" => material::empty_state(
                Icon::new(IconName::Lock),
                crate::tr!(
                    "pull_requests.detail.no_credential_title",
                    host_name = self.host_name(cx)
                )
                .into_owned(),
                String::new(),
                cx,
            )
            .into_any_element(),
            "pull_request_not_found" => {
                let url = self.url(cx);
                material::empty_state(
                    Icon::new(IconName::GitPullRequest),
                    crate::tr!(
                        "pull_requests.detail.unavailable_title",
                        number = key.number.to_string()
                    )
                    .into_owned(),
                    crate::tr!("pull_requests.detail.unavailable_desc").into_owned(),
                    cx,
                )
                .child(
                    h_flex()
                        .gap_2()
                        .child(
                            Button::new("pr-unavailable-retry")
                                .outline()
                                .small()
                                .label(crate::tr!("pull_requests.detail.retry"))
                                .on_click(cx.listener(|this, _, _, cx| this.retry(cx))),
                        )
                        .children(url.map(|url| {
                            Button::new("pr-unavailable-open")
                                .ghost()
                                .small()
                                .label(crate::tr!(
                                    "pull_requests.open_on_host",
                                    host_name = self.host_name(cx)
                                ))
                                .on_click(move |_, _, cx| cx.open_url(&url))
                        })),
                )
                .into_any_element()
            }
            _ => div()
                .p(cx.theme().tokens.spacing.md)
                .child(
                    material::notice(cx.theme().danger, cx)
                        .flex_col()
                        .items_start()
                        .child(div().text_title(cx).child(title))
                        .child(
                            div()
                                .id("pr-failure-reason")
                                .text_secondary(self.compact(cx), cx)
                                .child(reason(error))
                                .tooltip({
                                    let full = error.message.clone();
                                    move |window, cx| Tooltip::new(full.clone()).build(window, cx)
                                }),
                        )
                        .child(
                            div().child(
                                Button::new("pr-failure-retry")
                                    .outline()
                                    .xsmall()
                                    .label(crate::tr!("pull_requests.detail.retry"))
                                    .on_click(cx.listener(|this, _, _, cx| this.retry(cx))),
                            ),
                        ),
                )
                .into_any_element(),
        }
    }

    fn author_avatar(&self, login: &str) -> Option<String> {
        let conversation = self.page()?.conversation.data.as_ref()?;
        std::iter::once(&conversation.description)
            .chain(&conversation.comments)
            .filter_map(|comment| comment.author.as_ref())
            .find(|author| author.login == login)?
            .avatar_url
            .clone()
    }

    pub(super) fn files_of(&self) -> Option<&[PullRequestFile]> {
        self.page()
            .and_then(|page| page.files.data.as_ref())
            .map(|files| files.files.as_slice())
    }
}

impl Render for PullRequestView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.current.is_none() {
            return div().into_any_element();
        }
        self.ensure_reads(cx);
        let compact = self.compact(cx);
        let tab = self.tab(cx);
        let body = match tab {
            Tab::Files => self.render_files(window, cx),
            Tab::Conversation => self.render_conversation(window, cx),
        };
        v_flex()
            .size_full()
            .min_w_0()
            .on_action(cx.listener(Self::change_link))
            .on_action(cx.listener(Self::change_watch))
            .on_action(cx.listener(Self::on_selection_menu))
            .on_action(cx.listener(Self::on_comment_menu))
            .on_action(cx.listener(Self::run_lifecycle))
            .on_action(cx.listener(Self::choose_merge_method))
            .when(!compact, |view| view.child(self.sub_bar(cx)))
            .child(self.header(window, cx))
            .when(compact, |view| {
                view.children(self.primary(true, cx).map(|primary| {
                    div()
                        .flex_none()
                        .px(px(material::COMPACT_PAGE_INSET))
                        .pb_2()
                        .child(primary)
                }))
            })
            .children(self.notices(cx))
            .child(self.switch(cx))
            .child(div().flex_1().min_h_0().flex().flex_col().child(body))
            .into_any_element()
    }
}

fn count_label(plain: &str, counted: &str, count: Option<u64>) -> String {
    match count {
        Some(count) => crate::tr!(counted, count = count.to_string()).into_owned(),
        None => crate::tr!(plain).into_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{TestAppContext, VisualTestContext};
    use tcode_protocol::{
        ClientPayload, HostMessage, PullRequestCapabilities, PullRequestComment, PullRequestPatch,
        Query, QueryResponse, decode_client_line, encode_line,
    };

    const HEAD: &str = "2222222222222222222222222222222222222222";

    fn files(range: std::ops::Range<usize>, next_cursor: Option<&str>) -> PullRequestFiles {
        PullRequestFiles {
            base: "1111111111111111111111111111111111111111".into(),
            head: HEAD.into(),
            files: range
                .map(|index| PullRequestFile {
                    path: format!("src/{index}.rs"),
                    previous_path: None,
                    kind: agent::FileChangeKind::Modify,
                    additions: 1,
                    deletions: 0,
                    patch: PullRequestPatch::Hunks("@@ -1 +1,2 @@\n a\n+b\n".into()),
                })
                .collect(),
            next_cursor: next_cursor.map(str::to_owned),
            complete: next_cursor.is_none(),
            changed_files: 150,
        }
    }

    /// The pull request reads the view has asked for and the host has not answered.
    fn reads(requests: &async_channel::Receiver<String>) -> Vec<(u64, PullRequestRead)> {
        std::iter::from_fn(|| requests.try_recv().ok())
            .map(|line| decode_client_line(&line).unwrap())
            .filter_map(|request| match request.payload {
                ClientPayload::Query(Query::PullRequest { read, .. }) => Some((request.id, read)),
                _ => None,
            })
            .collect()
    }

    fn answer(
        incoming: &async_channel::Sender<String>,
        id: u64,
        response: PullRequestReadResponse,
        expires_at: u64,
    ) {
        incoming
            .try_send(
                encode_line(&HostMessage::QueryResult {
                    id,
                    result: Ok(QueryResponse::PullRequest {
                        response: Box::new(response),
                        expires_at,
                    }),
                })
                .unwrap(),
            )
            .unwrap();
    }

    fn settle(cx: &mut VisualTestContext) {
        for _ in 0..3 {
            cx.run_until_parked();
            cx.update(|window, cx| _ = window.draw(cx));
        }
    }

    struct Mounted<'a> {
        view: Entity<PullRequestView>,
        requests: async_channel::Receiver<String>,
        incoming: async_channel::Sender<String>,
        _pump: gpui::Task<()>,
        cx: &'a mut VisualTestContext,
    }

    struct PhonePage(Entity<PullRequestView>);
    impl Render for PhonePage {
        fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            let actions = self.0.update(cx, |view, cx| view.nav_actions(cx));
            v_flex()
                .size_full()
                .child(
                    h_flex().children(actions.into_iter().enumerate().map(|(index, action)| {
                        div()
                            .debug_selector(move || format!("phone-action-{index}"))
                            .child(action)
                    })),
                )
                .child(div().flex_1().child(self.0.clone()))
        }
    }

    #[gpui::test]
    fn the_phone_navigation_menu_delivers_its_link_action(cx: &mut TestAppContext) {
        use gpui::VisualContext as _;
        let Mounted {
            view,
            requests,
            incoming,
            _pump,
            cx,
        } = mount(cx);
        let store = view.read_with(cx, |view, _| view.store.clone());
        let mut meta = tcode_core::project::SessionMeta::new(
            agent::ProviderKind::Codex,
            "/tmp/tcode-pr".into(),
            None,
        );
        meta.id = "session".into();
        meta.pull_requests = vec![ThreadPullRequestLink {
            key: PullRequestKey::new("github.com", "octo/repo", 7),
            url: "https://github.com/octo/repo/pull/7".into(),
            source: PullRequestSource::Dismissed,
            linked_at: None,
            snapshot: None,
            stack: Default::default(),
            sync_error: None,
            watch: None,
        }];
        incoming
            .try_send(
                encode_line(&HostMessage::Event(tcode_protocol::EventEnvelope {
                    request_id: None,
                    topic: tcode_protocol::Topic::Index,
                    event: tcode_protocol::ServerEvent::IndexSnapshot(
                        tcode_protocol::IndexSnapshot {
                            summary: Default::default(),
                            sessions: vec![meta],
                            projects: Vec::new(),
                        },
                    ),
                }))
                .unwrap(),
            )
            .unwrap();
        settle(cx);
        store.update(cx, |store, cx| store.drain_host_events_for_test(cx));
        store.update(cx, |store, _| store.select_session("session".into()));
        view.update(cx, |view, cx| {
            view.window_state
                .update(cx, |state, cx| state.set_compact(true, cx))
        });
        cx.replace_root_view(|_, _| PhonePage(view.clone()));
        cx.simulate_resize(gpui::size(px(393.), px(852.)));
        settle(cx);
        let touch = |cx: &mut VisualTestContext, phase, position| {
            cx.simulate_event(gpui::TouchEvent {
                id: gpui::TouchId(1),
                phase,
                position,
                predicted_position: None,
                force: None,
            });
            settle(cx);
        };
        let more = cx.debug_bounds("phone-action-1").unwrap().center();
        touch(cx, gpui::TouchPhase::Started, more);
        touch(cx, gpui::TouchPhase::Ended, more);
        let link = cx
            .debug_bounds("menu-item-3")
            .expect("link menu item")
            .center();
        touch(cx, gpui::TouchPhase::Started, link);
        touch(cx, gpui::TouchPhase::Ended, link);
        let commands: Vec<_> = std::iter::from_fn(|| requests.try_recv().ok())
            .map(|line| decode_client_line(&line).unwrap().payload)
            .filter_map(|payload| match payload {
                ClientPayload::Command(command) => Some(command),
                _ => None,
            })
            .collect();
        assert!(commands.iter().any(|command| matches!(command, Command::LinkPullRequest { session_id, .. } if session_id == "session")), "phone menu must send a link command: {commands:?}");
    }

    /// A wide view showing octo/repo#7, its first reads asked for and unanswered.
    fn mount(cx: &mut TestAppContext) -> Mounted<'_> {
        cx.update(crate::theme::init);
        cx.update(crate::markdown::init);
        let (to_host, requests) = async_channel::unbounded();
        let (incoming, from_host) = async_channel::unbounded();
        let link = tcode_client::HostLink::new(to_host, from_host);
        let pump = link.clone();
        let executor = cx.background_executor.clone();
        let pump = cx.background_executor.spawn(async move {
            pump.pump_with_timer(|| executor.timer(std::time::Duration::from_millis(25)))
                .await;
        });
        let store = cx.new(|cx| {
            WorkspaceStore::new_attached(
                link,
                crate::store::WorkspaceAttachment::Local,
                None,
                None,
                false,
                cx,
            )
        });
        crate::store::tests::seed_full_scope(&store, &incoming, Vec::new(), cx);
        let window_state = cx.new(|_| WindowState::new(false));
        let (view, cx) = cx
            .add_window_view(|_, cx| PullRequestView::new(store.clone(), window_state.clone(), cx));
        cx.simulate_resize(gpui::size(px(1200.), px(800.)));
        view.update(cx, |view, cx| {
            view.show(
                "session".into(),
                PullRequestKey::new("github.com", "octo/repo", 7),
                cx,
            )
        });
        settle(cx);
        Mounted {
            view,
            requests,
            incoming,
            _pump: pump,
            cx,
        }
    }

    /// An empty conversation on a host that offers everything.
    fn conversation() -> PullRequestConversation {
        PullRequestConversation {
            description: PullRequestComment {
                id: "PR_7".into(),
                author: None,
                body: String::new(),
                created_at: "2026-10-01T00:00:00Z".into(),
                edited_at: None,
                url: None,
                review_state: None,
                reactions: Vec::new(),
                viewer_can_update: false,
                viewer_can_react: false,
            },
            comments: Vec::new(),
            threads: Vec::new(),
            complete: true,
            account: "account".into(),
            permissions: Default::default(),
            labels: Vec::new(),
            reviewers: Vec::new(),
            capabilities: PullRequestCapabilities::ALL,
        }
    }

    /// Answers a read other than a page of files, valid until `expires_at`.
    fn answer_other(
        incoming: &async_channel::Sender<String>,
        id: u64,
        read: PullRequestRead,
        expires_at: u64,
    ) {
        let response = match read {
            PullRequestRead::Conversation => {
                PullRequestReadResponse::Conversation(Box::new(conversation()))
            }
            PullRequestRead::ViewedFiles => {
                PullRequestReadResponse::ViewedFiles(PullRequestViewedFiles {
                    files: Vec::new(),
                    complete: true,
                })
            }
            read => panic!("unexpected read {read:?}"),
        };
        answer(incoming, id, response, expires_at);
    }

    #[gpui::test]
    fn an_expiry_reread_keeps_the_pages_read_after_page_one_and_the_scroll(
        cx: &mut TestAppContext,
    ) {
        let Mounted {
            view,
            requests,
            incoming,
            _pump,
            cx,
        } = mount(cx);
        let later = now() + 3600;
        let mut reread = None;
        for (id, read) in reads(&requests) {
            match read {
                // Page 1 lands as its expiry passes, so the view reads it again at once.
                PullRequestRead::Files { cursor: None } => answer(
                    &incoming,
                    id,
                    PullRequestReadResponse::Files(files(0..100, Some("2"))),
                    now(),
                ),
                read => answer_other(&incoming, id, read, later),
            }
        }
        settle(cx);
        for (id, read) in reads(&requests) {
            assert_eq!(read, PullRequestRead::Files { cursor: None });
            reread = Some(id);
        }
        let reread = reread.expect("an expired page 1 is read again");

        let scroll_to = |cx: &mut VisualTestContext, file: usize| {
            view.update(cx, |view, cx| {
                view.page()
                    .unwrap()
                    .files_view
                    .list
                    .as_ref()
                    .unwrap()
                    .scroll_to_file(file);
                cx.notify();
            });
            settle(cx);
        };
        scroll_to(cx, 95);
        let page_two = reads(&requests);
        assert_eq!(
            page_two.iter().map(|(_, read)| read).collect::<Vec<_>>(),
            [&PullRequestRead::Files {
                cursor: Some("2".into())
            }],
            "nearing the end of page 1 reads page 2"
        );
        view.update(cx, |view, cx| {
            view.page()
                .unwrap()
                .files_view
                .list
                .as_ref()
                .unwrap()
                .unified_list
                .scroll_by(px(7.));
            cx.notify();
        });
        settle(cx);
        let before_append = view.read_with(cx, |view, _| {
            view.page()
                .unwrap()
                .files_view
                .list
                .as_ref()
                .unwrap()
                .unified_list
                .logical_scroll_top()
        });
        answer(
            &incoming,
            page_two[0].0,
            PullRequestReadResponse::Files(files(100..150, None)),
            later,
        );
        settle(cx);
        let after_append = view.read_with(cx, |view, _| {
            view.page()
                .unwrap()
                .files_view
                .list
                .as_ref()
                .unwrap()
                .unified_list
                .logical_scroll_top()
        });
        assert_eq!(
            after_append.item_ix, before_append.item_ix,
            "paging keeps the same source row"
        );
        assert_eq!(
            after_append.offset_in_item, before_append.offset_in_item,
            "paging keeps the exact offset within the row"
        );
        scroll_to(cx, 120);

        answer(
            &incoming,
            reread,
            PullRequestReadResponse::Files(files(0..100, Some("2"))),
            later,
        );
        settle(cx);
        view.update(cx, |view, _| {
            let page = view.page().unwrap();
            let held = page.files.data.as_ref().unwrap();
            assert_eq!(
                (held.files.len(), held.next_cursor.as_deref(), held.complete),
                (150, None, true),
                "the re-read of page 1 keeps the pages read after it"
            );
            assert_eq!(
                page.files_view.list.as_ref().unwrap().top_file(false),
                Some(120),
                "and the reader's place in them"
            );
        });
        assert!(reads(&requests).is_empty());
    }

    /// The meta block has a Labels row where the host has labels: on one without them
    /// (Bitbucket) there is none, not even an empty one, beside the reviewers, unless the host
    /// still reports labels on the pull request.
    #[gpui::test]
    fn the_labels_row_is_absent_where_the_host_has_no_labels(cx: &mut TestAppContext) {
        for (labels, applied, shown) in [
            (true, false, true),
            (false, false, false),
            (false, true, true),
        ] {
            let Mounted {
                view,
                requests,
                incoming,
                _pump,
                cx,
            } = mount(cx);
            let later = now() + 3600;
            for (id, read) in reads(&requests) {
                let response = match read {
                    PullRequestRead::Conversation => {
                        PullRequestReadResponse::Conversation(Box::new(PullRequestConversation {
                            reviewers: vec![tcode_protocol::PullRequestReviewerState {
                                reviewer: tcode_protocol::PullRequestReviewer {
                                    id: "reviewer".into(),
                                    login: "reviewer".into(),
                                    kind: tcode_protocol::PullRequestReviewerKind::User,
                                },
                                avatar_url: None,
                                verdict: None,
                            }],
                            labels: applied
                                .then(|| tcode_protocol::PullRequestLabel {
                                    id: "bug".into(),
                                    name: "bug".into(),
                                    color: None,
                                    description: None,
                                })
                                .into_iter()
                                .collect(),
                            capabilities: PullRequestCapabilities {
                                labels,
                                ..PullRequestCapabilities::ALL
                            },
                            ..conversation()
                        }))
                    }
                    PullRequestRead::Files { cursor: None } => {
                        PullRequestReadResponse::Files(files(0..1, None))
                    }
                    read => {
                        answer_other(&incoming, id, read, later);
                        continue;
                    }
                };
                answer(&incoming, id, response, later);
            }
            settle(cx);
            view.update(cx, |view, cx| view.select_tab(Tab::Conversation, cx));
            settle(cx);
            assert!(cx.debug_bounds("pr-meta-reviewers").is_some());
            assert_eq!(
                cx.debug_bounds("pr-meta-labels").is_some(),
                shown,
                "labels offered: {labels}, applied: {applied}"
            );
        }
    }

    /// A host that gives no count of changed files (Bitbucket, Azure DevOps) shows the Files tab
    /// without a number until the files read answers with one, never "Files 0".
    #[gpui::test]
    fn a_snapshot_without_a_file_count_labels_the_files_tab_without_one(cx: &mut TestAppContext) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        let Mounted {
            view,
            requests,
            incoming,
            _pump,
            cx,
        } = mount(cx);
        let mut meta = tcode_core::project::SessionMeta::new(
            agent::ProviderKind::ClaudeCode,
            "/tmp/tcode-pr".into(),
            None,
        );
        meta.id = "session".into();
        meta.pull_requests = vec![ThreadPullRequestLink {
            key: PullRequestKey::new("github.com", "octo/repo", 7),
            url: "https://github.com/octo/repo/pull/7".into(),
            source: PullRequestSource::Manual,
            linked_at: None,
            snapshot: Some(tcode_core::pull_request::PullRequestSnapshot {
                state: PullRequestState::Open,
                title: "Change".into(),
                head_branch: "feature".into(),
                base_branch: "main".into(),
                is_draft: false,
                updated_at: "2026-10-01T00:00:00Z".into(),
                synced_at: 1,
                closed_at: None,
                merged_at: None,
                author: None,
                additions: None,
                deletions: None,
                changed_files: None,
                review_decision: None,
                checks_state: None,
                mergeability: Mergeability::Unknown,
            }),
            stack: Default::default(),
            sync_error: None,
            watch: None,
        }];
        incoming
            .try_send(
                encode_line(&HostMessage::Event(tcode_protocol::EventEnvelope {
                    request_id: None,
                    topic: tcode_protocol::Topic::Index,
                    event: tcode_protocol::ServerEvent::IndexSnapshot(
                        tcode_protocol::IndexSnapshot {
                            summary: Default::default(),
                            sessions: vec![meta],
                            projects: Vec::new(),
                        },
                    ),
                }))
                .unwrap(),
            )
            .unwrap();
        settle(cx);
        let store = view.read_with(cx, |view, _| view.store.clone());
        store.update(cx, |store, cx| store.drain_host_events_for_test(cx));
        settle(cx);
        let label =
            |cx: &mut VisualTestContext| view.read_with(cx, |view, cx| view.files_label(cx));
        assert_eq!(label(cx), "Files");

        let later = now() + 3600;
        let (id, _) = reads(&requests)
            .into_iter()
            .find(|(_, read)| *read == PullRequestRead::Files { cursor: None })
            .expect("the first page of files is read");
        answer(
            &incoming,
            id,
            PullRequestReadResponse::Files(files(0..3, None)),
            later,
        );
        settle(cx);
        assert_eq!(
            label(cx),
            "Files 150",
            "the files read's count once it answers"
        );
    }

    #[gpui::test]
    fn switching_language_relocalizes_the_file_filter_placeholder(cx: &mut TestAppContext) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        crate::settings::apply_locale(Some(crate::LANGUAGE_SIMPLIFIED_CHINESE));
        let Mounted {
            view,
            requests,
            incoming,
            _pump,
            cx,
        } = mount(cx);
        let later = now() + 3600;
        for (id, read) in reads(&requests) {
            match read {
                PullRequestRead::Files { cursor: None } => answer(
                    &incoming,
                    id,
                    PullRequestReadResponse::Files(files(0..3, None)),
                    later,
                ),
                read => answer_other(&incoming, id, read, later),
            }
        }
        settle(cx);
        crate::settings::assert_follows_language_switch(cx, "pull_requests.files.filter", |cx| {
            view.read_with(cx, |view, cx| {
                let (filter, _) = view.file_filter.as_ref().expect("the file column");
                filter.read(cx).presentation().placeholder().to_string()
            })
        });
    }
}
