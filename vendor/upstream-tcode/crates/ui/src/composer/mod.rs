//! The floating composer card: input, control row (model picker + context +
//! permission/mode chips + send/stop), the below-card checkout/branch row, and
//! the pending-approval panel.

mod components;
pub(crate) mod model;

use components::images::PendingImage;
use components::steer_gesture::SteerGesture;
#[cfg(feature = "voice")]
use components::voice::Voice;
use model::*;

use std::borrow::Cow;
use std::cell::Cell;
use std::rc::Rc;

use std::path::PathBuf;
use std::time::Duration;

use crate::overlay::{DialogButtons, Notification, OverlayExt as _};
use crate::theme::ActiveTheme as _;
use crate::typography::TextRole as _;
use crate::widgets::button::{Button, ButtonVariants as _};
use crate::widgets::input::{
    BrowserPaste, Input, InputEvent, InputState, Paste, Textarea, TextareaState,
};
use crate::widgets::menu::{ContextMenuExt as _, CopyText};
use crate::widgets::spinner::Spinner;
use crate::{
    icon::{Icon, IconName},
    sizing::Sizable as _,
};
use agent::{
    ApprovalDecision, ApprovalKind, ApprovalOptionKind, ApprovalRequest, ModelSpec,
    OptionDescriptor, ProviderCommandKind, ProviderKind, TokenUsage, UserInputQuestion,
};
use chrono::Local;
use gpui::{
    Anchor, Animation, AnimationExt as _, AnyElement, App, AppContext as _, ClipboardEntry,
    Context, Entity, EventEmitter, ExternalPaths, Focusable as _, Hsla, InteractiveElement as _,
    IntoElement, ParentElement as _, Render, Role, StatefulInteractiveElement as _, Styled as _,
    Subscription, Task, Window, div, img, prelude::FluentBuilder as _, px, rgb,
};
use gpui_base::PopoverState;
use gpui_base::input::{MoveDown, MoveUp};

pub(crate) const CONTEXT: &str = "Composer";

/// A context-menu action on the composer's chips, queue rows and pending
/// images, which the composer applies as their buttons do.
#[derive(gpui::Action, Clone, PartialEq, Eq, serde::Deserialize)]
#[action(namespace = tcode_composer, no_json)]
enum ComposerMenu {
    RemoveTerminalContext(u64),
    RemoveReviewComment(usize),
    QueueSteer(u64),
    QueueEdit { id: u64, text: String },
    QueueDrop(u64),
    ImageOpen(usize),
    ImageRemove(usize),
}
use gpui_base::{ElementExt as _, h_flex, v_flex};

use crate::attachments::attach_error_message;
use crate::composer_trigger::{
    ComposerTrigger, TriggerKind, detect_composer_trigger, serialize_composer_file_link,
};
use crate::context_meter;
use crate::palette::fuzzy_score;
use crate::provider_card::provider_glyph;
use crate::settings::provider_label;
use crate::shortcut::format_secondary_shortcut;
use crate::store::{TopicKind, WorkspaceStore, observe_store_topics};
use crate::workspace_walk::filter_entries;
use tcode_core::attachments::{mime_from_path, validate_attachment};
use tcode_core::provider_models::{FastLevel, FastMode};
use tcode_core::ui::WorkspaceMode;
use tcode_protocol::PathEntry;

/// Context-meter colors, shared with provider usage bars.
use crate::usage::{METER_BLUE, METER_RED};
/// File mentions are potentially unbounded; command and skill feeds are not
/// capped and instead use the trigger menu's scrolling viewport.
const FILE_MENU_ROW_CAP: usize = 50;
/// Matching threads an `@` menu offers above the files.
const THREAD_MENU_ROW_CAP: usize = 5;

/// The desktop send and stop square: as tall as the control row's chips
/// ([`Button::chip`]), which carry no square form.
const SEND_SIZE: f32 = 28.;
/// Below this measured control-row width the row collapses its context /
/// permission / mode chips into a "⋯" overflow popover so nothing spills past
/// the card edge (diff panel open, or a small window).
const CONTROL_ROW_COMPACT_BELOW: f32 = 520.;

/// Which rail filter the model picker is showing.
#[derive(Clone, PartialEq, Eq)]
enum PickerRail {
    Favorites,
    /// One provider profile (by profile id) — a built-in (`"claude"`/`"codex"`)
    /// or a user-created third-party profile. Each profile is its own rail entry
    /// and lists only its own models.
    Profile(String),
    /// One installed ACP agent (by registry id). ACP agents have no model
    /// catalog — the agent publishes its models over the wire once the session
    /// is up — so this rail lists the agent itself.
    Acp(String),
}

pub enum ComposerEvent {
    /// A turn was just submitted (chat view scrolls to the bottom).
    Submitted,
}

/// How far the draft field may grow before it scrolls. A compact window has to
/// leave room for the keyboard, so it starts taller and stops sooner.
fn auto_grow_rows(compact: bool) -> (usize, usize) {
    if compact { (2, 5) } else { (1, 8) }
}

fn draft_placeholder(compact: bool) -> Cow<'static, str> {
    if compact {
        crate::tr!("mobile.message")
    } else {
        crate::tr!("composer.placeholder")
    }
}

pub struct Composer {
    compact: bool,
    workspace_store: Entity<WorkspaceStore>,
    input: Entity<TextareaState>,
    /// Dedicated free-form answer field shown inside an agent question card.
    /// Keeping it separate from the turn composer makes the pending question
    /// and the destination of typed text unambiguous.
    user_input_custom: Entity<TextareaState>,
    /// The editable clarification draft suggested by the fallback reviewer, plus
    /// the draft last seeded into it — the field is only re-seeded when the
    /// suggestion itself changes, so the user's edits survive re-renders.
    fallback_review_input: Entity<TextareaState>,
    fallback_review_seeded: Option<String>,
    /// Unsent text is isolated by persisted thread or project New thread page.
    input_destination: Option<ComposerDestination>,
    input_cache: std::collections::HashMap<ComposerDestination, Entity<TextareaState>>,
    input_subscription: Subscription,
    model_search: Entity<InputState>,
    /// `None` = follow the active session's provider (set on first open).
    picker_rail: Option<PickerRail>,
    /// Whether the approval panel's detail is expanded.
    approval_expanded: bool,
    /// The user-input request currently being answered (its id), plus the
    /// question index and per-question selected option labels. Reset when a new
    /// request arrives or it resolves.
    ui_request_id: Option<String>,
    ui_question_index: usize,
    ui_selections: std::collections::HashMap<String, Vec<String>>,
    /// A non-blocking request the user closed on this client; its panel stays
    /// hidden until a newer request replaces it. The agent keeps working and
    /// the question text remains in the transcript.
    ui_dismissed_request_id: Option<String>,
    /// Whether the question card shows its answer area. A blocking request
    /// opens it; a non-blocking one arrives closed so it never takes the
    /// composer's place.
    ui_expanded: bool,
    /// Bumped by `/model` so the model-picker popover re-opens (a fresh popover
    /// instance, keyed by this token, starts open).
    model_picker_token: u64,
    /// Whether the control row was last laid out narrower than its controls
    /// fit, which folds them into the "⋯" overflow menu. Only layout knows
    /// the width, so its prepaint callback writes it here.
    control_row_narrow: Rc<Cell<bool>>,
    /// The inline trigger (`@`/`/`/`$`) active at the cursor, recomputed on every
    /// input change. Drives the trigger menu.
    active_trigger: Option<ComposerTrigger>,
    /// Highlighted row index within the open trigger menu (arrows + hover).
    menu_highlight: usize,
    /// The trigger identity the menu was last shown for; when it changes the
    /// highlight resets and any Escape-dismissal clears.
    menu_last_key: Option<String>,
    /// Set when Escape dismissed the menu (until the query changes).
    menu_dismissed: bool,
    /// Cached workspace listing for the active session cwd (for `@`-mentions),
    /// loaded lazily in the background the first time a mention trigger opens.
    workspace: Option<(PathBuf, Vec<PathEntry>)>,
    workspace_loading: bool,
    /// Pending image attachments for the active session, validated + persisted to
    /// disk. Cleared on send and whenever the active session changes.
    pending_images: Vec<PendingImage>,
    /// The session id `pending_images` belongs to (reset the strip on switch).
    images_session: Option<String>,
    /// Invalidates image jobs when the owning session changes or a turn sends.
    image_load_generation: u64,
    /// Reserved strip slots for image jobs that have not completed yet.
    pending_image_loads: usize,
    /// One cancellable one-second repaint loop, present only while the queue
    /// strip contains at least one scheduled row.
    scheduled_countdown_tick: Option<Task<()>>,
    queued_refill: Option<(String, u64, String)>,
    queue_drag: Option<components::queue_strip::QueueDrag>,
    queue_scroll: gpui::ScrollHandle,
    /// A touch on the compact Queue button that has not lifted yet.
    steer_gesture: Option<SteerGesture>,
    /// Mic button + live dictation session (see `components::voice`).
    #[cfg(feature = "voice")]
    voice: Voice,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<ComposerEvent> for Composer {}

impl Composer {
    fn interactive(&self, cx: &App) -> bool {
        !self.workspace_store.read(cx).native_subagent_readonly()
            && !matches!(self.workspace_store.read(cx).connection_state(),
            tcode_client::ConnectionState::Offline { reason } if reason.is_terminal())
    }

    pub fn focus(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.interactive(cx) && !crate::window_seam::is_mobile(cx) {
            self.input.update(cx, |input, cx| input.focus(window, cx));
        }
    }

    #[cfg(test)]
    pub(crate) fn input_focus_handle(&self, cx: &App) -> gpui::FocusHandle {
        self.input.read(cx).focus_handle(cx)
    }

    #[cfg(test)]
    pub(crate) fn model_search_focus_handle(&self, cx: &App) -> gpui::FocusHandle {
        self.model_search.read(cx).focus_handle(cx)
    }

    #[cfg(test)]
    pub(crate) fn draft(&self, cx: &App) -> String {
        self.input.read(cx).value().to_string()
    }

    #[cfg(test)]
    pub(crate) fn set_draft(&mut self, text: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.input
            .update(cx, |input, cx| input.replace_all(text, window, cx));
    }

    #[cfg(test)]
    pub(crate) fn is_compact(&self) -> bool {
        self.compact
    }

    /// Follow the window onto the other layout. Updated in place on purpose: a
    /// rebuilt composer would drop the draft, its selection and its pending
    /// attachments, and the user only resized a window.
    pub fn set_compact(&mut self, compact: bool, cx: &mut Context<Self>) {
        if self.compact == compact {
            return;
        }
        self.compact = compact;
        let (min_rows, max_rows) = auto_grow_rows(compact);
        self.input
            .update(cx, |input, cx| input.set_auto_grow(min_rows, max_rows, cx));
        cx.notify();
    }
    pub fn new(
        workspace_store: Entity<WorkspaceStore>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        Self::new_with_layout(workspace_store, false, window, cx)
    }

    pub fn new_with_layout(
        workspace_store: Entity<WorkspaceStore>,
        compact: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let (min_rows, max_rows) = auto_grow_rows(compact);
        let input = cx.new(|cx| {
            TextareaState::new(window, cx)
                .autocorrect(true)
                .input_action(gpui::TextInputAction::Enter)
                .auto_grow(min_rows, max_rows)
                .submit_on_enter(true)
        });
        let model_search = cx.new(|cx| InputState::new(window, cx));
        let user_input_custom = cx.new(|cx| {
            TextareaState::new(window, cx)
                .input_action(gpui::TextInputAction::Enter)
                .rows(1)
                .submit_on_enter(true)
        });
        let fallback_review_input = cx.new(|cx| TextareaState::new(window, cx).auto_grow(1, 6));

        let input_subscription = cx.subscribe_in(&input, window, Self::on_input_event);
        let subscriptions = vec![
            // Re-render when app state changes (e.g. the provider's commands /
            // skills feed arrives after session start, feeding the `/`+`$` menus).
            observe_store_topics(
                &workspace_store,
                &[
                    TopicKind::ActiveSession,
                    TopicKind::Index,
                    TopicKind::SessionStatus,
                    TopicKind::SessionPlan,
                    TopicKind::SessionEvents,
                    TopicKind::Settings,
                    TopicKind::Providers,
                ],
                cx,
            ),
            cx.subscribe_in(
                &user_input_custom,
                window,
                |this, input, event, window, cx| match event {
                    InputEvent::PressEnter { shift: false, .. } => {
                        this.submit_custom_user_input(input, window, cx);
                    }
                    InputEvent::Change => cx.notify(),
                    _ => {}
                },
            ),
            cx.subscribe(&fallback_review_input, |_, _, event, cx| {
                if matches!(event, InputEvent::Change) {
                    cx.notify();
                }
            }),
            cx.subscribe(&model_search, |_, _, event, cx| {
                if matches!(event, InputEvent::Change) {
                    cx.notify();
                }
            }),
        ];

        #[cfg(feature = "voice")]
        let (voice, subscriptions) = {
            let mut subscriptions = subscriptions;
            let voice = Voice::new(window, cx, &mut subscriptions);
            (voice, subscriptions)
        };

        Self {
            compact,
            workspace_store,
            input,
            user_input_custom,
            fallback_review_input,
            fallback_review_seeded: None,
            input_destination: None,
            input_cache: Default::default(),
            input_subscription,
            model_search,
            picker_rail: None,
            approval_expanded: true,
            ui_request_id: None,
            ui_question_index: 0,
            ui_selections: std::collections::HashMap::new(),
            ui_dismissed_request_id: None,
            ui_expanded: false,
            model_picker_token: 0,
            control_row_narrow: Rc::default(),
            active_trigger: None,
            menu_highlight: 0,
            menu_last_key: None,
            menu_dismissed: false,
            workspace: None,
            workspace_loading: false,
            pending_images: Vec::new(),
            images_session: None,
            image_load_generation: 0,
            pending_image_loads: 0,
            scheduled_countdown_tick: None,
            queued_refill: None,
            queue_drag: None,
            queue_scroll: gpui::ScrollHandle::new(),
            steer_gesture: None,
            #[cfg(feature = "voice")]
            voice,
            _subscriptions: subscriptions,
        }
    }

    fn on_input_event(
        &mut self,
        input: &Entity<TextareaState>,
        event: &InputEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            InputEvent::PressEnter {
                shift: false,
                secondary,
            } => {
                if self.menu_visible(cx) {
                    self.accept_menu(self.menu_highlight, window, cx);
                } else {
                    self.submit(input, *secondary, window, cx);
                }
            }
            InputEvent::Change => {
                self.queued_refill = None;
                // An edit that did not come from the transcript writer
                // ends dictation (see `components::voice`).
                #[cfg(feature = "voice")]
                self.stop_dictation_on_user_edit(cx);
                self.recompute_trigger(cx);
                cx.notify();
            }
            _ => {}
        }
    }

    fn sync_text_destination(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let destination = self
            .workspace_store
            .read(cx)
            .with_composer_destination(composer_destination)
            .flatten();
        if self.input_destination == destination {
            return;
        }
        #[cfg(feature = "voice")]
        self.abort_dictation(cx);
        self.input.update(cx, |input, cx| {
            crate::widgets::input::commit_marked_text(input, window, cx);
        });
        let focused = self.input.read(cx).focus_handle(cx).is_focused(window);
        if let Some(outgoing) = self.input_destination.take() {
            self.input_cache.insert(outgoing, self.input.clone());
        }
        let (min_rows, max_rows) = auto_grow_rows(self.compact);
        self.input = destination
            .as_ref()
            .and_then(|key| self.input_cache.remove(key))
            .unwrap_or_else(|| {
                cx.new(|cx| {
                    TextareaState::new(window, cx)
                        .autocorrect(true)
                        .input_action(gpui::TextInputAction::Enter)
                        .auto_grow(min_rows, max_rows)
                        .submit_on_enter(true)
                })
            });
        self.input_destination = destination;
        self.input_subscription = cx.subscribe_in(&self.input, window, Self::on_input_event);
        self.input.update(cx, |state, cx| {
            state.set_auto_grow(min_rows, max_rows, cx);
            if focused {
                state.focus(window, cx);
            }
        });
        self.recompute_trigger(cx);
    }

    /// Claude's native conversation rewind returns the selected user prompt.
    /// Put that provider-owned prefill into the ordinary composer so the user
    /// can edit or resend it; no inline transcript editor is involved.
    fn sync_native_rewind_prefill(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let prefill = self
            .workspace_store
            .update(cx, |store, _cx| store.take_native_rewind_prefill());
        let Some(prefill) = prefill else {
            return;
        };
        self.set_input_text(prefill, window, cx);
    }

    /// Text another surface put here goes after what the user typed, which it never replaces.
    fn sync_composer_append(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(text) = self
            .workspace_store
            .update(cx, |store, _cx| store.take_composer_append())
        else {
            return;
        };
        let typed = self.input.read(cx).value().trim_end().to_owned();
        let text = if typed.is_empty() {
            text
        } else {
            format!("{typed}\n\n{text}")
        };
        self.set_input_text(text, window, cx);
    }

    /// Replace the composer text with `text`, caret at the end.
    /// Software-keyboard devices wait for an explicit tap to focus.
    fn set_input_text(&mut self, text: String, window: &mut Window, cx: &mut Context<Self>) {
        let cursor = text.len();
        self.input.update(cx, |state, cx| {
            state.replace_all(text, window, cx);
            state.set_selected_range(cursor..cursor, cx);
            if !crate::window_seam::is_mobile(cx) {
                state.focus(window, cx);
            }
        });
        self.recompute_trigger(cx);
    }

    /// Remove a queued message and return its text to the composer for editing.
    /// Selecting the replacement lets the user discard it with one keystroke,
    /// while queued attachments are deliberately ignored.
    fn on_menu(&mut self, action: &ComposerMenu, window: &mut Window, cx: &mut Context<Self>) {
        match action.clone() {
            ComposerMenu::RemoveTerminalContext(id) => self
                .workspace_store
                .update(cx, |store, _cx| store.remove_terminal_context(id)),
            ComposerMenu::RemoveReviewComment(index) => self
                .workspace_store
                .update(cx, |store, _cx| store.remove_review_comment(index)),
            ComposerMenu::QueueSteer(id) => self
                .workspace_store
                .update(cx, |store, _cx| store.steer_queued(id)),
            ComposerMenu::QueueEdit { id, text } => {
                self.drop_queued_and_refill(id, text, window, cx)
            }
            ComposerMenu::QueueDrop(id) => self
                .workspace_store
                .update(cx, |store, _cx| store.drop_queued(id)),
            ComposerMenu::ImageOpen(index) => self.open_image_preview(index, window, cx),
            ComposerMenu::ImageRemove(index) => self.remove_image(index, cx),
        }
    }

    fn drop_queued_and_refill(
        &mut self,
        id: u64,
        text: String,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let store = self.workspace_store.read(cx);
        if !self.interactive(cx)
            || !store.composer_state().queue.is_some_and(|queue| {
                queue
                    .messages
                    .iter()
                    .any(|message| message.id == id && message.editable)
            })
        {
            return;
        }
        let Some(session_id) = store.active_session_id() else {
            return;
        };
        self.queued_refill = Some((session_id, id, text));
        self.workspace_store
            .update(cx, |store, _cx| store.drop_queued(id));
        cx.notify();
    }

    fn sync_queued_refill(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((session_id, id, _)) = &self.queued_refill else {
            return;
        };
        let store = self.workspace_store.read(cx);
        if store.active_session_id().as_ref() != Some(session_id) {
            self.queued_refill = None;
            return;
        }
        let Some(queue) = store.composer_state().queue else {
            return;
        };
        if let Some(message) = queue.messages.iter().find(|message| message.id == *id) {
            if !message.editable {
                self.queued_refill = None;
            }
            return;
        }
        let (_, _, text) = self.queued_refill.take().unwrap();
        let selection = 0..text.len();
        self.input.update(cx, |state, cx| {
            state.replace_all(text, window, cx);
            state.set_selected_range(selection, cx);
            if !crate::window_seam::is_mobile(cx) {
                state.focus(window, cx);
            }
        });
        self.recompute_trigger(cx);
    }

    fn has_sendable_content(&self, cx: &App) -> bool {
        !self.input.read(cx).value().trim().is_empty()
            || !self.pending_images.is_empty()
            || !self
                .workspace_store
                .read(cx)
                .composer_state()
                .terminal_contexts
                .is_empty()
            || !self.workspace_store.read(cx).review_comments().is_empty()
    }

    /// Send the composer's contents. `steer` is set by the ⌘/Ctrl+Enter gesture:
    /// inject into the running turn rather than queue behind it. It is a no-op
    /// when no turn is running (steering just sends), and degrades to
    /// queueing (with a notice) on providers that cannot steer.
    fn submit(
        &mut self,
        input: &Entity<TextareaState>,
        steer: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self
            .workspace_store
            .read(cx)
            .composer_state()
            .conversation_read_only
        {
            return;
        }
        if self.compact
            && !self
                .workspace_store
                .read(cx)
                .connection_state()
                .is_connected()
        {
            return;
        }
        #[cfg(feature = "voice")]
        if self.submit_after_dictation(steer, cx) {
            return;
        }
        // A blocking question uses this text as the current custom answer,
        // advancing through the same path as an option click. A non-blocking
        // one leaves the composer to ordinary sends and steers.
        if self
            .pending_user_input(cx)
            .is_some_and(|pending| pending.delivery.is_blocking())
        {
            self.submit_custom_user_input(input, window, cx);
            return;
        }
        let text = input.read(cx).value().trim().to_string();
        let composer_state = self.workspace_store.read(cx).composer_state();
        let compact_offered = compact_offered(&composer_state);
        let goal_offered = goal_offered(&composer_state);
        let current_goal = composer_state.goal.clone();
        let terminal_contexts = composer_state.terminal_contexts;
        if !self.has_sendable_content(cx) {
            return;
        }
        if !composer_state.has_active_session {
            window.push_notification(Notification::info(crate::tr!("composer.no_session")), cx);
            return;
        }
        if terminal_contexts.is_empty()
            && let Some(later) = parse_later(&text, Local::now())
        {
            let Ok((fire_at_unix_secs, message)) = later else {
                window
                    .push_notification(Notification::error(crate::tr!("composer.later_usage")), cx);
                return;
            };
            let attachment_paths = self
                .pending_images
                .iter()
                .map(|image| image.path.clone())
                .collect();
            input.update(cx, |state, cx| state.replace_all("", window, cx));
            self.pending_images.clear();
            self.image_load_generation = self.image_load_generation.wrapping_add(1);
            self.pending_image_loads = 0;
            self.workspace_store.update(cx, |store, _cx| {
                store.schedule_turn(message, attachment_paths, fire_at_unix_secs);
            });
            cx.emit(ComposerEvent::Submitted);
            cx.notify();
            return;
        }
        // Local mode and picker commands are consumed without sending a turn.
        if terminal_contexts.is_empty()
            && let Some(command) = slash_command(&text, compact_offered, goal_offered)
        {
            if let SlashIntent::Goal(GoalIntent::Edit) = command {
                match current_goal {
                    Some(goal) => self.edit_goal(&goal.objective, window, cx),
                    None => window.push_notification(
                        Notification::info(crate::tr!("composer.goal_usage")),
                        cx,
                    ),
                }
                return;
            }
            input.update(cx, |state, cx| state.replace_all("", window, cx));
            match command {
                SlashIntent::Model => {
                    self.model_picker_token = self.model_picker_token.wrapping_add(1);
                }
                SlashIntent::Compact => {
                    self.workspace_store
                        .update(cx, |store, _cx| store.compact_context());
                }
                SlashIntent::Goal(intent) => {
                    self.workspace_store.update(cx, |store, _cx| match intent {
                        GoalIntent::Set(objective) => {
                            store.set_goal(Some(objective), Some(agent::GoalStatus::Active))
                        }
                        GoalIntent::Pause => store.set_goal(None, Some(agent::GoalStatus::Paused)),
                        GoalIntent::Resume => store.set_goal(None, Some(agent::GoalStatus::Active)),
                        GoalIntent::Clear => store.clear_goal(),
                        GoalIntent::Edit => {}
                    })
                }
            }
            cx.notify();
            return;
        }
        let orchestrate_text = strip_orchestrate_prefix(&text).map(str::to_string);
        let prompt_text = orchestrate_text.as_deref().unwrap_or(&text);
        let sent_text = prompt_text.to_string();
        let attachment_paths = self
            .pending_images
            .iter()
            .map(|image| image.path.clone())
            .collect::<Vec<_>>();
        if let Some((from, to)) = self
            .workspace_store
            .read(cx)
            .composer_state()
            .relay_confirmation
        {
            let composer = cx.entity();
            let input = input.clone();
            window.open_alert_dialog(cx, move |alert, _, _| {
                let composer = composer.clone();
                let input = input.clone();
                let sent_text = sent_text.clone();
                let attachment_paths = attachment_paths.clone();
                alert
                    .title(crate::tr!("composer.relay_title"))
                    .description(crate::tr!(
                        "composer.relay_description",
                        from = from,
                        to = to
                    ))
                    .button_props(
                        DialogButtons::default()
                            .ok_text(crate::tr!("composer.relay_confirm"))
                            .cancel_text(crate::tr!("composer.relay_cancel"))
                            .show_cancel(true),
                    )
                    .on_ok(move |_, window, cx| {
                        composer.update(cx, |composer, cx| {
                            composer.finish_submit(
                                &input,
                                sent_text.clone(),
                                attachment_paths.clone(),
                                false,
                                false,
                                true,
                                window,
                                cx,
                            );
                        });
                        true
                    })
            });
            return;
        }
        self.finish_submit(
            input,
            sent_text,
            attachment_paths,
            orchestrate_text.is_some(),
            steer,
            false,
            window,
            cx,
        );
    }

    #[allow(clippy::too_many_arguments)]
    fn finish_submit(
        &mut self,
        input: &Entity<TextareaState>,
        sent_text: String,
        attachment_paths: Vec<PathBuf>,
        orchestrate: bool,
        steer: bool,
        relay: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        #[cfg(feature = "voice")]
        self.abort_dictation(cx);
        input.update(cx, |state, cx| state.replace_all("", window, cx));
        self.pending_images.clear();
        self.image_load_generation = self.image_load_generation.wrapping_add(1);
        self.pending_image_loads = 0;
        self.workspace_store.update(cx, |store, _cx| {
            if relay {
                store.confirm_relay_and_send(sent_text, attachment_paths);
            } else if orchestrate {
                store.orchestrate_turn(sent_text, attachment_paths);
            } else if steer {
                store.steer(sent_text, attachment_paths);
            } else {
                store.send_turn(sent_text, attachment_paths);
            }
        });
        cx.emit(ComposerEvent::Submitted);
        cx.notify();
    }

    /// Compact control that switches between Send, Queue and Stop.
    fn render_compact_primary_action(
        &self,
        turn_running: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let interactive = self.interactive(cx);
        let has_text = interactive && !self.input.read(cx).value().trim().is_empty();
        if self
            .workspace_store
            .read(cx)
            .composer_state()
            .preparing_worktree
        {
            return div()
                .size(px(crate::material::TOUCH_TARGET))
                .flex()
                .items_center()
                .justify_center()
                .child(Spinner::new().color(cx.theme().primary))
                .into_any_element();
        }
        let stopping = turn_running && !has_text;
        let composer_state = self.workspace_store.read(cx).composer_state();
        let stop_pending = composer_state.stopping;
        let steerable = turn_running && has_text && composer_state.steering_supported;
        let armed = self.steer_gesture.as_ref().is_some_and(SteerGesture::armed);
        let (label, id): (_, &'static str) = if stopping {
            (crate::tr!("composer.stop"), "stop-turn")
        } else if turn_running {
            (crate::tr!("mobile.queue"), "steer-turn")
        } else {
            (crate::tr!("composer.send"), "send-message")
        };
        let touch = px(crate::material::TOUCH_TARGET);
        let button = Button::new(id)
            .debug_selector(move || id.into())
            .with_size(touch)
            .rounded(touch / 2.)
            .aria_label(label)
            .map(|button| {
                if stopping {
                    // A filled stop square; the icon set's square is an outline.
                    // A child is not an icon, so the button's width is set too.
                    button
                        .danger()
                        .w(touch)
                        .disabled(!interactive || stop_pending)
                        .child(div().size(px(13.)).rounded(px(2.)).bg(cx.theme().danger))
                } else {
                    button.primary().disabled(!has_text).icon(IconName::ArrowUp)
                }
            })
            .on_click(cx.listener(move |this, _, window, cx| {
                if stopping {
                    if this.interactive(cx) {
                        this.workspace_store
                            .update(cx, |store, _cx| store.interrupt());
                    }
                    return;
                }
                let input = this.input.clone();
                this.submit(&input, false, window, cx);
            }));
        let button = div()
            .relative()
            .flex_none()
            .child(button)
            .when(steerable || self.steer_gesture.is_some(), |el| {
                el.child(self.render_steer_gesture(cx))
            });
        v_flex()
            .flex_none()
            .items_center()
            .gap(cx.theme().tokens.spacing.xxs)
            .child(button)
            .when(turn_running && has_text, |el| {
                el.child(
                    div()
                        .text_secondary(false, cx)
                        .text_color(if armed {
                            cx.theme().primary
                        } else {
                            cx.theme().muted_foreground
                        })
                        .child(crate::tr!(if armed {
                            "mobile.steer"
                        } else {
                            "mobile.queue"
                        })),
                )
            })
            .into_any_element()
    }

    fn render_stop(&self, cx: &mut Context<Self>) -> Button {
        let size = px(SEND_SIZE);
        // A filled stop square; the icon set's square is an outline. A child is
        // not an icon, so the button's width is set too.
        Button::new("stop-turn")
            .debug_selector(|| "stop-turn".into())
            .danger()
            .with_size(size)
            .w(size)
            .aria_label(crate::tr!("composer.stop"))
            .disabled(
                !self.interactive(cx) || self.workspace_store.read(cx).composer_state().stopping,
            )
            .child(div().size(px(11.)).rounded(px(2.)).bg(cx.theme().danger))
            .on_click(cx.listener(|this, _, _, cx| {
                if !this.interactive(cx) {
                    return;
                }
                this.workspace_store
                    .update(cx, |store, _cx| store.interrupt());
            }))
    }

    fn render_send_or_stop(&self, turn_running: bool, cx: &mut Context<Self>) -> AnyElement {
        if self.compact {
            return self.render_compact_primary_action(turn_running, cx);
        }
        if turn_running {
            // Providers with native mid-turn steering keep a send button active
            // beside Stop while a turn runs.
            let steers = self
                .workspace_store
                .read(cx)
                .composer_state()
                .steering_supported;
            let has_text = self.interactive(cx) && !self.input.read(cx).value().trim().is_empty();
            let mut row = h_flex()
                .gap_2()
                .items_center()
                .child(Spinner::new().color(cx.theme().primary));
            if steers {
                let queue_hint = crate::tr!(
                    "composer.queue_hint",
                    shortcut = format_secondary_shortcut("enter")
                )
                .into_owned();
                row = row.child(
                    Button::new("steer-turn")
                        .debug_selector(|| "steer-turn".into())
                        .primary()
                        .with_size(px(SEND_SIZE))
                        .icon(IconName::ArrowUp)
                        .aria_label(crate::tr!("composer.steer_tooltip"))
                        .tooltip(queue_hint)
                        .disabled(!has_text)
                        .on_click(cx.listener(|this, _, window, cx| {
                            let input = this.input.clone();
                            this.submit(&input, false, window, cx);
                        })),
                );
            }
            return row.child(self.render_stop(cx)).into_any_element();
        }

        // First-send worktree preparation temporarily disables the send control.
        if self
            .workspace_store
            .read(cx)
            .composer_state()
            .preparing_worktree
        {
            return h_flex()
                .gap_2()
                .items_center()
                .child(Spinner::new().color(cx.theme().primary))
                .child(
                    div()
                        .text_secondary(false, cx)
                        .child(crate::tr!("composer.preparing_worktree")),
                )
                .into_any_element();
        }

        let has_text = self.interactive(cx) && !self.input.read(cx).value().trim().is_empty();
        Button::new("send-message")
            .debug_selector(|| "send-message".into())
            .primary()
            .with_size(px(SEND_SIZE))
            .icon(IconName::ArrowUp)
            .aria_label(crate::tr!("composer.send"))
            .disabled(!has_text)
            .on_click(cx.listener(|this, _, window, cx| {
                let input = this.input.clone();
                this.submit(&input, false, window, cx);
            }))
            .into_any_element()
    }

    fn render_primary_action(&self, turn_running: bool, cx: &mut Context<Self>) -> AnyElement {
        if self
            .workspace_store
            .read(cx)
            .composer_state()
            .conversation_read_only
        {
            return Button::new("send-message")
                .debug_selector(|| "send-message".into())
                .ghost()
                .disabled(true)
                .aria_label(crate::tr!("composer.send").into_owned())
                .with_size(px(if self.compact {
                    crate::material::TOUCH_TARGET
                } else {
                    SEND_SIZE
                }))
                .icon(IconName::ArrowUp)
                .into_any_element();
        }
        if turn_running {
            return self.render_send_or_stop(true, cx);
        }
        self.render_send_or_stop(turn_running, cx)
    }
}

impl Render for Composer {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.sync_user_input_state(window, cx);
        self.sync_images_session(cx);
        self.sync_text_destination(window, cx);
        self.sync_queued_refill(window, cx);
        self.sync_native_rewind_prefill(window, cx);
        self.sync_composer_append(window, cx);
        self.sync_fallback_review_draft(window, cx);
        let composer_state = self.workspace_store.read(cx).composer_state();
        let readonly = composer_state.conversation_read_only;
        let turn_running = composer_state.turn_running;
        let approval = composer_state.pending_approval;
        let approval_count = composer_state.pending_approval_count;

        let border = cx.theme().border;
        let divider = move || div().w_px().h(px(16.)).bg(border);

        let compact = self.compact || self.control_row_narrow.get();

        let control_row_base = h_flex()
            .w_full()
            .min_w_0()
            .overflow_hidden()
            .gap_1()
            .items_center();

        #[cfg(feature = "voice")]
        let mic = if readonly {
            None
        } else {
            self.render_mic_button(cx)
        };
        #[cfg(not(feature = "voice"))]
        let mic: Option<AnyElement> = None;

        // The platform picker is the phone's only way in; desktops paste and
        // drop, and the readonly and offline states have nothing to add to.
        let attach_menu = (self.compact && !readonly && self.interactive(cx))
            .then(|| {
                cx.try_global::<crate::remote::ClientAttachment>()
                    .is_some_and(|client| client.host().supports_image_picker())
                    .then(|| self.render_attach_menu(cx))
            })
            .flatten();
        // The chips give way to the send/stop control: Stop must stay
        // reachable however long the chips' labels run.
        let leading = || {
            h_flex()
                .flex_1()
                .min_w_0()
                .overflow_hidden()
                .gap_1()
                .items_center()
        };
        // The model name truncates to make room, down to the width of one
        // control so the picker can still be opened.
        let model_cell = || div().min_w(px(SEND_SIZE));
        let primary = div()
            .flex_none()
            .child(self.render_primary_action(turn_running, cx));
        let control_row = if self.compact || (readonly && compact) {
            control_row_base
                .child(
                    leading()
                        .children(attach_menu)
                        .child(model_cell().child(self.render_model_picker(cx)))
                        .child(self.render_traits_picker(cx))
                        .child(div().flex_1())
                        .children(mic),
                )
                .child(primary)
        } else if compact {
            control_row_base
                .child(
                    leading()
                        .child(model_cell().child(self.render_model_picker(cx)))
                        .child(self.render_overflow_menu(cx))
                        .child(div().flex_1())
                        .children(mic)
                        .child(self.render_context_meter(cx)),
                )
                .child(primary)
        } else {
            control_row_base
                .child(
                    leading()
                        .child(model_cell().child(self.render_model_picker(cx)))
                        .child(self.render_traits_picker(cx))
                        .child(divider())
                        .child(self.render_context_meter(cx))
                        .child(self.render_permission_picker(cx))
                        .child(div().flex_1())
                        .children(mic),
                )
                .child(primary)
        };

        // The row's width is known only once it is laid out, after this
        // render; one that folds or unfolds the controls renders again.
        let narrow_cell = self.control_row_narrow.clone();
        let composer = cx.entity_id();
        let control_row = control_row.on_prepaint(move |bounds, window, _| {
            let narrow = f32::from(bounds.size.width) < CONTROL_ROW_COMPACT_BELOW;
            if narrow_cell.replace(narrow) != narrow {
                window.on_next_frame(move |_, cx| cx.notify(composer));
            }
        });

        let desired_placeholder = if readonly {
            crate::tr!("chat.subagent_readonly")
        } else if self.compact && !self.interactive(cx) {
            crate::tr!("mobile.offline_message")
        } else {
            draft_placeholder(self.compact)
        };

        let user_input = self.pending_user_input(cx);
        let fallback_block = self
            .workspace_store
            .read(cx)
            .active_fallback_block()
            .cloned();
        let fallback_review = self
            .workspace_store
            .read(cx)
            .active_fallback_review()
            .cloned();

        let composer = cx.entity();
        let terminal_contexts = self
            .workspace_store
            .read(cx)
            .composer_state()
            .terminal_contexts;
        let has_terminal_contexts = !terminal_contexts.is_empty();
        let context_chips =
            h_flex()
                .w_full()
                .flex_wrap()
                .gap_1()
                .children(terminal_contexts.into_iter().map(|context| {
                    let id = context.id;
                    let range = if context.line_start == context.line_end {
                        format!("L{}", context.line_start)
                    } else {
                        format!("L{}-L{}", context.line_start, context.line_end)
                    };
                    let text = context.text.clone();
                    Button::new(("terminal-context-chip", id))
                        .ghost()
                        .small()
                        .chip()
                        .text_meta_mono(self.compact, cx)
                        .label(format!("{} · {}  ×", context.terminal_label, range))
                        .tooltip(context.text)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.workspace_store
                                .update(cx, |store, _cx| store.remove_terminal_context(id));
                        }))
                        .context_menu(move |menu, _, _| {
                            menu.menu(
                                crate::tr!("chat.copy_text").into_owned(),
                                Box::new(CopyText(text.clone())),
                            )
                            .separator()
                            .menu(
                                crate::tr!("composer.remove_context").into_owned(),
                                Box::new(ComposerMenu::RemoveTerminalContext(id)),
                            )
                        })
                }));
        let review_comments = self.workspace_store.read(cx).review_comments();
        let has_review_comments = !review_comments.is_empty();
        let review_chips = h_flex().w_full().flex_wrap().gap_1().children(
            review_comments
                .into_iter()
                .enumerate()
                .map(|(index, comment)| {
                    let range = if comment.line_start == comment.line_end {
                        format!("L{}", comment.line_start)
                    } else {
                        format!("L{}-L{}", comment.line_start, comment.line_end)
                    };
                    let text = comment.text.clone();
                    Button::new(("review-comment-chip", index))
                        .ghost()
                        .small()
                        .chip()
                        .text_meta_mono(self.compact, cx)
                        .label(format!("{} {}  ×", comment.file, range))
                        .tooltip(comment.text)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.workspace_store
                                .update(cx, |store, _cx| store.remove_review_comment(index));
                        }))
                        .context_menu(move |menu, _, _| {
                            menu.menu(
                                crate::tr!("chat.copy_text").into_owned(),
                                Box::new(CopyText(text.clone())),
                            )
                            .separator()
                            .menu(
                                crate::tr!("composer.remove_context").into_owned(),
                                Box::new(ComposerMenu::RemoveReviewComment(index)),
                            )
                        })
                }),
        );

        // Focus swaps the hairline to primary in one frame. Geometry stays
        // fixed: focus never changes border width, radius, or layout.
        let composer_focused = !readonly && self.input.read(cx).focus_handle(cx).is_focused(window);
        let card = v_flex()
            .debug_selector(|| "composer-card".into())
            .w_full()
            .gap_1p5()
            .p(px(6.))
            .rounded(if self.compact {
                px(16.)
            } else {
                crate::material::radius_composer(cx)
            })
            .border_1()
            .border_color(if composer_focused {
                cx.theme().primary
            } else {
                cx.theme().border
            })
            // The editor needs an opaque fill over the translucent canvas.
            .bg(cx.theme().popover)
            .shadow_md()
            // The editor binds Secondary+V to Paste, which consumes the keystroke
            // before any key-down listener runs — so image paste must intercept
            // the Paste *action* in the capture phase. Swallow it only when the
            // clipboard held an image; text paste propagates to the editor.
            .capture_action(cx.listener(|this, _: &Paste, window, cx| {
                if this.interactive(cx) && !this.compact && this.paste_clipboard_image(window, cx) {
                    cx.stop_propagation();
                }
            }))
            .capture_action(cx.listener(|this, paste: &BrowserPaste, window, cx| {
                if this.interactive(cx) && !this.compact && this.paste_images(&paste.0, window, cx)
                {
                    cx.stop_propagation();
                }
            }))
            // Arrow/Escape trigger-menu navigation (fires after the input's own
            // key actions).
            .capture_key_down(cx.listener(|this, ev: &gpui::KeyDownEvent, window, cx| {
                let key = ev.keystroke.key.as_str();
                if this.handle_user_input_digit(ev, window, cx) {
                    cx.stop_propagation();
                    return;
                }
                // Escape ends dictation (keeping the transcript) before it can
                // mean anything else.
                #[cfg(feature = "voice")]
                if key == "escape" && this.stop_dictation(cx) {
                    cx.stop_propagation();
                    return;
                }
                if key == "escape" && this.menu_visible(cx) {
                    this.menu_dismissed = true;
                    cx.notify();
                }
            }))
            // The editor binds Up and Down to cursor moves, which consume the
            // keystroke before any key-down listener runs.
            .capture_action(cx.listener(|this, _: &MoveUp, _, cx| {
                if this.menu_visible(cx) {
                    this.menu_highlight = this.menu_highlight.saturating_sub(1);
                    cx.stop_propagation();
                    cx.notify();
                }
            }))
            .capture_action(cx.listener(|this, _: &MoveDown, _, cx| {
                if this.menu_visible(cx) {
                    let (rows, _, _) = this.menu_rows(cx);
                    this.menu_highlight =
                        (this.menu_highlight + 1).min(rows.len().saturating_sub(1));
                    cx.stop_propagation();
                    cx.notify();
                }
            }))
            .on_drop(
                move |paths: &ExternalPaths, window: &mut Window, cx: &mut App| {
                    let paths: Vec<PathBuf> = paths.paths().to_vec();
                    composer.update(cx, |this, cx| {
                        if this.compact || !this.interactive(cx) {
                            return;
                        }
                        for path in paths {
                            if mime_from_path(&path).starts_with("image/") {
                                this.add_image_path(path, window, cx);
                            }
                        }
                    });
                },
            )
            .when(has_terminal_contexts, |this| this.child(context_chips))
            .when(has_review_comments, |this| this.child(review_chips))
            .children(self.render_reference_chips(cx))
            .child(
                Textarea::new(&self.input)
                    .placeholder(desired_placeholder)
                    .disabled(readonly)
                    .appearance(false),
            )
            .children(self.render_image_strip(cx))
            .child(control_row);

        // Mirror the chat timeline's centered content column (same page
        // padding, same max width) so the composer lines up with the messages
        // above it instead of stretching edge to edge on wide windows.
        v_flex()
            .relative()
            .flex_shrink_0()
            .w_full()
            .items_center()
            .px(px(if self.compact {
                crate::material::COMPACT_PAGE_INSET
            } else {
                crate::chat::CONTENT_MIN_PADDING
            }))
            .pb_2()
            .key_context(CONTEXT)
            .on_action(cx.listener(Self::on_menu))
            .child(
                v_flex()
                    .w_full()
                    .max_w(px(crate::chat::CONTENT_MAX_WIDTH))
                    .gap_2()
                    .when_some(approval, |this, request| {
                        this.child(self.render_approval_panel(&request, approval_count, cx))
                    })
                    .children(self.render_control_request_panel(cx))
                    .when_some(user_input, |this, pending| {
                        this.child(self.render_user_input_panel(&pending, window, cx))
                    })
                    .when_some(fallback_block, |this, block| {
                        this.child(self.render_fallback_panel(&block, cx))
                    })
                    .when_some(fallback_review, |this, review| {
                        this.child(self.render_fallback_review_panel(&review, cx))
                    })
                    .children(self.render_trigger_menu(cx))
                    .children(self.render_goal_strip(cx))
                    .children(self.render_queue_strip(cx))
                    .child(v_flex().w_full().child(card).when(
                        self.compact || (readonly && compact),
                        |el| {
                            el.child(
                                h_flex()
                                    .debug_selector(|| "composer-settings-drawer".into())
                                    .mx_2()
                                    .px_1()
                                    .min_w_0()
                                    .gap_1()
                                    .items_center()
                                    .rounded_b(px(12.))
                                    .border_1()
                                    .border_t_0()
                                    .border_color(cx.theme().border)
                                    .bg(cx.theme().muted)
                                    .child(self.render_permission_picker(cx))
                                    .child(div().flex_1())
                                    .child(self.render_context_meter(cx)),
                            )
                        },
                    ))
                    .when(!self.compact && !readonly, |el| {
                        el.children(self.render_checkout_row(cx))
                    }),
            )
    }
}

#[cfg(test)]
mod replica_tests {
    use super::*;
    use gpui::{Modifiers, TestAppContext};
    use tcode_protocol::{
        EventEnvelope, HostMessage, ServerEvent, SessionStatus, Topic, encode_line,
    };

    fn attach(
        cx: &mut TestAppContext,
    ) -> (
        Entity<WorkspaceStore>,
        async_channel::Sender<String>,
        async_channel::Receiver<String>,
        SessionStatus,
    ) {
        cx.update(crate::theme::init);
        cx.update(crate::markdown::init);
        let root = crate::store::tests::scratch_root("composer-replica");
        let host = tcode_runtime::pipe::spawn_host(
            tcode_services::store::SessionStore::open_at(root.clone()).unwrap(),
            tcode_runtime::pipe::HostServices::default(),
        )
        .unwrap();
        let status = smol::block_on(host.update_state_for_test(|state, cx| {
            let id = state.start_draft("replica".into(), std::env::temp_dir(), cx);
            state.session_status_snapshot(&id).unwrap()
        }))
        .unwrap();
        host.shutdown_blocking().unwrap();
        std::fs::remove_dir_all(root).unwrap();
        let (to_host, outgoing) = async_channel::unbounded();
        let (incoming, from_host) = async_channel::unbounded();
        let link = tcode_client::HostLink::new(to_host, from_host);
        let pump = link.clone();
        let executor = cx.background_executor.clone();
        cx.background_executor
            .spawn(async move {
                pump.pump_with_timer(|| executor.timer(Duration::from_millis(25)))
                    .await;
            })
            .detach();
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
        store.update(cx, |store, _| {
            store.select_session(status.session_id.clone())
        });
        (store, incoming, outgoing, status)
    }

    fn replace(incoming: &async_channel::Sender<String>, topic: Topic, event: ServerEvent) {
        incoming
            .try_send(
                encode_line(&HostMessage::Event(EventEnvelope {
                    request_id: None,
                    topic,
                    event,
                }))
                .unwrap(),
            )
            .unwrap();
    }

    #[gpui::test]
    fn composer_history_survives_submission_and_destination_switches(cx: &mut TestAppContext) {
        for is_draft in [false, true] {
            let (store, incoming, outgoing, mut status) = attach(cx);
            status.draft = is_draft;
            status.project_id = Some("project-a".into());
            let mut restored_status = status.clone();
            replace(
                &incoming,
                Topic::SessionStatus {
                    session_id: status.session_id.clone(),
                },
                ServerEvent::SessionStatusReplaced(Box::new(status.clone())),
            );
            cx.run_until_parked();
            store.update(cx, |store, cx| store.drain_host_events_for_test(cx));
            let (composer, cx) =
                cx.add_window_view(|window, cx| Composer::new(store.clone(), window, cx));
            cx.update(|window, cx| {
                window.draw(cx).clear(cx);
                composer.update(cx, |composer, cx| composer.focus(window, cx));
            });
            cx.simulate_input("Keep this prompt");
            cx.update(|window, cx| window.draw(cx).clear(cx));
            let send = cx.debug_bounds("send-message").expect("send button");
            cx.simulate_click(send.center(), Modifiers::default());
            cx.update(|window, cx| {
                composer.update(cx, |composer, cx| composer.focus(window, cx));
                window.draw(cx).clear(cx);
            });
            assert_eq!(
                composer.read_with(cx, |composer, cx| composer.draft(cx)),
                ""
            );
            assert!(
                std::iter::from_fn(|| outgoing.try_recv().ok()).any(|line| matches!(
                    tcode_protocol::decode_client_line(&line).unwrap().payload,
                    tcode_protocol::ClientPayload::Command(
                        tcode_protocol::Command::SendTurn { .. }
                    )
                ))
            );
            cx.simulate_keystrokes(if cfg!(target_os = "macos") {
                "cmd-z"
            } else {
                "ctrl-z"
            });
            assert_eq!(
                composer.read_with(cx, |composer, cx| composer.draft(cx)),
                "Keep this prompt"
            );
            status.session_id = "other-thread".into();
            status.project_id = Some("project-b".into());
            store.update(cx, |store, _| {
                store.select_session(status.session_id.clone())
            });
            replace(
                &incoming,
                Topic::SessionStatus {
                    session_id: status.session_id.clone(),
                },
                ServerEvent::SessionStatusReplaced(Box::new(status.clone())),
            );
            cx.run_until_parked();
            store.update(cx, |store, cx| {
                store.drain_host_events_for_test(cx);
            });
            cx.update(|window, cx| window.draw(cx).clear(cx));
            assert_eq!(
                composer.read_with(cx, |composer, cx| composer.draft(cx)),
                ""
            );
            cx.update(|window, cx| composer.update(cx, |composer, cx| composer.focus(window, cx)));
            cx.simulate_input("Other destination draft");
            let outgoing = composer.read_with(cx, |composer, _| composer.input.clone());
            cx.update(|window, cx| {
                outgoing.update(cx, |input, cx| {
                    use gpui::EntityInputHandler as _;
                    input.replace_and_mark_text_in_range(None, "，你好", None, window, cx);
                });
                window.draw(cx).clear(cx);
            });
            if is_draft {
                restored_status.session_id = "reopened-project-draft".into();
            }
            store.update(cx, |store, _| {
                store.select_session(restored_status.session_id.clone())
            });
            replace(
                &incoming,
                Topic::SessionStatus {
                    session_id: restored_status.session_id.clone(),
                },
                ServerEvent::SessionStatusReplaced(Box::new(restored_status.clone())),
            );
            cx.run_until_parked();
            store.update(cx, |store, cx| store.drain_host_events_for_test(cx));
            cx.update(|window, cx| {
                window.draw(cx).clear(cx);
                composer.update(cx, |composer, cx| composer.focus(window, cx));
                outgoing.update(cx, |input, cx| {
                    use gpui::EntityInputHandler as _;
                    assert_eq!(input.marked_text_range(window, cx), None);
                    assert_eq!(input.value(), "Other destination draft，你好");
                });
            });
            assert_eq!(
                composer.read_with(cx, |composer, cx| composer.draft(cx)),
                "Keep this prompt"
            );
            cx.simulate_keystrokes(if cfg!(target_os = "macos") {
                "cmd-shift-z"
            } else {
                "ctrl-y"
            });
            assert_eq!(
                composer.read_with(cx, |composer, cx| composer.draft(cx)),
                ""
            );
            for (destination, expected) in [
                (status, "Other destination draft，你好"),
                (restored_status, ""),
            ] {
                store.update(cx, |store, _| {
                    store.select_session(destination.session_id.clone())
                });
                replace(
                    &incoming,
                    Topic::SessionStatus {
                        session_id: destination.session_id.clone(),
                    },
                    ServerEvent::SessionStatusReplaced(Box::new(destination)),
                );
                cx.run_until_parked();
                store.update(cx, |store, cx| store.drain_host_events_for_test(cx));
                cx.update(|window, cx| window.draw(cx).clear(cx));
                assert_eq!(
                    composer.read_with(cx, |composer, cx| composer.draft(cx)),
                    expected
                );
            }
        }
    }

    /// A desktop composer folds its controls into the overflow menu once its
    /// row is laid out narrower than they fit, and unfolds them once wider,
    /// in the frames that follow the resize.
    #[gpui::test]
    fn the_control_row_folds_with_the_width_it_is_laid_out_at(cx: &mut TestAppContext) {
        let (store, _incoming, _outgoing, _status) = attach(cx);
        let (_composer, cx) =
            cx.add_window_view(|window, cx| Composer::new(store.clone(), window, cx));
        let resize = |width: f32, cx: &mut gpui::VisualTestContext| {
            cx.simulate_resize(gpui::size(px(width), px(800.)));
            for _ in 0..100 {
                cx.run_until_parked();
                if cx.update(|window, cx| window.simulate_next_frame(cx)) == 0 {
                    return cx.debug_bounds("overflow-controls").is_some();
                }
            }
            panic!("the composer kept asking for frames");
        };
        assert!(!resize(1200., cx), "a wide row folded its controls");
        assert!(resize(400., cx), "a narrow row kept its controls");
        assert!(!resize(1200., cx), "a row widened again kept them folded");
    }

    /// A running turn widens the desktop action to spinner, steer and Stop;
    /// at every width the row is laid out at, Stop stays inside the window and
    /// clear of a model name too long for the row.
    #[gpui::test]
    fn a_running_turn_keeps_stop_on_screen_at_every_width(cx: &mut TestAppContext) {
        let (store, incoming, _outgoing, mut status) = attach(cx);
        status.activity.turn_running = true;
        status.steering_supported = true;
        status.requested_model = Some("an-unlisted-model-with-a-long-identifier".into());
        replace(
            &incoming,
            Topic::SessionStatus {
                session_id: status.session_id.clone(),
            },
            ServerEvent::SessionStatusReplaced(Box::new(status)),
        );
        cx.run_until_parked();
        store.update(cx, |store, cx| store.drain_host_events_for_test(cx));
        let (_composer, cx) =
            cx.add_window_view(|window, cx| Composer::new(store.clone(), window, cx));
        for width in (360..=1200).step_by(20) {
            let width = width as f32;
            cx.simulate_resize(gpui::size(px(width), px(800.)));
            for _ in 0..100 {
                cx.run_until_parked();
                if cx.update(|window, cx| window.simulate_next_frame(cx)) == 0 {
                    break;
                }
            }
            let stop = cx
                .debug_bounds("stop-turn")
                .unwrap_or_else(|| panic!("no Stop at {width}px"));
            assert!(
                f32::from(stop.right()) <= width,
                "Stop is clipped at {width}px: its right edge is at {:?}",
                stop.right()
            );
            let picker = cx.debug_bounds("model-picker").unwrap();
            assert!(
                picker.right() <= stop.left(),
                "the model picker runs under Stop at {width}px: {picker:?} vs {stop:?}"
            );
        }
    }

    /// A value the provider reports as unavailable is shown but never sent.
    #[gpui::test]
    fn unavailable_permission_rows_cannot_be_chosen(cx: &mut TestAppContext) {
        let (store, incoming, outgoing, mut status) = attach(cx);
        let mut descriptor = agent::permission_control(status.provider).unwrap();
        let OptionDescriptor::Select { id, options, .. } = &mut descriptor else {
            panic!("the draft's provider publishes a select permission control");
        };
        let id = id.clone();
        options[1].unavailable = Some("Not available for this account".into());
        let chosen = options[0].value.clone();
        status
            .provider_option_descriptors
            .retain(|existing| !matches!(existing, OptionDescriptor::Select { id: existing, .. } if *existing == id));
        status.provider_option_descriptors.push(descriptor);
        replace(
            &incoming,
            Topic::SessionStatus {
                session_id: status.session_id.clone(),
            },
            ServerEvent::SessionStatusReplaced(Box::new(status)),
        );
        cx.run_until_parked();
        store.update(cx, |store, cx| store.drain_host_events_for_test(cx));
        let (_composer, cx) =
            cx.add_window_view(|window, cx| Composer::new(store.clone(), window, cx));
        cx.simulate_resize(gpui::size(px(1200.), px(800.)));
        cx.update(|window, cx| _ = window.draw(cx));
        while outgoing.try_recv().is_ok() {}
        let sent = || {
            std::iter::from_fn(|| outgoing.try_recv().ok())
                .filter_map(|line| {
                    match tcode_protocol::decode_client_line(&line).unwrap().payload {
                        tcode_protocol::ClientPayload::Command(
                            tcode_protocol::Command::SetActiveOption { id, value, .. },
                        ) => Some((id, value)),
                        _ => None,
                    }
                })
                .collect::<Vec<_>>()
        };
        let click = |cx: &mut gpui::VisualTestContext, selector: &'static str| {
            let bounds = cx
                .debug_bounds(selector)
                .unwrap_or_else(|| panic!("{selector}"));
            cx.simulate_click(bounds.center(), Modifiers::default());
            cx.update(|window, cx| _ = window.draw(cx));
        };
        click(cx, "permission-chip");
        click(cx, "permission-row-1");
        cx.run_until_parked();
        assert_eq!(sent(), []);
        assert!(
            cx.debug_bounds("permission-pane").is_some(),
            "the menu stays open"
        );
        click(cx, "permission-row-0");
        cx.run_until_parked();
        assert_eq!(sent(), [(id, Some(serde_json::Value::String(chosen)))]);
    }

    #[gpui::test]
    fn switching_language_relocalizes_the_model_search_placeholder(cx: &mut TestAppContext) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        crate::settings::apply_locale(Some(crate::LANGUAGE_SIMPLIFIED_CHINESE));
        let (store, incoming, _outgoing, status) = attach(cx);
        replace(
            &incoming,
            Topic::SessionStatus {
                session_id: status.session_id.clone(),
            },
            ServerEvent::SessionStatusReplaced(Box::new(status)),
        );
        cx.run_until_parked();
        store.update(cx, |store, cx| store.drain_host_events_for_test(cx));
        let (composer, cx) =
            cx.add_window_view(|window, cx| Composer::new(store.clone(), window, cx));
        cx.simulate_resize(gpui::size(px(1200.), px(800.)));
        cx.update(|window, cx| _ = window.draw(cx));
        let picker = cx.debug_bounds("model-picker").expect("model picker");
        cx.simulate_click(picker.center(), Modifiers::default());
        crate::settings::assert_follows_language_switch(cx, "composer.search_models", |cx| {
            composer.read_with(cx, |composer, cx| {
                let search = composer.model_search.read(cx);
                search.presentation().placeholder().to_string()
            })
        });
    }

    #[gpui::test]
    fn holding_queue_reveals_choices_and_release_height_picks_steer_queue_or_nothing(
        cx: &mut TestAppContext,
    ) {
        use gpui::{TouchEvent, TouchId, TouchPhase, point};

        let (store, incoming, outgoing, mut status) = attach(cx);
        status.activity.turn_running = true;
        status.steering_supported = true;
        let mut unsupported = status.clone();
        unsupported.steering_supported = false;
        // A phone sends only once its screen is current, so every baseline
        // topic is answered.
        let session_id = status.session_id.clone();
        let baseline = [
            (
                Topic::Index,
                ServerEvent::IndexSnapshot(tcode_protocol::IndexSnapshot {
                    summary: Default::default(),
                    sessions: Vec::new(),
                    projects: Vec::new(),
                }),
            ),
            (
                Topic::Settings,
                ServerEvent::SettingsSnapshot(tcode_core::settings::Settings::default()),
            ),
            (
                Topic::SessionStatus {
                    session_id: session_id.clone(),
                },
                ServerEvent::SessionStatusReplaced(Box::new(status)),
            ),
            (
                Topic::SessionPlan {
                    session_id: session_id.clone(),
                },
                ServerEvent::SessionPlanReplaced(tcode_protocol::SessionPlan {
                    session_id: session_id.clone(),
                    steps: Vec::new(),
                }),
            ),
            (
                Topic::SessionEvents { session_id },
                ServerEvent::SessionSnapshot {
                    from: 0,
                    end: 0,
                    records: Vec::new(),
                    total: 0,
                    total_turns: 0,
                    truncated: false,
                },
            ),
        ];
        for (topic, event) in baseline {
            replace(&incoming, topic, event);
        }
        cx.run_until_parked();
        store.update(cx, |store, cx| store.drain_host_events_for_test(cx));
        let (composer, cx) =
            cx.add_window_view(|window, cx| Composer::new(store.clone(), window, cx));
        composer.update(cx, |composer, cx| composer.set_compact(true, cx));
        cx.simulate_resize(gpui::size(px(393.), px(800.)));
        let touch = |cx: &mut gpui::VisualTestContext, phase, position| {
            cx.simulate_event(TouchEvent {
                id: TouchId(1),
                phase,
                position,
                predicted_position: None,
                force: None,
            });
            cx.update(|window, cx| _ = window.draw(cx));
        };
        let sent =
            |cx: &mut gpui::VisualTestContext| {
                cx.executor().advance_clock(Duration::from_millis(50));
                cx.run_until_parked();
                let mut sent = Vec::new();
                while let Ok(line) = outgoing.try_recv() {
                    match tcode_protocol::decode_client_line(&line).unwrap().payload {
                        tcode_protocol::ClientPayload::Command(
                            tcode_protocol::Command::Steer { text, .. },
                        ) => sent.push(("steer", text)),
                        tcode_protocol::ClientPayload::Command(
                            tcode_protocol::Command::SendTurn { text, .. },
                        ) => sent.push(("queue", text)),
                        _ => {}
                    }
                }
                sent
            };
        assert!(sent(cx).is_empty());

        let mut upper_target = None;
        for (gesture, expected) in [
            ("hold", "queue"),
            ("hold-slide", "steer"),
            ("slide", "steer"),
            ("direct-slide", "steer"),
            ("lower", "queue"),
            ("tap", "queue"),
            ("sideways", "queue"),
            ("hold-sideways", "queue"),
            ("steer-sideways", "steer"),
        ] {
            composer.update_in(cx, |composer, window, cx| {
                composer.set_input_text(gesture.into(), window, cx)
            });
            cx.update(|window, cx| _ = window.draw(cx));
            let button = cx
                .debug_bounds("steer-turn")
                .expect("the Queue button is shown");
            let center = button.center();
            touch(cx, TouchPhase::Started, center);
            assert!(cx.debug_bounds("steer-gesture-steer").is_none());
            assert!(cx.debug_bounds("steer-gesture-queue").is_none());
            let mut lift = center;
            if matches!(gesture, "hold" | "hold-slide" | "lower" | "hold-sideways") {
                cx.executor()
                    .advance_clock(gpui::GestureTuning::default().long_press_duration);
                cx.run_until_parked();
                cx.update(|window, cx| _ = window.draw(cx));
            }
            if matches!(gesture, "slide" | "steer-sideways") {
                touch(
                    cx,
                    TouchPhase::Moved,
                    point(center.x, button.top() - px(1.)),
                );
            }
            if gesture == "direct-slide" {
                lift = upper_target.expect("the previous gesture measured Steer");
                touch(cx, TouchPhase::Moved, lift);
            }
            if gesture == "sideways" {
                lift = point(center.x - px(120.), center.y);
                touch(cx, TouchPhase::Moved, lift);
            }
            if gesture != "tap" && gesture != "sideways" {
                cx.update(|window, cx| _ = window.draw(cx));
                let upper = cx
                    .debug_bounds("steer-gesture-steer")
                    .expect("holding reveals the upper Steer button");
                let lower = cx
                    .debug_bounds("steer-gesture-queue")
                    .expect("holding reveals the lower Queue button");
                assert_eq!(lower, button, "Queue stays under the finger");
                assert_eq!(upper.center().x, lower.center().x);
                assert!(
                    upper.bottom() <= lower.top(),
                    "Steer is directly above Queue"
                );
                upper_target = Some(upper.center());
                if gesture == "direct-slide" {
                    assert!(cx.debug_bounds("steer-gesture-steer-selected").is_some());
                } else if !matches!(gesture, "slide" | "steer-sideways") {
                    assert!(cx.debug_bounds("steer-gesture-queue-selected").is_some());
                }
                match gesture {
                    "hold-sideways" => {
                        lift = point(center.x - px(120.), lower.top() - px(4.));
                        touch(cx, TouchPhase::Moved, lift);
                        assert!(
                            cx.debug_bounds("steer-gesture-queue-selected").is_some(),
                            "level with Queue queues wherever the finger is horizontally"
                        );
                    }
                    "steer-sideways" => {
                        lift = point(center.x - px(120.), upper.top() - px(40.));
                        touch(cx, TouchPhase::Moved, lift);
                        assert!(
                            cx.debug_bounds("steer-gesture-steer-selected").is_some(),
                            "anywhere above the gap under Steer steers"
                        );
                    }
                    "slide" | "hold-slide" | "lower" => {
                        lift = upper.center();
                        touch(cx, TouchPhase::Moved, lift);
                        assert!(
                            cx.debug_bounds("steer-gesture-steer-selected").is_some(),
                            "{gesture}: moving onto Steer highlights it"
                        );
                        if gesture == "lower" {
                            lift = lower.center();
                            touch(cx, TouchPhase::Moved, lift);
                            assert!(cx.debug_bounds("steer-gesture-queue-selected").is_some());
                        }
                    }
                    _ => {}
                }
            }
            assert!(
                sent(cx).is_empty(),
                "nothing is sent before the finger lifts"
            );
            touch(cx, TouchPhase::Ended, lift);
            assert_eq!(sent(cx), vec![(expected, gesture.to_string())], "{gesture}");
            assert!(cx.debug_bounds("steer-gesture-steer").is_none());
            assert!(cx.debug_bounds("steer-gesture-queue").is_none());
            assert!(
                composer
                    .read_with(cx, |composer, cx| composer.draft(cx))
                    .is_empty()
            );
        }

        for gesture in ["down", "hold-down", "steer-down", "cancel"] {
            composer.update_in(cx, |composer, window, cx| {
                composer.set_input_text("kept".into(), window, cx)
            });
            cx.update(|window, cx| _ = window.draw(cx));
            let button = cx.debug_bounds("steer-turn").unwrap();
            touch(cx, TouchPhase::Started, button.center());
            if gesture == "hold-down" {
                cx.executor()
                    .advance_clock(gpui::GestureTuning::default().long_press_duration);
                cx.run_until_parked();
                cx.update(|window, cx| _ = window.draw(cx));
            }
            let mut lift = button.center();
            if matches!(gesture, "steer-down" | "cancel") {
                touch(
                    cx,
                    TouchPhase::Moved,
                    point(button.center().x, button.top() - px(1.)),
                );
                let upper = cx.debug_bounds("steer-gesture-steer").unwrap();
                touch(cx, TouchPhase::Moved, upper.center());
                lift = upper.center();
            }
            let away = point(button.center().x - px(120.), button.bottom() + px(1.));
            if gesture == "cancel" {
                touch(cx, TouchPhase::Cancelled, lift);
            } else {
                touch(cx, TouchPhase::Moved, away);
                if gesture != "down" {
                    assert!(cx.debug_bounds("steer-gesture-steer").is_some());
                    assert!(cx.debug_bounds("steer-gesture-steer-selected").is_none());
                    assert!(cx.debug_bounds("steer-gesture-queue-selected").is_none());
                }
                touch(cx, TouchPhase::Ended, away);
            }
            assert!(sent(cx).is_empty(), "{gesture} sends nothing");
            assert!(cx.debug_bounds("steer-gesture-steer").is_none());
            assert!(cx.debug_bounds("steer-gesture-queue").is_none());
            assert_eq!(
                composer.read_with(cx, |composer, cx| composer.draft(cx)),
                "kept"
            );
        }

        replace(
            &incoming,
            Topic::SessionStatus {
                session_id: unsupported.session_id.clone(),
            },
            ServerEvent::SessionStatusReplaced(Box::new(unsupported)),
        );
        cx.run_until_parked();
        store.update(cx, |store, cx| store.drain_host_events_for_test(cx));
        cx.update(|window, cx| _ = window.draw(cx));
        let button = cx.debug_bounds("steer-turn").unwrap();
        touch(cx, TouchPhase::Started, button.center());
        cx.executor()
            .advance_clock(gpui::GestureTuning::default().long_press_duration);
        cx.run_until_parked();
        cx.update(|window, cx| _ = window.draw(cx));
        assert!(cx.debug_bounds("steer-gesture-steer").is_none());
        assert!(cx.debug_bounds("steer-gesture-queue").is_none());
        touch(cx, TouchPhase::Cancelled, button.center());
        assert!(sent(cx).is_empty());
    }

    #[gpui::test]
    fn queue_edit_waits_for_host_removal_and_in_flight_actions_are_disabled(
        cx: &mut TestAppContext,
    ) {
        let (store, incoming, outgoing, mut status) = attach(cx);
        status.queued_messages = vec![tcode_protocol::QueuedMessageStatus {
            id: 7,
            editable: false,
            delivery_key: None,
            text: "Keep this queued until the host accepts the edit".into(),
            fire_at_unix_secs: Some(u64::MAX),
        }];
        let topic = Topic::SessionStatus {
            session_id: status.session_id.clone(),
        };
        replace(
            &incoming,
            topic.clone(),
            ServerEvent::SessionStatusReplaced(Box::new(status.clone())),
        );
        cx.run_until_parked();
        store.update(cx, |store, cx| store.drain_host_events_for_test(cx));
        let (composer, cx) =
            cx.add_window_view(|window, cx| Composer::new(store.clone(), window, cx));
        cx.simulate_resize(gpui::size(px(1024.), px(768.)));
        cx.update(|window, cx| _ = window.draw(cx));
        for action in ["queue-drop-7", "queue-steer-7"] {
            let button = cx.debug_bounds(action).expect("queue action visible");
            cx.simulate_click(button.center(), Modifiers::default());
            cx.update(|window, cx| _ = window.draw(cx));
            assert!(
                composer
                    .read_with(cx, |composer, cx| composer.draft(cx))
                    .is_empty()
            );
        }
        while let Ok(line) = outgoing.try_recv() {
            let message = tcode_protocol::decode_client_line(&line).unwrap();
            assert!(
                !matches!(
                    message.payload,
                    tcode_protocol::ClientPayload::Command(
                        tcode_protocol::Command::DropQueued { .. }
                            | tcode_protocol::Command::SteerQueued { .. }
                    )
                ),
                "disabled actions must not send queue commands"
            );
        }
        status.queued_messages[0].editable = true;
        replace(
            &incoming,
            topic.clone(),
            ServerEvent::SessionStatusReplaced(Box::new(status.clone())),
        );
        cx.run_until_parked();
        store.update(cx, |store, cx| store.drain_host_events_for_test(cx));
        cx.update(|window, cx| _ = window.draw(cx));
        let drop_button = cx.debug_bounds("queue-drop-7").unwrap();
        cx.simulate_click(drop_button.center(), Modifiers::default());
        cx.update(|window, cx| _ = window.draw(cx));
        assert!(
            composer
                .read_with(cx, |composer, cx| composer.draft(cx))
                .is_empty(),
            "the pending host drop is still queued"
        );
        assert!(
            std::iter::from_fn(|| outgoing.try_recv().ok()).any(|line| matches!(
                tcode_protocol::decode_client_line(&line).unwrap().payload,
                tcode_protocol::ClientPayload::Command(tcode_protocol::Command::DropQueued {
                    id: 7,
                    ..
                })
            )),
            "the enabled drop must reach the host"
        );
        status.queued_messages.clear();
        replace(
            &incoming,
            topic.clone(),
            ServerEvent::SessionStatusReplaced(Box::new(status.clone())),
        );
        cx.run_until_parked();
        store.update(cx, |store, cx| store.drain_host_events_for_test(cx));
        cx.update(|window, cx| _ = window.draw(cx));
        assert!(cx.debug_bounds("queue-drop-7").is_none());
        assert_eq!(
            composer.read_with(cx, |composer, cx| composer.draft(cx)),
            "Keep this queued until the host accepts the edit"
        );
        cx.update(|window, cx| {
            composer.update(cx, |composer, cx| composer.set_draft("", window, cx))
        });
        status
            .queued_messages
            .push(tcode_protocol::QueuedMessageStatus {
                id: 8,
                editable: true,
                delivery_key: None,
                text: "Queued text".into(),
                fire_at_unix_secs: None,
            });
        replace(
            &incoming,
            topic.clone(),
            ServerEvent::SessionStatusReplaced(Box::new(status.clone())),
        );
        cx.run_until_parked();
        store.update(cx, |store, cx| store.drain_host_events_for_test(cx));
        cx.update(|window, cx| _ = window.draw(cx));
        let drop_button = cx.debug_bounds("queue-drop-8").unwrap();
        cx.simulate_click(drop_button.center(), Modifiers::default());
        cx.update(|window, cx| composer.update(cx, |composer, cx| composer.focus(window, cx)));
        cx.simulate_input("New draft while the drop is pending");
        cx.run_until_parked();
        status.queued_messages.clear();
        replace(
            &incoming,
            topic,
            ServerEvent::SessionStatusReplaced(Box::new(status)),
        );
        cx.run_until_parked();
        store.update(cx, |store, cx| store.drain_host_events_for_test(cx));
        cx.update(|window, cx| _ = window.draw(cx));
        assert_eq!(
            composer.read_with(cx, |composer, cx| composer.draft(cx)),
            "New draft while the drop is pending"
        );
    }

    /// Dragging a queued row onto another asks the host to send it in that
    /// row's place; letting go where it started asks nothing.
    #[gpui::test]
    fn dragging_a_queued_row_moves_it_in_the_host_queue(cx: &mut TestAppContext) {
        let (store, incoming, outgoing, mut status) = attach(cx);
        status.queued_messages = (1..=3)
            .map(|id| tcode_protocol::QueuedMessageStatus {
                id,
                editable: true,
                delivery_key: None,
                text: format!("Queued {id}"),
                fire_at_unix_secs: None,
            })
            .collect();
        replace(
            &incoming,
            Topic::SessionStatus {
                session_id: status.session_id.clone(),
            },
            ServerEvent::SessionStatusReplaced(Box::new(status)),
        );
        cx.run_until_parked();
        store.update(cx, |store, cx| store.drain_host_events_for_test(cx));
        let (_composer, cx) =
            cx.add_window_view(|window, cx| Composer::new(store.clone(), window, cx));
        cx.simulate_resize(gpui::size(px(1024.), px(768.)));
        let draw = |cx: &mut gpui::VisualTestContext| cx.update(|window, cx| _ = window.draw(cx));
        draw(cx);
        let left = gpui::MouseButton::Left;
        let drag = |cx: &mut gpui::VisualTestContext, id: u64, to: gpui::Point<gpui::Pixels>| {
            let start = cx
                .debug_bounds(format!("queued-row-{id}").leak())
                .unwrap()
                .center();
            cx.simulate_mouse_down(start, left, Modifiers::default());
            cx.simulate_mouse_move(
                start + gpui::point(px(0.), px(8.)),
                left,
                Modifiers::default(),
            );
            draw(cx);
            cx.simulate_mouse_move(to, left, Modifiers::default());
            draw(cx);
            cx.simulate_mouse_up(to, left, Modifiers::default());
            draw(cx);
        };
        let moves = |outgoing: &async_channel::Receiver<String>| {
            std::iter::from_fn(|| outgoing.try_recv().ok())
                .filter_map(|line| {
                    match tcode_protocol::decode_client_line(&line).unwrap().payload {
                        tcode_protocol::ClientPayload::Command(
                            tcode_protocol::Command::MoveQueued { id, before, .. },
                        ) => Some((id, before)),
                        _ => None,
                    }
                })
                .collect::<Vec<_>>()
        };

        let first = cx.debug_bounds("queued-row-1").unwrap();
        drag(cx, 3, gpui::point(first.center().x, first.top() + px(2.)));
        assert_eq!(moves(&outgoing), [(3, Some(1))]);

        let last = cx.debug_bounds("queued-row-3").unwrap();
        drag(cx, 1, gpui::point(last.center().x, last.bottom() - px(2.)));
        assert_eq!(moves(&outgoing), [(1, None)]);

        let second = cx.debug_bounds("queued-row-2").unwrap().center();
        drag(cx, 2, second);
        assert!(moves(&outgoing).is_empty(), "a row dropped in place stays");
    }

    /// Choosing a thread from the `@` menu references it: the host is asked to
    /// add the reference and the draft keeps no mention text.
    #[gpui::test]
    fn an_at_thread_is_referenced_rather_than_written_into_the_draft(cx: &mut TestAppContext) {
        let (store, incoming, outgoing, mut status) = attach(cx);
        status.draft = false;
        status.control = Some(tcode_protocol::ControlStatus {
            grant: tcode_core::control::ControlGrant::default_grant(),
            source: tcode_core::control::ControlGrantSource::Default,
            pending_requests: Vec::new(),
            references: Vec::new(),
            self_settle: None,
            tools_offered: true,
        });
        let mut current =
            tcode_core::project::SessionMeta::new(status.provider, std::env::temp_dir(), None);
        current.id = status.session_id.clone();
        current.title = "Current".into();
        let mut other = current.clone();
        other.id = "release-thread".into();
        other.title = "Release notes".into();
        replace(
            &incoming,
            Topic::Index,
            ServerEvent::IndexSnapshot(tcode_protocol::IndexSnapshot {
                summary: Default::default(),
                sessions: vec![current, other],
                projects: Vec::new(),
            }),
        );
        replace(
            &incoming,
            Topic::SessionStatus {
                session_id: status.session_id.clone(),
            },
            ServerEvent::SessionStatusReplaced(Box::new(status)),
        );
        cx.run_until_parked();
        store.update(cx, |store, cx| store.drain_host_events_for_test(cx));
        let (composer, cx) =
            cx.add_window_view(|window, cx| Composer::new(store.clone(), window, cx));
        cx.update(|window, cx| {
            window.draw(cx).clear(cx);
            composer.update(cx, |composer, cx| composer.focus(window, cx));
        });
        cx.simulate_input("@release");
        cx.update(|window, cx| window.draw(cx).clear(cx));
        composer.read_with(cx, |composer, cx| {
            assert_eq!(composer.menu_rows(cx).0[0].primary, "Release notes");
        });
        let row = cx.debug_bounds("menu-row-0").expect("thread row");
        cx.simulate_click(row.center(), Modifiers::default());
        cx.update(|window, cx| window.draw(cx).clear(cx));
        assert_eq!(
            composer.read_with(cx, |composer, cx| composer.draft(cx)),
            ""
        );
        cx.run_until_parked();
        assert!(
            std::iter::from_fn(|| outgoing.try_recv().ok()).any(|line| matches!(
                tcode_protocol::decode_client_line(&line).unwrap().payload,
                tcode_protocol::ClientPayload::Command(
                    tcode_protocol::Command::AddThreadReference { thread_id, .. }
                ) if thread_id == "release-thread"
            )),
            "no AddThreadReference was sent"
        );
    }
}
