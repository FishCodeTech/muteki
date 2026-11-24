use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::sync::Arc;
use std::time::Duration;
#[cfg(not(target_family = "wasm"))]
use std::time::Instant;
#[cfg(target_family = "wasm")]
use web_time::Instant;

use std::path::{Path, PathBuf};
use std::rc::Rc;

pub(crate) mod components;
pub(crate) mod model;
pub(crate) use model::format_duration;
mod residency;

use crate::markdown::{SelectionArea, SelectionDocument};
use crate::overlay::OverlayExt as _;
use crate::scroll::ScrollableElement as _;
use crate::theme::ActiveTheme as _;
use crate::typography::TextRole as _;
use crate::widgets::button::{Button, ButtonVariants as _};
use crate::widgets::copy::CopiedMark;
use crate::widgets::menu::{ContextMenuExt as _, PopupMenu, open_in_zed};
use crate::widgets::tooltip::Tooltip;
use crate::{
    icon::{Icon, IconName},
    sizing::Sizable as _,
};
use agent::{ItemContent, RewindMode};
use gpui::{
    Action, Anchor, AnyElement, App, AppContext as _, ClickEvent, ClipboardItem, Context, Entity,
    FollowMode, InteractiveElement as _, IntoElement, ListAlignment, ListOffset, ListState,
    ParentElement as _, Render, Role, SharedString, StatefulInteractiveElement as _, Styled as _,
    Subscription, Task, Window, div, list, prelude::FluentBuilder as _, px,
};
use gpui_base::{ElementExt as _, Scrollbar, StyledExt as _, h_flex, v_flex};
use serde::Deserialize;

use tcode_core::git::GitAction;
use tcode_core::notice::{HostNotice, OrchestrateCallback};
use tcode_core::session::{EntryContent, SteeringStatus, Timeline, TimelineEntry};
use tcode_core::ui::RightTab;

use crate::commit_dialog::CommitDialog;
use crate::composer::Composer;
use crate::git::{git_action_label_key, git_hint_key};
use crate::shortcut::format_secondary_shortcut;
use crate::store::WorkspaceStore;
use crate::terminal_drawer::TerminalDrawer;
use crate::time::now_secs;
use crate::window_caption;
use crate::window_drag_area;
use crate::window_state::{NavigationPanel, OpenPanel, WindowState};

use self::components::assistant::MdState;

/// A message's context-menu rewind, which the chat applies as the bubble's
/// rewind popover does.
#[derive(Action, Clone, PartialEq, Eq, Deserialize)]
#[action(namespace = tcode_chat, no_json)]
pub(crate) struct RewindTurn {
    pub(crate) turn: usize,
    pub(crate) mode: RewindMode,
}

gpui::actions!(tcode_chat, [OpenMoveThreadDialog, RevokeSelfSettle]);

/// A changed-file chip's context-menu "View diff".
#[derive(Action, Clone, PartialEq, Eq, Deserialize)]
#[action(namespace = tcode_chat, no_json)]
pub(crate) struct OpenFileDiff {
    pub(crate) turn: usize,
    pub(crate) path: String,
}

use self::components::changed_files::InlineDiffCache;
use self::components::command_panel::CommandPanelCache;
use self::model::{
    LimitResumeAction, ListSync, RowPart, RowRenderArgs, Segment, TimelineContinuity, TimelineRow,
    TurnIndexCache, activity_run_duration_ms, displayed_error_text, divergent_served_model,
    format_elapsed_deciseconds, latest_message_ids, limit_resume_action, live_edit_counts,
    live_edit_rows, notice_keys, partition_activity_run, plain_text_as_markdown, row_of_entry,
    rows_of_turn, segment_entries, start_hub_projects, timeline_overdraw, user_content,
    user_notice, user_visible_text, work_log_capsule_label, work_log_counts, work_log_key,
    work_log_outcome,
};
use self::residency::{
    MarkdownEntry, ResidencyInput, ResidencyScope, decide, tail_row_window, viewport_row_window,
};
pub(crate) use crate::material::{
    CHAT_CONTENT_MAX_WIDTH as CONTENT_MAX_WIDTH, CHAT_CONTENT_MIN_PADDING as CONTENT_MIN_PADDING,
};
/// Below this header width the git and Open split buttons drop their text and
/// keep their icon and chevron. The header with its longest git label, five
/// panel toggles and a title kept to a few words beside a "+10" pull request
/// badge measures under 672pt in English, the widest shipped locale.
/// [`header_extra_width`] adds what a thread's header holds beyond that.
const HEADER_LABELS_MIN_WIDTH: f32 = 672.;
pub(crate) const HEADER_COMPACT_MIN_WIDTH: f32 = 532.;
/// The way back to the parent thread, each with the 8pt gap that follows it.
/// Its arrow alone measures 20pt. With the parent's title cut to its
/// 24-character maximum it measured 215pt for an ordinary title; 265pt leaves
/// room for a title of wide letters.
const PARENT_LINK_COMPACT_WIDTH: f32 = 28.;
const PARENT_LINK_LABELLED_WIDTH: f32 = 273.;

/// The thread the active one was reached from: its id and title.
fn parent_session(store: &WorkspaceStore) -> Option<(String, String)> {
    let active = store.active_session_id()?;
    let sessions = store.sidebar_sessions();
    let parent_id = sessions
        .iter()
        .find(|meta| meta.id == active)?
        .parent_session_id
        .clone()?;
    let parent = sessions.iter().find(|meta| meta.id == parent_id)?;
    Some((parent_id, parent.title.clone()))
}

/// The header's left inset in place of the bar's own. Collapsed, the sidebar
/// has zero width and the header starts at the window's left edge, where macOS
/// draws the traffic lights — but only when it draws them: they are hidden in
/// fullscreen, and other platforms never had them. The budget counts the whole
/// inset, not its excess over the bar's, which leaves the widths a little slack.
fn traffic_light_clearance(sidebar_collapsed: bool, fullscreen: bool) -> f32 {
    if cfg!(target_os = "macos") && sidebar_collapsed && !fullscreen {
        crate::window_caption::TRAFFIC_LIGHT_INSET
    } else {
        0.
    }
}

/// How much wider than the plain header the active thread's header is, with
/// its split buttons labelled or not: the padding that clears the macOS
/// traffic lights, and the parent link.
pub(crate) fn header_extra_width(
    store: &WorkspaceStore,
    sidebar_collapsed: bool,
    fullscreen: bool,
    labelled: bool,
) -> f32 {
    let parent_link = match (parent_session(store), labelled) {
        (None, _) => 0.,
        (Some(_), true) => PARENT_LINK_LABELLED_WIDTH,
        (Some(_), false) => PARENT_LINK_COMPACT_WIDTH,
    };
    traffic_light_clearance(sidebar_collapsed, fullscreen) + parent_link
}
/// Vertical rhythm between turns. Turns are separated by space and typographic
/// hierarchy alone — there is deliberately no rule/divider under the user bubble.
const TURN_GAP: f32 = 32.;
/// Vertical rhythm between the segments of one turn.
const SEGMENT_GAP: f32 = 10.;
/// Large documents are parsed away from the UI executor before becoming resident.
const ASYNC_MARKDOWN_THRESHOLD_BYTES: usize = 4 * 1024;
/// Target minimum time for the latest activity and its immediate predecessor.
/// An activity with two newer successors folds immediately instead.
const AUTO_ACTIVITY_MIN_VISIBILITY: Duration = Duration::from_millis(500);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AutoActivityExpansion {
    Manual {
        expanded: bool,
    },
    Expanded {
        visible_since: Instant,
    },
    CollapsePending {
        generation: u64,
        visible_since: Instant,
    },
    Collapsed,
}

#[derive(Debug, Default)]
struct AutoActivityExpansions {
    entries_by_session: HashMap<String, HashMap<String, AutoActivityExpansion>>,
    /// Expandable details already present when the active session is hydrated.
    /// Each key is consumed on first render so later live updates use the
    /// ordinary minimum-visibility state machine.
    session_snapshot: Option<AutoActivitySessionSnapshot>,
    next_generation: u64,
}

#[derive(Debug)]
struct AutoActivitySessionSnapshot {
    session_key: String,
    /// `None` while the session status has switched but its asynchronous
    /// timeline snapshot has not arrived yet.
    pending_keys: Option<HashSet<String>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AutoActivityRecency {
    Latest,
    ImmediatelySuperseded,
    Older,
}

impl AutoActivityRecency {
    fn from_newer_activity_count(count: usize) -> Self {
        match count {
            0 => Self::Latest,
            1 => Self::ImmediatelySuperseded,
            _ => Self::Older,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct AutoActivityObservation {
    expanded: bool,
    collapse: Option<(u64, Duration)>,
}

impl AutoActivityExpansions {
    fn activate_session(&mut self, session_key: Option<&str>) {
        self.session_snapshot = session_key.map(|session_key| AutoActivitySessionSnapshot {
            session_key: session_key.to_string(),
            pending_keys: None,
        });
    }

    fn awaiting_session_snapshot(&self, session_key: Option<&str>) -> bool {
        self.session_snapshot.as_ref().is_some_and(|snapshot| {
            Some(snapshot.session_key.as_str()) == session_key && snapshot.pending_keys.is_none()
        })
    }

    fn hydrate_session_snapshot(&mut self, session_key: &str, keys: HashSet<String>) {
        if let Some(snapshot) = self
            .session_snapshot
            .as_mut()
            .filter(|snapshot| snapshot.session_key == session_key)
            && snapshot.pending_keys.is_none()
        {
            snapshot.pending_keys = Some(keys);
        }
    }

    fn observe(
        &mut self,
        session_key: &str,
        key: &str,
        enabled: bool,
        recency: AutoActivityRecency,
        now: Instant,
    ) -> AutoActivityObservation {
        let current = self
            .entries_by_session
            .get(session_key)
            .and_then(|entries| entries.get(key))
            .copied();
        if let Some(AutoActivityExpansion::Manual { expanded }) = current {
            return AutoActivityObservation {
                expanded,
                collapse: None,
            };
        }

        if !enabled {
            if let Some(entries) = self.entries_by_session.get_mut(session_key) {
                entries.remove(key);
            }
            return AutoActivityObservation {
                expanded: false,
                collapse: None,
            };
        }

        let first_snapshot_observation = self
            .session_snapshot
            .as_mut()
            .filter(|snapshot| snapshot.session_key == session_key)
            .and_then(|snapshot| snapshot.pending_keys.as_mut())
            .is_some_and(|keys| keys.remove(key));
        if first_snapshot_observation {
            let latest = recency == AutoActivityRecency::Latest;
            let state = if latest {
                AutoActivityExpansion::Expanded { visible_since: now }
            } else {
                AutoActivityExpansion::Collapsed
            };
            self.insert(session_key, key, state);
            return AutoActivityObservation {
                expanded: latest,
                collapse: None,
            };
        }

        if recency == AutoActivityRecency::Older {
            // During rapid bursts, keep at most the current detail and its
            // immediate predecessor open; a pending timer must not hold an
            // activity open after a second successor arrives.
            self.insert(session_key, key, AutoActivityExpansion::Collapsed);
            return AutoActivityObservation {
                expanded: false,
                collapse: None,
            };
        }

        let visible_since = match current {
            Some(AutoActivityExpansion::Expanded { visible_since })
            | Some(AutoActivityExpansion::CollapsePending { visible_since, .. }) => visible_since,
            _ => now,
        };
        if recency == AutoActivityRecency::Latest {
            self.insert(
                session_key,
                key,
                AutoActivityExpansion::Expanded { visible_since },
            );
            return AutoActivityObservation {
                expanded: true,
                collapse: None,
            };
        }

        match current {
            Some(AutoActivityExpansion::CollapsePending { .. }) => {
                return AutoActivityObservation {
                    expanded: true,
                    collapse: None,
                };
            }
            Some(AutoActivityExpansion::Collapsed) => {
                return AutoActivityObservation {
                    expanded: false,
                    collapse: None,
                };
            }
            _ => {}
        }
        let remaining = AUTO_ACTIVITY_MIN_VISIBILITY
            .saturating_sub(now.saturating_duration_since(visible_since));
        if remaining.is_zero() {
            self.insert(session_key, key, AutoActivityExpansion::Collapsed);
            AutoActivityObservation {
                expanded: false,
                collapse: None,
            }
        } else {
            self.next_generation = self.next_generation.wrapping_add(1);
            let generation = self.next_generation;
            self.insert(
                session_key,
                key,
                AutoActivityExpansion::CollapsePending {
                    generation,
                    visible_since,
                },
            );
            AutoActivityObservation {
                expanded: true,
                collapse: Some((generation, remaining)),
            }
        }
    }

    fn finish_collapse(&mut self, session_key: &str, key: &str, generation: u64) -> bool {
        if matches!(
            self.entries_by_session
                .get(session_key)
                .and_then(|entries| entries.get(key)),
            Some(AutoActivityExpansion::CollapsePending {
                generation: pending,
                ..
            }) if *pending == generation
        ) {
            self.insert(session_key, key, AutoActivityExpansion::Collapsed);
            true
        } else {
            false
        }
    }

    fn insert(&mut self, session_key: &str, key: &str, state: AutoActivityExpansion) {
        self.entries_by_session
            .entry(session_key.to_string())
            .or_default()
            .insert(key.to_string(), state);
    }
}

fn auto_activity_snapshot_keys(entries: &[Arc<TimelineEntry>]) -> HashSet<String> {
    let mut keys = HashSet::new();
    for entry in entries {
        match &entry.content {
            EntryContent::Item(ItemContent::CommandExecution { .. }) => {
                keys.insert(format!("activity-{}", entry.id));
            }
            EntryContent::Item(ItemContent::FileChange { changes, .. }) => {
                for (file_index, change) in changes.iter().enumerate() {
                    if live_edit_counts(change.diff.as_deref()).is_some() {
                        keys.insert(format!("activity-{}-file-{file_index}", entry.id));
                    }
                }
            }
            _ => {}
        }
    }
    keys
}

struct PendingMarkdownBuild {
    generation: u64,
    session_key: Option<String>,
    desired_text: String,
}

/// A whole tool output fetched on request.
enum FullOutput {
    /// Dropping the task cancels the fetch.
    Loading {
        _request: Task<()>,
    },
    Loaded(SharedString),
    Failed(String),
}

pub struct ChatView {
    workspace_store: Entity<WorkspaceStore>,
    pull_requests: Entity<crate::pull_requests::PullRequestsPanel>,
    window_state: Entity<WindowState>,
    composer: Entity<Composer>,
    terminal_drawer: Entity<TerminalDrawer>,
    /// The Agents sheet's content on phone.
    agents: Entity<crate::agents_panel::AgentsPanel>,
    terminal_was_open: bool,
    list_state: ListState,
    /// The timeline changed since the list last mirrored it. GPUI's list
    /// resolves wheel and pan packets against the row indices it painted, so
    /// rows are only spliced during a frame, never between two frames.
    timeline_stale: bool,
    history_placeholder_height: gpui::Pixels,
    /// Height of the phone row floating over the foot of the timeline, zero
    /// while it is absent. The last row's padding holds it so the tail clears it.
    phone_strip_height: gpui::Pixels,
    /// Distance to scroll back once a page has filled the reservation the
    /// reader had scrolled into. Applied by the render after the frame that
    /// measured the page, never between frames: GPUI resolves the wheel and
    /// pan packets of a moving finger against the anchor it painted, so a
    /// scroll applied between two frames is either overwritten by the next
    /// packet or would have to give up when one arrived first.
    reservation_scroll_back: Option<ReservationScrollBack>,
    /// The anchor was carried into rows not yet measured.
    carried_anchor: bool,
    rows: Vec<TimelineRow>,
    turn_index_cache: TurnIndexCache,
    md_states: HashMap<String, MdState>,
    /// The transcript's one text selection, across its messages; each
    /// message is the entry at its index in the timeline. It goes when the
    /// focus leaves the whole view.
    selection: Entity<SelectionDocument>,
    /// Holds the focus of everything the view draws, the composer included,
    /// so the selection knows when the focus has left it.
    focus_handle: gpui::FocusHandle,
    pending_md_builds: HashMap<String, PendingMarkdownBuild>,
    next_md_build_generation: u64,
    markdown_visible_rows: Range<usize>,
    markdown_scroll_top: Option<usize>,
    /// The rows the timeline paints in the frame being drawn: GPUI's list
    /// prepaints only the rows on screen.
    painted_rows: Rc<Cell<Option<(usize, usize)>>>,
    /// The first row painted while following the tail.
    painted_tail_start: Option<usize>,
    /// Open/closed keys for collapsibles other than activity details.
    expanded: HashSet<String>,
    auto_activity_expansions: AutoActivityExpansions,
    command_panels: RefCell<CommandPanelCache>,
    inline_diffs: InlineDiffCache,
    /// Whole tool outputs fetched for the open session, by item id.
    full_outputs: HashMap<String, FullOutput>,
    session_key: Option<String>,
    /// Turn selected from a command-palette content hit.
    highlighted_turn: Option<usize>,
    /// 1s ticker kept alive while an error card shows a scheduled resume.
    _limit_tick: Option<Task<()>>,
    /// Keyed `plan`, `user:<id>`, `assistant:<id>` or `error:<id>`.
    copied: CopiedMark,
    /// The live commit dialog entity while it is open (kept alive across frames).
    commit_dialog: Option<Entity<CommitDialog>>,
    /// A system scrolling screenshot is driving the timeline: where it was
    /// before the capture moved it. History paging and tail following stay
    /// off until the capture ends so the origin keeps meaning the same pixel.
    capture: Option<CaptureSession>,
    _subscriptions: Vec<Subscription>,
    #[cfg(test)]
    markdown_remeasured_rows: Vec<usize>,
}

// ListState retains measured row heights, so variable-height turns use the same
// pixel geometry as scrolling rather than treating a long turn as one short row.
fn history_screens_covered(list: &ListState, leading: gpui::Pixels) -> Option<f32> {
    let height = list.viewport_bounds().size.height;
    (height > px(0.)).then(|| {
        let above = -list.scroll_px_offset_for_scrollbar().y - leading;
        f32::from(above.max(px(0.))) / f32::from(height)
    })
}

fn jump_to_latest_visible(list: &ListState) -> bool {
    let height = list.viewport_bounds().size.height;
    // The scrollbar geometry retains size hints for invalidated rows. Unlike
    // is_scrolled_to_end(), it does not require every offscreen row to have
    // been measured. Prepending shifts the extent and offset together.
    height > px(0.)
        && list.max_offset_for_scrollbar().y + list.scroll_px_offset_for_scrollbar().y > height
}

/// Move an anchor whose offset passes the end of its row onto the row that
/// offset lands in. A remeasure of the anchor row would otherwise clamp the
/// offset to that row's height. False while a row it passes is unmeasured.
fn settle_carried_anchor(list: &ListState) -> bool {
    if list.is_following_tail() {
        return true;
    }
    let mut anchor = list.logical_scroll_top();
    let start = anchor.item_ix;
    let settled = loop {
        if anchor.item_ix + 1 >= list.item_count() {
            break true;
        }
        let Some(bounds) = list.bounds_for_item(anchor.item_ix) else {
            break false;
        };
        if anchor.offset_in_item < bounds.size.height {
            break true;
        }
        anchor.offset_in_item -= bounds.size.height;
        anchor.item_ix += 1;
    };
    if anchor.item_ix != start {
        list.scroll_to(anchor);
    }
    settled
}

/// Pixels between the top of the content and the top of the viewport. A list
/// anchored past its last row reports the whole content height; on screen it
/// is scrolled no further than the end.
fn capture_scrolled(list: &ListState) -> gpui::Pixels {
    (-list.scroll_px_offset_for_scrollbar().y).min(list.max_offset_for_scrollbar().y)
}

struct CaptureSession {
    anchor: ListOffset,
    following: bool,
    /// The row at the top of the viewport when the capture began; capture
    /// offsets are measured from it. A row, not a pixel count: rows measured
    /// for the first time during the capture change every pixel count above
    /// them.
    origin: ListOffset,
}

/// One tile's scroll, in pixels from the capture's starting top.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CaptureScroll {
    /// The offset reached: smaller than asked past the end of the measured
    /// content, larger above its start.
    pub reached: gpui::Pixels,
    /// Whether the scroll changed the list, so the frame on screen is stale.
    pub moved: bool,
}

/// The timeline as the surface of the platform's scrolling screenshot.
///
/// The capturer works in pixels relative to the viewport at the start of the
/// capture: it asks for the content that would sit `offset` below that top,
/// the timeline scrolls there as far as measured content allows, and the
/// platform reads back the next painted frame. The history reservation above
/// the first turn is blank and never captured.
impl ChatView {
    /// The timeline's on-screen rectangle, when it has something to capture.
    pub(crate) fn capture_viewport(&self) -> Option<gpui::Bounds<gpui::Pixels>> {
        let bounds = self.list_state.viewport_bounds();
        (!self.rows.is_empty() && bounds.size.height > px(0.) && bounds.size.width > px(0.))
            .then_some(bounds)
    }

    pub(crate) fn capture_begin(&mut self, cx: &mut Context<Self>) -> bool {
        if self.capture_viewport().is_none() {
            return false;
        }
        if self.capture.is_none() {
            let list = &self.list_state;
            let anchor = list.logical_scroll_top();
            let following = list.is_following_tail();
            // Layout re-arms tail following whenever the bottom comes into
            // view, which the capture's last tile does; `Normal` keeps the
            // list where the capture put it.
            list.set_follow_mode(FollowMode::Normal);
            // An anchor past the last row is laid out from the end, but
            // `scroll_by` counts its pixel offset from the top of every row:
            // a scroll relative to it would land a viewport short. Re-anchor
            // at the row on screen first.
            if anchor.item_ix >= list.item_count() {
                list.scroll_by(-list.viewport_bounds().size.height);
            }
            self.capture = Some(CaptureSession {
                anchor,
                following,
                origin: list.logical_scroll_top(),
            });
            self.reservation_scroll_back = None;
            cx.notify();
        }
        true
    }

    /// Scroll so the content `offset` pixels below the capture's starting top
    /// is at the top of the viewport. `None` outside a capture.
    pub(crate) fn capture_scroll(
        &mut self,
        offset: gpui::Pixels,
        cx: &mut Context<Self>,
    ) -> Option<CaptureScroll> {
        let session = self.capture.as_ref()?;
        let list = &self.list_state;
        let before = list.logical_scroll_top();
        // Pixel counts are only comparable within one measurement of the
        // rows, so the origin's is taken now, along with the target's.
        list.scroll_to(session.origin);
        // `scroll_by` counts from the top of every row even past the end,
        // where layout shows the list scrolled no further than the end.
        let origin = -list.scroll_px_offset_for_scrollbar().y;
        let end = list.max_offset_for_scrollbar().y;
        let target = (origin + offset)
            .min(end)
            .max(self.history_placeholder_height)
            .max(px(0.));
        list.scroll_by(target - origin);
        let after = list.logical_scroll_top();
        let moved =
            after.item_ix != before.item_ix || after.offset_in_item != before.offset_in_item;
        if moved {
            cx.notify();
        }
        Some(CaptureScroll {
            reached: capture_scrolled(list) - origin.min(end),
            moved,
        })
    }

    /// Whether a frame painted now shows the rows' content rather than a
    /// Markdown placeholder still being built.
    pub(crate) fn capture_settled(&self) -> bool {
        self.pending_md_builds.is_empty()
    }

    pub(crate) fn capture_end(&mut self, cx: &mut Context<Self>) {
        let Some(session) = self.capture.take() else {
            return;
        };
        let list = &self.list_state;
        list.set_follow_mode(FollowMode::Tail);
        if !session.following {
            list.scroll_to(session.anchor);
        }
        cx.notify();
    }
}

impl ChatView {
    /// The composer itself declines on a mobile build, where focus would
    /// raise the software keyboard; a desktop window of any width types.
    pub fn focus_composer(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.composer
            .update(cx, |composer, cx| composer.focus(window, cx));
    }

    #[cfg(test)]
    pub(crate) fn composer(&self) -> Entity<crate::composer::Composer> {
        self.composer.clone()
    }

    /// The thread's terminal workspace. Compact windows have no room to split
    /// it under the timeline, so the shell shows this same entity as its own
    /// destination rather than compiling the portable terminal away.
    pub(crate) fn terminal_drawer(&self) -> Entity<TerminalDrawer> {
        self.terminal_drawer.clone()
    }
    pub fn new(
        workspace_store: Entity<WorkspaceStore>,
        window_state: Entity<WindowState>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let compact = window_state.read(cx).compact;
        let composer = cx.new(|cx| {
            if compact {
                Composer::new_with_layout(workspace_store.clone(), true, window, cx)
            } else {
                Composer::new(workspace_store.clone(), window, cx)
            }
        });
        // Measure enough rows to establish the six-screen window even when
        // distant rows still have unknown heights in ListState's summary.
        let height = f32::from(window.bounds().size.height);
        let overdraw =
            timeline_overdraw(height).max(height * (crate::store::HISTORY_WINDOW_SCREENS + 1.));
        let list_state = ListState::new(0, ListAlignment::Bottom, px(overdraw));
        list_state.set_follow_mode(FollowMode::Tail);
        let chat = cx.entity().downgrade();
        list_state.set_scroll_handler(move |event, window, cx| {
            let visible_rows = event.visible_range.clone();
            let chat = chat.clone();
            window.defer(cx, move |_, cx| {
                let _ = chat.update(cx, |chat, cx| {
                    chat.set_markdown_visible_rows(visible_rows, cx);
                });
            });
        });

        let selection = transcript_selection(cx.entity().downgrade(), cx);
        let focus_handle = cx.focus_handle();
        let subscriptions = vec![
            cx.on_focus_out(&focus_handle, window, |this, _, _, cx| {
                this.selection
                    .update(cx, |selection, cx| selection.clear(cx));
            }),
            cx.observe_in(&workspace_store, window, |this, store, window, cx| {
                this.timeline_stale = true;
                // Desktop can type immediately after opening the terminal.
                // On software-keyboard devices this also runs during restore,
                // so wait for a tap on the grid before raising the keyboard.
                let open = store.read(cx).panel_state().terminal_open;
                if open && !this.terminal_was_open {
                    let drawer = this.terminal_drawer.clone();
                    window.defer(cx, move |window, cx| {
                        if !crate::window_seam::is_mobile(cx) {
                            gpui::Focusable::focus_handle(drawer.read(cx), cx).focus(window, cx);
                        }
                    });
                }
                this.terminal_was_open = open;
                cx.notify();
            }),
        ];
        let terminal_drawer = cx.new(|cx| TerminalDrawer::new(workspace_store.clone(), window, cx));
        let pull_requests = cx.new(|cx| {
            crate::pull_requests::PullRequestsPanel::new(
                workspace_store.clone(),
                window_state.clone(),
                cx,
            )
        });
        let agents =
            cx.new(|cx| crate::agents_panel::AgentsPanel::for_sheet(workspace_store.clone(), cx));
        let terminal_was_open = workspace_store.read(cx).panel_state().terminal_open;

        let mut this = Self {
            pull_requests,
            workspace_store,
            window_state,
            composer,
            terminal_drawer,
            agents,
            terminal_was_open,
            list_state,
            timeline_stale: false,
            history_placeholder_height: px(0.),
            phone_strip_height: px(0.),
            reservation_scroll_back: None,
            carried_anchor: false,
            rows: Vec::new(),
            turn_index_cache: TurnIndexCache::default(),
            md_states: HashMap::new(),
            selection,
            focus_handle,
            pending_md_builds: HashMap::new(),
            next_md_build_generation: 0,
            markdown_visible_rows: 0..0,
            markdown_scroll_top: None,
            painted_rows: Rc::default(),
            painted_tail_start: None,
            expanded: HashSet::new(),
            auto_activity_expansions: AutoActivityExpansions::default(),
            command_panels: RefCell::new(CommandPanelCache::new()),
            inline_diffs: InlineDiffCache::new(),
            full_outputs: HashMap::new(),
            session_key: None,
            highlighted_turn: None,
            _limit_tick: None,
            copied: CopiedMark::default(),
            commit_dialog: None,
            capture: None,
            _subscriptions: subscriptions,
            #[cfg(test)]
            markdown_remeasured_rows: Vec::new(),
        };
        this.sync_markdown_states(cx);
        this
    }

    /// Mirror timeline markdown text into synchronous [`MarkdownState`] entities.
    fn sync_markdown_states(&mut self, cx: &mut Context<Self>) {
        self.timeline_stale = false;
        // ListState owns follow intent: user scrolling pauses Tail until the
        // bottom is reached again. Content updates must not override that pause.
        let session_key = self.workspace_store.read(cx).active_session_id();
        let session_changed = session_key != self.session_key;
        if session_changed {
            self.expanded.clear();
            self.auto_activity_expansions
                .activate_session(session_key.as_deref());
        }
        let awaiting_activity_snapshot = self
            .auto_activity_expansions
            .awaiting_session_snapshot(session_key.as_deref());
        let continuity = if session_changed {
            TimelineContinuity::NewSession
        } else if self.workspace_store.read(cx).history_available() {
            TimelineContinuity::PartialFirstTurn
        } else {
            TimelineContinuity::Complete
        };
        let (list_sync, activity_snapshot_keys) = self
            .workspace_store
            .read(cx)
            .with_active_timeline(|timeline| {
                let list_sync = self.turn_index_cache.sync(
                    &mut self.rows,
                    &timeline.turns,
                    &timeline.entries,
                    &self.expanded,
                    continuity,
                );
                let activity_snapshot_keys = awaiting_activity_snapshot
                    .then(|| auto_activity_snapshot_keys(&timeline.entries));
                (list_sync, activity_snapshot_keys)
            })
            .unwrap_or_else(|| {
                let list_sync = self.turn_index_cache.sync(
                    &mut self.rows,
                    &[],
                    &[],
                    &self.expanded,
                    continuity,
                );
                (list_sync, None)
            });

        if let Some(session_key) = session_key.as_deref()
            && let Some(keys) = activity_snapshot_keys
        {
            self.auto_activity_expansions
                .hydrate_session_snapshot(session_key, keys);
        }

        let requested_turn = session_key
            .as_deref()
            .and_then(|session_id| self.workspace_store.read(cx).pending_chat_turn(session_id));
        if session_changed {
            self.md_states.clear();
            self.selection
                .update(cx, |selection, cx| selection.clear(cx));
            self.pending_md_builds.clear();
            self.command_panels.borrow_mut().clear();
            self.inline_diffs.clear();
            self.full_outputs.clear();
            self.highlighted_turn = None;
            self.session_key = session_key;
            self.painted_tail_start = None;
            self.markdown_visible_rows = tail_row_window(self.rows.len());
            self.markdown_scroll_top = Some(self.rows.len());
        }

        match list_sync {
            ListSync::None => {}
            ListSync::Prepend { count, remeasure } => {
                let anchor = self.list_state.logical_scroll_top();
                let following = self.list_state.is_following_tail();
                self.list_state.splice(0..0, count);
                // A page requested before a capture began still lands during
                // it; the capture's anchor names the same turn as before.
                if let Some(capture) = &mut self.capture {
                    capture.anchor.item_ix += count;
                    capture.origin.item_ix += count;
                }
                // The edge padding and the reservation move to the new first
                // turn. Remove them from the former first turn without moving
                // its content.
                self.list_state.remeasure_items(count..count + 1);
                if anchor.item_ix == 0 && !following {
                    let into_reservation = self.leading_space() - anchor.offset_in_item;
                    let anchor = ListOffset {
                        item_ix: count,
                        offset_in_item: (-into_reservation).max(px(0.)),
                    };
                    self.list_state.scroll_to(anchor);
                    // A reader inside the padding or the reservation keeps that
                    // pixel position, which now belongs to the page above. A
                    // list anchor cannot precede its row (rows above it stay
                    // unpainted), so the render after the next frame walks
                    // back over the page once layout has measured it.
                    if into_reservation > px(0.) {
                        let distance = self
                            .reservation_scroll_back
                            .take()
                            .map_or(px(0.), |back| back.distance);
                        self.reservation_scroll_back = Some(ReservationScrollBack {
                            distance: distance + into_reservation,
                            measured: false,
                        });
                    }
                }
                for index in remeasure {
                    self.list_state.remeasure_items(index..index + 1);
                }
            }
            ListSync::Reset { count } => {
                self.reservation_scroll_back = None;
                self.list_state.reset(count);
                if session_changed {
                    // Reset also clears stale item focus handles. A newly opened
                    // session always starts actively following its tail.
                    self.list_state.set_follow_mode(FollowMode::Tail);
                }
            }
            ListSync::Incremental {
                splices,
                remeasure,
                carried,
            } => {
                let anchor = self.list_state.logical_scroll_top();
                let carried_offset = (!self.list_state.is_following_tail()
                    && carried.iter().any(|range| range.contains(&anchor.item_ix)))
                .then_some(anchor.offset_in_item);
                for (range, count) in splices.into_iter().rev() {
                    // The row above a splice changes its padding (the former
                    // last row hands its edge padding to the new one); its
                    // cached height must not keep it.
                    if range.start > 0 {
                        self.list_state
                            .remeasure_items(range.start - 1..range.start);
                    }
                    self.list_state.splice(range, count);
                }
                for index in remeasure {
                    self.list_state.remeasure_items(index..index + 1);
                }
                // A splice over the anchor moves it to the first new row at
                // offset zero; the carried content starts where the old row
                // did, so the old offset still names the same pixel. It may
                // pass that row's end until `settle_carried_anchor` measures it.
                if let Some(offset) = carried_offset {
                    self.list_state.scroll_to(ListOffset {
                        item_ix: self.list_state.logical_scroll_top().item_ix,
                        offset_in_item: offset,
                    });
                    self.carried_anchor = true;
                }
            }
        }

        if self.list_state.is_following_tail() {
            self.markdown_visible_rows = self.markdown_tail_window();
            self.markdown_scroll_top = Some(self.rows.len());
        }

        let requested_row = requested_turn.and_then(|turn| rows_of_turn(&self.rows, turn).next());
        if let (Some(turn), Some(row)) = (requested_turn, requested_row) {
            self.reservation_scroll_back = None;
            self.list_state.pause_following_tail();
            self.list_state.scroll_to(ListOffset {
                item_ix: row,
                offset_in_item: px(0.),
            });
            self.highlighted_turn = Some(turn);
            self.markdown_visible_rows = viewport_row_window(row, self.rows.len());
            self.markdown_scroll_top = Some(row);
            if let Some(session_id) = self.session_key.as_deref() {
                self.workspace_store.update(cx, |store, _cx| {
                    store.take_pending_chat_turn(session_id, turn);
                });
            }
        }

        self.sync_markdown_residency(requested_row, cx);

        let scheduled_limit_resume = {
            let store = self.workspace_store.read(cx);
            let deadlines: HashSet<u64> = store
                .composer_state()
                .queue
                .into_iter()
                .flat_map(|queue| queue.messages)
                .filter_map(|message| message.fire_at_unix_secs)
                .collect();
            store
                .with_active_timeline(|timeline| {
                    timeline
                        .turn_retry
                        .as_ref()
                        .is_some_and(|retry| retry.retry_at.is_some())
                        || timeline.entries.iter().any(|entry| {
                            matches!(
                                entry.content,
                                EntryContent::Error {
                                    limit_resets_at: Some(resets_at),
                                    ..
                                } if deadlines.contains(&resets_at)
                            )
                        })
                })
                .unwrap_or(false)
        };
        if scheduled_limit_resume && self._limit_tick.is_none() {
            self._limit_tick = Some(cx.spawn(async move |this, cx| {
                loop {
                    cx.background_executor().timer(Duration::from_secs(1)).await;
                    if this.update(cx, |_, cx| cx.notify()).is_err() {
                        break;
                    }
                }
            }));
        } else if !scheduled_limit_resume {
            self._limit_tick = None;
        }
    }

    fn sync_markdown_residency(
        &mut self,
        one_shot_row_target: Option<usize>,
        cx: &mut Context<Self>,
    ) {
        let running = self.workspace_store.read(cx).composer_state().turn_running;
        let row_count = self.rows.len();
        let resident_ids = self.md_states.keys().cloned().collect();
        let (texts, decisions) = self
            .workspace_store
            .read(cx)
            .with_active_timeline(|timeline| {
                let scope = ResidencyScope::new(
                    row_count,
                    self.markdown_visible_rows.clone(),
                    one_shot_row_target,
                    running,
                );
                let entries = markdown_entries_for_residency(timeline, &self.rows, &scope);
                let decisions = decide(ResidencyInput {
                    row_count,
                    visible_rows: self.markdown_visible_rows.clone(),
                    one_shot_row_target,
                    entries: &entries,
                    stream_running: running,
                    resident_ids: &resident_ids,
                });
                let mut texts = Vec::new();
                for (index, entry) in timeline.entries.iter().enumerate() {
                    if !decisions.build.contains(&entry.id) {
                        continue;
                    }
                    let row = row_of_entry(&self.rows, index, entry.turn);
                    if let Some(text) = entry_markdown(entry) {
                        texts.push((row, entry.id.clone(), text));
                    }
                }
                (texts, decisions)
            })
            .unwrap_or_default();
        self.md_states.retain(|id, _| !decisions.evict.contains(id));
        self.pending_md_builds
            .retain(|id, _| decisions.build.contains(id) && !decisions.evict.contains(id));

        let mut rebuilt_rows = HashSet::new();
        for (row, id, text) in texts {
            match self.md_states.get_mut(&id) {
                Some(md) => md.sync(text, cx),
                None if self.pending_md_builds.contains_key(&id) => {
                    let pending = self
                        .pending_md_builds
                        .get_mut(&id)
                        .expect("pending Markdown build disappeared");
                    pending.desired_text = text;
                }
                None if text.len() > ASYNC_MARKDOWN_THRESHOLD_BYTES => {
                    self.spawn_markdown_build(id, text, cx);
                }
                None => {
                    self.md_states.insert(id, MdState::new(&text, cx));
                    rebuilt_rows.extend(row);
                }
            }
        }
        self.place_messages_in_selection(cx);
        let mut rebuilt_rows = rebuilt_rows
            .into_iter()
            .filter(|row| *row < self.rows.len())
            .collect::<Vec<_>>();
        rebuilt_rows.sort_unstable();
        let Some((&first, rest)) = rebuilt_rows.split_first() else {
            return;
        };
        let mut range = first..first + 1;
        for &row in rest {
            if row == range.end {
                range.end += 1;
            } else {
                // Eviction leaves this cache untouched. A lazy rebuild only
                // invalidates rebuilt rows; ListState preserves the absolute
                // scroll-top offset while it measures the parsed Markdown.
                self.remeasure_markdown_rows(range);
                range = row..row + 1;
            }
        }
        self.remeasure_markdown_rows(range);
    }

    /// Make each resident message the selection entry at its index in the
    /// timeline.
    fn place_messages_in_selection(&self, cx: &mut Context<Self>) {
        let places = self
            .workspace_store
            .read(cx)
            .with_active_timeline(|timeline| {
                timeline
                    .entries
                    .iter()
                    .enumerate()
                    .filter_map(|(index, entry)| {
                        let md = self.md_states.get(&entry.id)?;
                        Some((md.state.clone(), index as u64))
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        for (state, entry) in places {
            state.update(cx, |state, cx| {
                state.set_selection_entry(&self.selection, entry, cx)
            });
        }
    }

    fn markdown_row(&self, id: &str, cx: &App) -> Option<usize> {
        self.workspace_store
            .read(cx)
            .with_active_timeline(|timeline| {
                timeline
                    .entries
                    .iter()
                    .position(|entry| entry.id == id)
                    .and_then(|index| row_of_entry(&self.rows, index, timeline.entries[index].turn))
            })
            .flatten()
    }

    fn spawn_markdown_build(&mut self, id: String, text: String, cx: &mut Context<Self>) {
        self.next_md_build_generation = self.next_md_build_generation.wrapping_add(1);
        let generation = self.next_md_build_generation;
        self.pending_md_builds.insert(
            id.clone(),
            PendingMarkdownBuild {
                generation,
                session_key: self.session_key.clone(),
                desired_text: text.clone(),
            },
        );
        let parse_text = text;
        cx.spawn(async move |this, cx| {
            let parsed_text = parse_text.clone();
            let parsed = cx
                .background_executor()
                .spawn(async move { crate::markdown::parse::parse_document(&parse_text) })
                .await;
            let _ = this.update(cx, |chat, cx| {
                chat.finish_markdown_build(id, generation, parsed_text, parsed, cx);
            });
        })
        .detach();
    }

    fn finish_markdown_build(
        &mut self,
        id: String,
        generation: u64,
        parsed_text: String,
        parsed: crate::markdown::parse::ParsedDocument,
        cx: &mut Context<Self>,
    ) {
        let Some(pending) = self.pending_md_builds.get(&id) else {
            return;
        };
        if pending.generation != generation || pending.session_key != self.session_key {
            return;
        }
        let desired_text = pending.desired_text.clone();
        self.pending_md_builds.remove(&id);

        if desired_text != parsed_text && !desired_text.starts_with(&parsed_text) {
            // An edit invalidated the parse. Re-evaluate residency and kick a
            // replacement job for the latest text if it is still wanted.
            self.sync_markdown_residency(None, cx);
            return;
        }

        let mut state = MdState::from_parsed(&parsed_text, parsed, cx);
        if desired_text != parsed_text {
            state.sync(desired_text, cx);
        }
        let row = self.markdown_row(&id, cx);
        self.md_states.insert(id, state);
        self.place_messages_in_selection(cx);
        if let Some(row) = row.filter(|row| *row < self.rows.len()) {
            self.remeasure_markdown_rows(row..row + 1);
        }
        cx.notify();
    }

    fn remeasure_markdown_rows(&mut self, range: Range<usize>) {
        #[cfg(test)]
        self.markdown_remeasured_rows.extend(range.clone());
        self.list_state.remeasure_items(range);
    }

    /// Invalidate the measured heights of every row of `turn`.
    fn remeasure_turn(&self, turn: usize) {
        let rows = rows_of_turn(&self.rows, turn);
        if !rows.is_empty() {
            self.list_state.remeasure_items(rows);
        }
    }

    /// Invalidate the measured height of the row that renders the activity
    /// `entry_id`; an expanded Work Log gives each activity a row of its own.
    fn remeasure_activity(&self, turn: usize, entry_id: &str, cx: &App) {
        let row = self
            .workspace_store
            .read(cx)
            .with_active_timeline(|timeline| {
                let entries = &timeline.entries;
                let start = entries.partition_point(|entry| entry.turn < turn);
                let index = start
                    + entries[start..]
                        .iter()
                        .take_while(|entry| entry.turn == turn)
                        .position(|entry| entry.id == entry_id)?;
                row_of_entry(&self.rows, index, turn)
            })
            .flatten();
        match row {
            Some(row) => self.list_state.remeasure_items(row..row + 1),
            None => self.remeasure_turn(turn),
        }
    }

    fn set_markdown_visible_rows(&mut self, visible_rows: Range<usize>, cx: &mut Context<Self>) {
        self.painted_tail_start = None;
        let row_count = self.rows.len();
        let visible_rows = visible_rows.start.min(row_count)..visible_rows.end.min(row_count);
        self.markdown_scroll_top = Some(visible_rows.start);
        if visible_rows == self.markdown_visible_rows {
            return;
        }
        self.markdown_visible_rows = visible_rows;
        self.sync_markdown_residency(None, cx);
        cx.notify();
    }

    fn sync_markdown_scroll_position(&mut self, cx: &mut Context<Self>) {
        let row_count = self.rows.len();
        let scroll_top = self.list_state.logical_scroll_top().item_ix.min(row_count);
        if self.markdown_scroll_top == Some(scroll_top) {
            return;
        }
        self.markdown_scroll_top = Some(scroll_top);
        self.markdown_visible_rows = if scroll_top == row_count {
            self.markdown_tail_window()
        } else {
            self.painted_tail_start = None;
            viewport_row_window(scroll_top, row_count)
        };
        self.sync_markdown_residency(None, cx);
    }

    /// The tail's rows: the row hint, widened to the rows last painted there.
    /// Rows only join the tail while it is followed, so those stay in place.
    fn markdown_tail_window(&self) -> Range<usize> {
        let tail = tail_row_window(self.rows.len());
        let start = self
            .painted_tail_start
            .map_or(tail.start, |start| start.min(tail.start));
        start..tail.end
    }

    /// Row hints only approximate the rows on screen, which can be far
    /// shorter than a hint assumes (an expanded Work Log's activities). Take
    /// the rows the list painted when the hint missed some of them. `None`
    /// when they change nothing.
    fn painted_rows_change(&self, painted: Range<usize>) -> Option<PaintedRowsChange> {
        let row_count = self.rows.len();
        let painted = painted.start.min(row_count)..painted.end.min(row_count);
        let tail_start = (self.list_state.is_following_tail() && painted.end == row_count)
            .then_some(painted.start);
        let window = &self.markdown_visible_rows;
        let missed = painted.start < window.start || window.end < painted.end;
        (missed || tail_start != self.painted_tail_start).then(|| PaintedRowsChange {
            tail_start,
            visible: missed.then_some(painted),
        })
    }

    fn adopt_painted_rows(&mut self, change: PaintedRowsChange, cx: &mut Context<Self>) {
        self.painted_tail_start = change.tail_start;
        if let Some(visible) = change.visible {
            self.markdown_visible_rows = visible;
            self.sync_markdown_residency(None, cx);
            cx.notify();
        }
    }

    #[cfg(test)]
    fn resident_markdown_state_count(&self) -> usize {
        self.md_states.len()
    }

    #[cfg(test)]
    fn has_resident_markdown_state(&self, id: &str) -> bool {
        self.md_states.contains_key(id)
    }

    #[cfg(test)]
    fn resident_markdown_source(&self, id: &str) -> Option<&str> {
        self.md_states.get(id).map(|md| md.synced.as_ref())
    }

    fn toggle_expanded(&mut self, turn: usize, key: &str, cx: &mut Context<Self>) {
        if !self.expanded.remove(key) {
            self.expanded.insert(key.to_string());
        }
        self.remeasure_expanded(turn, cx);
    }

    fn toggle_activity_expanded(
        &mut self,
        (turn, entry_id): (usize, &str),
        key: &str,
        expanded: bool,
        cx: &mut Context<Self>,
    ) {
        if let Some(session_key) = self.session_key.as_deref() {
            self.auto_activity_expansions.insert(
                session_key,
                key,
                AutoActivityExpansion::Manual {
                    expanded: !expanded,
                },
            );
        }
        self.remeasure_activity(turn, entry_id, cx);
        cx.notify();
    }

    fn remeasure_expanded(&mut self, turn: usize, cx: &mut Context<Self>) {
        // The next frame refreshes the cached turn fingerprint; the direct
        // remeasure covers collapsibles whose state is intentionally not
        // fingerprinted.
        self.timeline_stale = true;
        self.remeasure_turn(turn);
        cx.notify();
    }

    fn auto_activity_expanded(
        &mut self,
        (turn, entry_id): (usize, &str),
        key: &str,
        enabled: bool,
        recency: AutoActivityRecency,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(session_key) = self.session_key.clone() else {
            return false;
        };
        let auto = self.auto_activity_expansions.observe(
            &session_key,
            key,
            enabled,
            recency,
            Instant::now(),
        );
        if let Some((generation, delay)) = auto.collapse {
            let collapse_key = key.to_string();
            let collapse_entry_id = entry_id.to_string();
            let collapse_session_key = session_key.clone();
            let timer = cx.background_executor().timer(delay);
            cx.spawn(async move |this, cx| {
                timer.await;
                let _ = this.update(cx, |this, cx| {
                    if this.auto_activity_expansions.finish_collapse(
                        &collapse_session_key,
                        &collapse_key,
                        generation,
                    ) && this.session_key.as_deref() == Some(collapse_session_key.as_str())
                    {
                        this.remeasure_activity(turn, &collapse_entry_id, cx);
                        cx.notify();
                    }
                });
            })
            .detach();
        }
        auto.expanded
    }

    /// Render one timeline row: the segment it spans (a message, an error, a
    /// Work Log run or a part of an expanded one) and, on the turn's last
    /// row, the turn's trailer.
    ///
    /// The trailer reads the whole turn (pending steers, the last
    /// timestamp). `pinned` carries the ids of the last user / last assistant
    /// message in the whole timeline: their action rows stay visible instead
    /// of waiting for a hover, so Copy is never invisible-and-hover-only.
    fn render_row(
        &mut self,
        args: RowRenderArgs<'_>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let (row, turn, cwd, own, entries, pinned) = args;
        let index = row.turn;
        let mut column = v_flex().w_full().gap(px(SEGMENT_GAP));

        // A row's entries re-segment to the one segment it was indexed from.
        let segmented = segment_entries(own, turn.running);
        let segments = &segmented.flow;

        for segment in segments.iter() {
            match segment {
                Segment::Relay(entry) => {
                    let EntryContent::ProviderRelay {
                        from_provider,
                        to_provider,
                        prompt,
                        moved_from,
                        ..
                    } = &entry.content
                    else {
                        unreachable!();
                    };
                    column = column.child(components::dividers::relay_divider(
                        &entry.id,
                        *from_provider,
                        *to_provider,
                        moved_from.as_deref(),
                        cx,
                    ));
                    if let Some(prompt) = prompt {
                        column = column.child(
                            self.compose_disclosure(
                                index,
                                format!("relay-handoff-{}", entry.id),
                                crate::tr!("chat.relay_handoff", to = to_provider.display_name())
                                    .into_owned()
                                    .into(),
                                prompt,
                                cx,
                            ),
                        );
                    }
                }
                Segment::ModelChange(entry) => {
                    let EntryContent::ModelChanged { from, to, reason } = &entry.content else {
                        unreachable!();
                    };
                    column = column.child(components::dividers::model_change_divider(
                        &entry.id,
                        from.as_deref(),
                        to,
                        reason.as_deref(),
                        cx,
                    ));
                }
                Segment::ContextCompacted(entry) => {
                    column = column.child(components::dividers::context_compacted_divider(
                        &entry.id,
                        match &entry.content {
                            EntryContent::ContextCompacted(c) => Some(c),
                            _ => None,
                        },
                        cx,
                    ));
                }
                Segment::ContextWindowChanged(entry) => {
                    let EntryContent::ContextWindowChanged { window } = entry.content else {
                        unreachable!();
                    };
                    column = column.child(components::dividers::context_window_changed_divider(
                        &entry.id, window, cx,
                    ));
                }
                Segment::ControlAction(entry) => {
                    let EntryContent::ControlAction(action) = &entry.content else {
                        unreachable!();
                    };
                    column = column.child(components::dividers::control_action_divider(
                        &entry.id, action, cx,
                    ));
                }
                Segment::ActivityRun(activities) => {
                    column = column.child(self.compose_work_log(
                        (index, &row.part, turn, cwd, activities, row.live_activity),
                        cx,
                    ));
                }
                Segment::Images(images) => {
                    column = column.child(components::images::image_tiles(
                        images,
                        cwd,
                        self.session_key.as_deref().unwrap_or_default(),
                        cx,
                    ));
                    for entry in images {
                        if matches!(
                            &entry.content,
                            EntryContent::Item(ItemContent::ToolCall { .. })
                        ) {
                            column = column.child(self.compose_activity_row(
                                entry,
                                true,
                                false,
                                false,
                                AutoActivityRecency::Older,
                                cx,
                            ));
                        }
                    }
                }
                Segment::User(entry) => {
                    column = column.child(self.compose_user_entry(
                        (index, entry, cwd, pinned.0 == Some(entry.id.as_str())),
                        window,
                        cx,
                    ));
                }
                Segment::Assistant(entry) => {
                    let EntryContent::Item(ItemContent::AssistantMessage { text }) = &entry.content
                    else {
                        unreachable!();
                    };
                    let (markdown, copy_text) = self.md_states.get(&entry.id).map_or_else(
                        || (None, Arc::from(text.as_str())),
                        |md| (Some(md.state.clone()), md.synced.clone()),
                    );
                    let copy_key = format!("assistant:{}", entry.id);
                    let copied = self.copied.is(&copy_key);
                    let mark = copy_key;
                    column = column.child(components::assistant::assistant(
                        components::assistant::AssistantData {
                            id: &entry.id,
                            text,
                            cwd,
                            markdown,
                            compact: self.window_state.read(cx).compact,
                            pinned: pinned.1 == Some(entry.id.as_str()),
                            show_actions: !turn.running && row.last_assistant,
                            copied,
                        },
                        cx.listener(move |this, _, _, cx| {
                            cx.write_to_clipboard(ClipboardItem::new_string(copy_text.to_string()));
                            this.mark_copied(mark.clone(), cx);
                        }),
                        cx,
                    ));
                }
                Segment::Error(entry) => {
                    let message = displayed_error_text(&entry.content);
                    let resume = match &entry.content {
                        EntryContent::Error {
                            limit_resets_at: Some(resets_at),
                            ..
                        } => {
                            let resets_at = *resets_at;
                            let store = self.workspace_store.read(cx);
                            let queued: Vec<_> = store
                                .composer_state()
                                .queue
                                .into_iter()
                                .flat_map(|queue| queue.messages)
                                .collect();
                            let queued_id = queued
                                .iter()
                                .find(|message| message.fire_at_unix_secs == Some(resets_at))
                                .map(|message| message.id);
                            let queue_empty = queued.is_empty();
                            let ends_thread = store
                                .with_active_timeline(|timeline| {
                                    !timeline.turn_running && index + 1 == timeline.turns.len()
                                })
                                .unwrap_or(false);
                            match limit_resume_action(
                                resets_at,
                                queued_id,
                                now_secs(),
                                ends_thread,
                                queue_empty,
                            ) {
                                LimitResumeAction::Scheduled {
                                    queued_id: id,
                                    remaining_secs,
                                } => Some(components::error_card::LimitResume::Scheduled {
                                    remaining_secs,
                                    on_cancel: Box::new(cx.listener(move |this, _, _, cx| {
                                        this.workspace_store
                                            .update(cx, |store, _| store.drop_queued(id));
                                    })),
                                }),
                                LimitResumeAction::Schedule => {
                                    Some(components::error_card::LimitResume::Offer {
                                        on_schedule: Box::new(cx.listener(
                                            move |this, _, _, cx| {
                                                this.workspace_store.update(cx, |store, _| {
                                                    store.schedule_turn(
                                                        tcode_core::session::RESUME_PROMPT
                                                            .to_string(),
                                                        vec![],
                                                        resets_at,
                                                    )
                                                });
                                            },
                                        )),
                                    })
                                }
                                LimitResumeAction::ContinueNow => {
                                    Some(components::error_card::LimitResume::Continue {
                                        on_continue: Box::new(cx.listener(
                                            move |this, _, _, cx| {
                                                this.workspace_store.update(cx, |store, _| {
                                                    store.send_turn(
                                                        tcode_core::session::RESUME_PROMPT
                                                            .to_string(),
                                                        vec![],
                                                    )
                                                });
                                            },
                                        )),
                                    })
                                }
                                LimitResumeAction::None => None,
                            }
                        }
                        _ => None,
                    };
                    let retry = self
                        .workspace_store
                        .read(cx)
                        .with_active_timeline(|timeline| {
                            timeline
                                .turn_retry
                                .as_ref()
                                .filter(|retry| {
                                    !timeline.turn_running
                                        && turn.provider_turn_id.as_deref()
                                            == Some(retry.turn_id.as_str())
                                        && timeline
                                            .entries
                                            .iter()
                                            .rev()
                                            .find(|entry| {
                                                matches!(entry.content, EntryContent::Error { .. })
                                            })
                                            .is_some_and(|last| last.id == entry.id)
                                })
                                .cloned()
                        })
                        .flatten();
                    let resume = retry
                        .map(|retry| {
                            let turn_id = retry.turn_id;
                            components::error_card::LimitResume::Retry {
                                remaining_secs: retry
                                    .retry_at
                                    .map(|at| at.saturating_sub(now_secs())),
                                on_retry: Box::new(cx.listener(move |this, _, _, cx| {
                                    this.workspace_store
                                        .update(cx, |store, _| store.retry_turn(turn_id.clone()));
                                })),
                                on_cancel: Box::new(cx.listener(move |this, _, _, cx| {
                                    this.workspace_store
                                        .update(cx, |store, _| store.interrupt());
                                })),
                            }
                        })
                        .or(resume);
                    let copy_key = format!("error:{}", entry.id);
                    let copied = self.copied.is(&copy_key);
                    let mark = copy_key;
                    let copy_text = message.to_string();
                    column = column.child(components::error_card::error_card(
                        &entry.id,
                        &message,
                        copied,
                        resume,
                        cx.listener(move |this, _, _, cx| {
                            cx.write_to_clipboard(ClipboardItem::new_string(copy_text.clone()));
                            this.mark_copied(mark.clone(), cx);
                        }),
                        cx,
                    ));
                }
            }
        }

        if !row.last_in_turn {
            return column.into_any_element();
        }

        // Changed-file evidence is the turn's settled summary: while the turn
        // still runs, the live per-file rows inside the work log carry this
        // information, so the section appears only once the turn finishes.
        let (changes, completeness) = self.workspace_store.read(cx).chat_turn_changes(index);
        if !turn.running && !changes.is_empty() {
            let more_key = format!("changed-files-more-{index}");
            let show_all = self.expanded.contains(&more_key);
            let toggle_more_key = more_key;
            let handlers = components::changed_files::ChangedFilesHandlers {
                view_diff: Box::new(cx.listener(move |this, _, _, cx| {
                    this.open_diff(index, None, cx);
                })),
                toggle_more: Box::new(cx.listener(move |this, _, _, cx| {
                    this.toggle_expanded(index, &toggle_more_key, cx);
                })),
                open_files: changes
                    .iter()
                    .map(|change| {
                        let path = change.path.clone();
                        Box::new(cx.listener(move |this, _, _, cx| {
                            this.open_diff(index, Some(path.clone()), cx);
                        })) as components::changed_files::ClickHandler
                    })
                    .collect(),
            };
            column = column.child(components::changed_files::changed_files(
                index,
                cwd,
                &changes,
                completeness,
                show_all,
                handlers,
                cx,
            ));
        }

        // The turn's liveness, bare after everything it produced: no fold can
        // reach it, so collapsing the last disclosure never hides that we work.
        if turn.running {
            let requested_model = self.workspace_store.read(cx).chat_requested_model();
            let served_model =
                divergent_served_model(turn.served_model.as_deref(), requested_model.as_deref())
                    .map(str::to_owned);
            column = column.child(
                div()
                    .id(("working-status", index))
                    .debug_selector(move || format!("working-status-{index}"))
                    .child(components::indicator::turn_working_indicator(
                        index,
                        turn.start_ts,
                        served_model,
                        cx,
                    )),
            );
        } else if let Some(ts) = turn.end_ts.or(entries.last().and_then(|e| e.ts)) {
            let requested_model = self.workspace_store.read(cx).chat_requested_model();
            column = column.child(components::indicator::finished_turn_time(
                ts,
                turn,
                requested_model.as_deref(),
                cx,
            ));
        }

        // Pending steers float below every live transcript/work-log element.
        // They are read from the whole turn, not this row's segment, so FIFO
        // order holds without making their request-time position look
        // model-visible.
        for entry in segment_entries(entries, turn.running).pending_steers {
            column = column.child(self.compose_user_entry(
                (index, entry, cwd, pinned.0 == Some(entry.id.as_str())),
                window,
                cx,
            ));
        }

        column.into_any_element()
    }

    fn on_rewind_turn(&mut self, action: &RewindTurn, _: &mut Window, cx: &mut Context<Self>) {
        let RewindTurn { turn, mode } = *action;
        self.workspace_store
            .update(cx, |store, _cx| store.rewind_turn(turn, mode));
    }

    fn open_diff(&mut self, turn: usize, path: Option<String>, cx: &mut Context<Self>) {
        self.workspace_store.update(cx, |store, cx| match path {
            Some(path) => store.open_diff_for_file(turn, path, cx),
            None => store.open_diff_for_turn(turn, cx),
        });
        if self.window_state.read(cx).compact {
            self.window_state
                .update(cx, |_, cx| cx.emit(OpenPanel(NavigationPanel::Diff)));
        }
    }

    fn on_open_file_diff(&mut self, action: &OpenFileDiff, _: &mut Window, cx: &mut Context<Self>) {
        let OpenFileDiff { turn, path } = action.clone();
        self.open_diff(turn, Some(path), cx);
    }

    fn compose_user(
        &self,
        args: components::bubble::UserMessageArgs<'_>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let (turn, entry_id, text, cwd, context_len, attachments, steering, pinned) = args;
        if self
            .workspace_store
            .read(cx)
            .delivery_messages()
            .iter()
            .any(|(key, _, _, _)| {
                entry_id == format!("local-user-{key}") || entry_id == format!("local-steer-{key}")
            })
        {
            return div().into_any_element();
        }

        let context = context_len
            .filter(|len| *len <= text.len() && text.is_char_boundary(*len))
            .map(|len| &text[..len]);
        let visible = user_visible_text(text, context_len);
        let rewind_menu = steering
            .is_none()
            .then(|| self.workspace_store.read(cx).chat_native_rewind_state(turn))
            .flatten()
            .filter(|(available, _)| *available)
            .map(|(_, disabled)| components::bubble::RewindMenu {
                turn,
                disabled,
                conversation_available: self.workspace_store.read(cx).absolute_turn(turn) > 0,
            });
        let rewind = steering
            .is_none()
            .then(|| {
                let rewind_handler = |mode| {
                    let workspace_store = self.workspace_store.clone();
                    Arc::new(cx.listener(move |_, _, _, cx| {
                        workspace_store.update(cx, |store, _cx| store.rewind_turn(turn, mode));
                    })) as components::bubble::SharedClickHandler
                };
                components::bubble::native_rewind_button(
                    turn,
                    self.workspace_store.read(cx).absolute_turn(turn) > 0,
                    (
                        self.workspace_store.read(cx).chat_native_rewind_state(turn),
                        self.window_state.read(cx).compact,
                    ),
                    components::bubble::RewindHandlers {
                        files_and_conversation: rewind_handler(RewindMode::FilesAndConversation),
                        conversation: rewind_handler(RewindMode::Conversation),
                        files: rewind_handler(RewindMode::Files),
                    },
                    cx,
                )
            })
            .flatten();
        let markdown = self.md_states.get(entry_id).map(|md| md.state.clone());
        let copy_key = format!("user:{entry_id}");
        let copied = self.copied.is(&copy_key);
        let mark = copy_key;
        let copy_text: Arc<str> = Arc::from(visible);
        let images = attachments
            .iter()
            .map(|path| {
                let path = PathBuf::from(path);
                let title = path
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "image".to_string());
                Box::new(cx.listener(move |_, _, window, cx| {
                    crate::image_viewer::open(
                        crate::image_viewer::ViewerSource::original(
                            tcode_protocol::OriginalTarget::File { path: path.clone() },
                            crate::store::image_preview(path.clone(), 120., true),
                        ),
                        title.clone(),
                        window,
                        cx,
                    );
                })) as components::bubble::ClickHandler
            })
            .collect();
        let bubble = components::bubble::user_bubble(
            components::bubble::BubbleData {
                entry_id,
                visible,
                cwd,
                attachments,
                steering,
                compact: self.window_state.read(cx).compact,
                pinned,
                copied,
                markdown,
                rewind,
                rewind_menu,
            },
            components::bubble::BubbleHandlers {
                copy: Box::new(cx.listener(move |this, _, _, cx| {
                    cx.write_to_clipboard(ClipboardItem::new_string(copy_text.to_string()));
                    this.mark_copied(mark.clone(), cx);
                })),
                images,
            },
            window,
            cx,
        );

        let bubble = if let Some(author) = self.workspace_store.read(cx).message_byline(entry_id) {
            // Inset by the bubble's own padding, so the name lines up with the
            // message text rather than the bubble's rounded corner.
            v_flex()
                .w_full()
                .items_end()
                .gap(px(2.))
                .child(
                    div()
                        .max_w_3_4()
                        .px(cx.theme().tokens.spacing.md)
                        .text_secondary(false, cx)
                        .overflow_hidden()
                        .child(crate::widgets::fade_text(author)),
                )
                .child(bubble)
                .into_any_element()
        } else {
            bubble
        };

        let Some(context) = context else {
            return bubble;
        };
        v_flex()
            .w_full()
            .gap_2()
            .child(self.compose_disclosure(
                turn,
                format!("orchestrate-context-{entry_id}"),
                injected_context_label(context).into(),
                context,
                cx,
            ))
            .child(bubble)
            .into_any_element()
    }

    /// A user-role entry, sent or still steering: the notice the host wrote,
    /// or else the user's bubble.
    fn compose_user_entry(
        &self,
        (turn, entry, cwd, pinned): (usize, &TimelineEntry, &Path, bool),
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let (text, steering, context_len, attachments) =
            user_content(&entry.content).expect("a user entry");
        let Some(notice) = user_notice(&entry.content) else {
            return self.compose_user(
                (
                    turn,
                    &entry.id,
                    text,
                    cwd,
                    context_len,
                    attachments,
                    steering,
                    pinned,
                ),
                window,
                cx,
            );
        };
        // A notice is the host's, never the user's words: centered disclosure
        // rows without an action row, below any context the turn injected.
        let context = context_len
            .filter(|len| *len <= text.len() && text.is_char_boundary(*len))
            .map(|len| &text[..len]);
        let pending = steering == Some(SteeringStatus::Pending);
        let label = |label: String| -> SharedString {
            if pending {
                format!("{label} · {}", crate::tr!("chat.steering")).into()
            } else {
                label.into()
            }
        };
        let mut column = v_flex()
            .w_full()
            .gap_2()
            .when_some(context, |column, context| {
                column.child(self.compose_disclosure(
                    turn,
                    format!("orchestrate-context-{}", entry.id),
                    injected_context_label(context).into(),
                    context,
                    cx,
                ))
            });
        let mut keys = notice_keys(&notice, &entry.id).into_iter();
        let mut key = || keys.next().expect("a key per notice row");
        let visible = user_visible_text(text, context_len);
        match &notice {
            HostNotice::Orchestrate(callbacks) => {
                for callback in callbacks {
                    let event =
                        label(components::disclosure::callback_event_label(callback).into_owned());
                    column =
                        column.child(self.compose_callback_row(turn, key(), event, callback, cx));
                }
            }
            HostNotice::PullRequest { .. }
            | HostNotice::ReportReminder
            | HostNotice::Control(_) => {
                column = column.child(self.compose_disclosure(
                    turn,
                    key(),
                    label(components::disclosure::notice_label(&notice)),
                    visible,
                    cx,
                ));
            }
        }
        column.into_any_element()
    }

    fn compose_callback_row(
        &self,
        turn: usize,
        key: String,
        event: SharedString,
        callback: &OrchestrateCallback,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let expanded = self.expanded.contains(&key);
        let child_id = callback.child_id.clone();
        let exists = {
            let store = self.workspace_store.read(cx);
            store
                .sidebar_sessions()
                .iter()
                .chain(store.archived_sessions())
                .any(|meta| meta.id == child_id)
        };
        let store = self.workspace_store.clone();
        let on_open = exists.then(|| {
            Box::new(move |_: &ClickEvent, _: &mut Window, cx: &mut App| {
                crate::agents_panel::open_agent(&store, child_id.clone(), cx);
            }) as components::subagent::ClickHandler
        });
        let toggle_key = key.clone();
        components::disclosure::callback_row(
            &key,
            event,
            callback,
            expanded,
            cx.listener(move |this, _, _, cx| {
                this.toggle_expanded(turn, &toggle_key, cx);
            }),
            on_open,
            cx,
        )
    }

    fn compose_disclosure(
        &self,
        turn: usize,
        key: String,
        label: SharedString,
        full_text: &str,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let expanded = self.expanded.contains(&key);
        let toggle_key = key.clone();
        components::disclosure::disclosure(
            &key,
            label,
            full_text,
            expanded,
            cx.listener(move |this, _, _, cx| {
                this.toggle_expanded(turn, &toggle_key, cx);
            }),
            cx,
        )
    }

    /// Prepare one stateless Work Log capsule, or the part of an expanded one
    /// that `part` names.
    fn compose_work_log(
        &mut self,
        args: components::work_log::WorkLogArgs<'_>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let (index, part, turn, cwd, activities, is_last) = args;
        let segment_id = activities[0].id.as_str();
        let section_key = work_log_key(index, segment_id);
        let running = is_last && turn.running;
        let live_reasoning_id = running
            .then(|| activities.last().copied())
            .flatten()
            .filter(|entry| {
                matches!(
                    entry.content,
                    EntryContent::Item(ItemContent::Reasoning { .. })
                )
            })
            .map(|entry| entry.id.as_str());
        match part {
            RowPart::WorkLogActivity => {
                let rows = self.compose_work_log_rows(activities, cwd, None, false, cx);
                return components::work_log::work_log_body(rows).into_any_element();
            }
            RowPart::WorkLogLive => {
                return v_flex()
                    .w_full()
                    .gap_1()
                    .children(self.compose_work_log_rows(
                        activities,
                        cwd,
                        live_reasoning_id,
                        running,
                        cx,
                    ))
                    .into_any_element();
            }
            RowPart::Segment | RowPart::WorkLogHeader { .. } => {}
        }
        let header_only = matches!(part, RowPart::WorkLogHeader { .. });
        let (folded, visible) = partition_activity_run(activities, running);
        let expanded = self.expanded.contains(&section_key);

        let mut flow = v_flex().w_full().gap_1();
        if !folded.is_empty() {
            let segment_counts = work_log_counts(folded);
            let mut capsule_label = work_log_capsule_label(&segment_counts, folded.len());
            if capsule_label.is_empty() {
                capsule_label = crate::tr!("chat.work_log").into_owned();
            }
            let duration =
                format_elapsed_deciseconds(activity_run_duration_ms(folded, turn, is_last));
            let outcome = work_log_outcome(turn, folded, is_last);
            let rows = if expanded && !header_only {
                self.compose_work_log_rows(folded, cwd, live_reasoning_id, false, cx)
            } else {
                Vec::new()
            };

            let toggle_section_key = section_key;
            flow = flow.child(components::work_log::work_log(
                components::work_log::WorkLogData {
                    compact: self.window_state.read(cx).compact,
                    index,
                    segment_id: segment_id.to_string(),
                    capsule_label,
                    duration,
                    outcome,
                    expanded,
                    running,
                    rows,
                },
                cx.listener(move |this, _, _, cx| {
                    this.toggle_expanded(index, &toggle_section_key, cx);
                }),
                cx,
            ));
        }

        if !visible.is_empty() && !header_only {
            flow = flow.child(
                v_flex()
                    .w_full()
                    .gap_1()
                    .children(self.compose_work_log_rows(
                        visible,
                        cwd,
                        live_reasoning_id,
                        running,
                        cx,
                    )),
            );
        }

        flow.into_any_element()
    }

    fn compose_work_log_rows(
        &mut self,
        activities: &[&TimelineEntry],
        cwd: &Path,
        live_reasoning_id: Option<&str>,
        auto_expand: bool,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let mut rows = Vec::new();
        for (activity_index, entry) in activities.iter().enumerate() {
            let recency = AutoActivityRecency::from_newer_activity_count(
                activities.len() - activity_index - 1,
            );
            if let EntryContent::Item(ItemContent::FileChange { changes, .. }) = &entry.content {
                let turn = entry.turn;
                for (file_index, row) in live_edit_rows(changes, cwd).iter().enumerate() {
                    let key = format!("activity-{}-file-{file_index}", entry.id);
                    let enabled = auto_expand && row.counts.is_some();
                    let expanded =
                        self.auto_activity_expanded((turn, &entry.id), &key, enabled, recency, cx);
                    let inline_diff = (expanded && row.counts.is_some())
                        .then(|| self.inline_diffs.render(&key, row, cx));
                    let toggle_key = key.clone();
                    let entry_id = entry.id.clone();
                    rows.push(components::changed_files::file_edit_row(
                        &key,
                        row,
                        cwd,
                        expanded,
                        inline_diff,
                        cx.listener(move |this, _, _, cx| {
                            this.toggle_activity_expanded(
                                (turn, &entry_id),
                                &toggle_key,
                                expanded,
                                cx,
                            );
                        }),
                        cx,
                    ));
                }
            } else {
                rows.push(self.compose_activity_row(
                    entry,
                    false,
                    live_reasoning_id == Some(entry.id.as_str()),
                    auto_expand,
                    recency,
                    cx,
                ));
            }
        }
        rows
    }

    /// Prepare one stateless Work Log activity component.
    fn compose_activity_row(
        &mut self,
        entry: &TimelineEntry,
        compact: bool,
        live_reasoning: bool,
        auto_expand: bool,
        recency: AutoActivityRecency,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        if matches!(
            &entry.content,
            EntryContent::Item(ItemContent::Subagent { .. })
        ) {
            return self.compose_subagent_row(entry, cx);
        }
        let key = format!("activity-{}", entry.id);
        let turn = entry.turn;
        let is_command = matches!(
            &entry.content,
            EntryContent::Item(ItemContent::CommandExecution { .. })
        );
        let auto_enabled =
            auto_expand && is_command && self.workspace_store.read(cx).live_command_panel();
        let expanded =
            self.auto_activity_expanded((turn, &entry.id), &key, auto_enabled, recency, cx);
        let command_detail = if expanded {
            match &entry.content {
                EntryContent::Item(ItemContent::CommandExecution {
                    command, output, ..
                }) => {
                    let panel_id = entry.id.clone();
                    let on_cols_change = cx.listener(move |_this, cols: &u16, window, cx| {
                        // Fires from `on_prepaint`, in the middle of laying the
                        // list out. Subscribe once the frame is done.
                        let cols = *cols;
                        let panel_id = panel_id.clone();
                        cx.defer_in(window, move |this, _window, cx| {
                            this.workspace_store.update(cx, |store, _| {
                                store.watch_command_output(&panel_id, cols);
                            });
                        });
                    });
                    let store = self.workspace_store.read(cx);
                    let replica = store.command_output(&entry.id);
                    let screen = components::command_panel::OutputScreen {
                        watched: replica.and_then(|replica| replica.watched()),
                        frame: replica.and_then(|replica| replica.frame()),
                    };
                    Some(self.command_panels.borrow_mut().render(
                        entry,
                        command,
                        output,
                        screen,
                        Some(Box::new(on_cols_change)),
                        cx,
                    ))
                }
                _ => None,
            }
        } else {
            None
        };
        let elided_output = if expanded
            && matches!(
                &entry.content,
                EntryContent::Item(ItemContent::ToolCall { .. })
            ) {
            self.elided_output(&entry.id, turn, cx)
        } else {
            None
        };
        let click_key = key;
        let entry_id = entry.id.clone();
        components::activity::activity_row(
            entry,
            compact,
            live_reasoning,
            expanded,
            command_detail,
            elided_output,
            cx.listener(move |this, _, _, cx| {
                this.toggle_activity_expanded((turn, &entry_id), &click_key, expanded, cx);
            }),
            cx,
        )
    }

    /// How much of a shortened tool output the reader has, or `None` when the
    /// record carried it whole.
    fn elided_output(
        &mut self,
        item_id: &str,
        turn: usize,
        cx: &mut Context<Self>,
    ) -> Option<components::activity::ElidedOutput> {
        use components::activity::ElidedOutput;
        let full_bytes = self
            .workspace_store
            .read(cx)
            .with_active_timeline(|timeline| timeline.elided_outputs.get(item_id).copied())??;
        let load = || -> components::subagent::ClickHandler {
            let item_id = item_id.to_owned();
            Box::new(cx.listener(move |this, _, _, cx| {
                this.load_full_output(item_id.clone(), turn, cx);
            }))
        };
        Some(match self.full_outputs.get(item_id) {
            None => ElidedOutput::Preview {
                full_bytes,
                on_load: load(),
            },
            Some(FullOutput::Loading { .. }) => ElidedOutput::Loading,
            Some(FullOutput::Loaded(output)) => ElidedOutput::Loaded(output.clone()),
            Some(FullOutput::Failed(error)) => ElidedOutput::Failed {
                error: error.clone(),
                on_load: load(),
            },
        })
    }

    fn load_full_output(&mut self, item_id: String, turn: usize, cx: &mut Context<Self>) {
        let Some(session_id) = self.session_key.clone() else {
            return;
        };
        let request = self.workspace_store.update(cx, |store, cx| {
            store.read_item_output(session_id.clone(), item_id.clone(), cx)
        });
        let id = item_id.clone();
        let task = cx.spawn(async move |this, cx| {
            let result = request.await;
            let _ = this.update(cx, |this, cx| {
                if this.session_key.as_deref() != Some(session_id.as_str()) {
                    return;
                }
                let output = match result {
                    Ok(output) => FullOutput::Loaded(output.into()),
                    Err(error) => FullOutput::Failed(error),
                };
                this.remeasure_activity(turn, &id, cx);
                this.full_outputs.insert(id, output);
                cx.notify();
            });
        });
        self.remeasure_activity(turn, &item_id, cx);
        self.full_outputs
            .insert(item_id, FullOutput::Loading { _request: task });
        cx.notify();
    }

    /// Release the screens of panels no longer laid out and remeasure the
    /// rows whose panel changed height with a new screen.
    fn apply_command_panel_changes(
        &mut self,
        changes: components::command_panel::PanelChanges,
        cx: &mut Context<Self>,
    ) {
        if !changes.unmounted.is_empty() {
            self.workspace_store.update(cx, |store, _| {
                for id in &changes.unmounted {
                    store.unwatch_command_output(id);
                }
            });
        }
        if changes.resized.is_empty() {
            return;
        }
        for (turn, id) in &changes.resized {
            self.remeasure_activity(*turn, id, cx);
        }
        cx.notify();
    }

    /// The phone's row over the foot of the timeline: the Agents pill first,
    /// then the linked pull requests, scrolling sideways when they overflow.
    /// It floats over the chat instead of taking height from it, so each chip
    /// carries its own backing; the last row's padding clears it at the tail.
    fn render_phone_strip(
        &self,
        agents: Option<&crate::agents_panel::Agents>,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if !self.window_state.read(cx).compact {
            return None;
        }
        let agents = agents.map(|agents| self.render_agents_pill(agents, cx));
        let pull_requests = self.render_pull_request_pill(cx);
        if agents.is_none() && pull_requests.is_none() {
            return None;
        }
        let radius = crate::material::radius_chip(cx);
        Some(
            crate::material::bar("phone-strip", crate::material::Bar::Toolbar, true, cx)
                .debug_selector(|| "phone-strip".into())
                .absolute()
                .bottom_0()
                .left_0()
                .px_0()
                .child(
                    h_flex()
                        .id("phone-strip-scroll")
                        .size_full()
                        .items_center()
                        .px(px(crate::material::COMPACT_PAGE_INSET))
                        .gap(cx.theme().tokens.spacing.xs)
                        .children(
                            agents
                                .into_iter()
                                .chain(pull_requests)
                                .map(|chip| floating_backing(radius, cx).child(chip)),
                        )
                        .overflow_x_scroll_area(),
                )
                .into_any_element(),
        )
    }

    /// The phone entry to the Agents view: a pill that opens the sheet, shown
    /// while the thread has agents.
    fn render_agents_pill(
        &self,
        agents: &crate::agents_panel::Agents,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let label: SharedString = match agents.count() {
            1 => crate::tr!("agents.pill_one").into_owned(),
            count => crate::tr!("agents.pill", count = count).into_owned(),
        }
        .into();
        let cue = agents.cue(cx);
        let panel = self.agents.clone();
        let shown = self.agents.downgrade();
        let pill = Button::new("agents-pill")
            .debug_selector(|| "agents-pill".into())
            .ghost()
            .outline()
            .chip()
            .text_body(false, cx)
            .icon(IconName::Bot)
            .label(label)
            .when_some(cue, |pill, color| {
                pill.child(div().size(px(6.)).rounded_full().bg(color))
            });
        crate::widgets::Popover::new("agents-sheet")
            .bottom_sheet(crate::tr!("agents.title"))
            .on_open_change(move |open, _, cx| {
                // Opening an agent dismisses the sheet from inside
                // the panel's own update.
                let (shown, open) = (shown.clone(), *open);
                cx.defer(move |cx| {
                    let _ = shown.update(cx, |panel, cx| panel.set_shown(open, cx));
                });
            })
            .trigger_with(move |_, _, _, _| pill.into_any_element())
            .content(move |_, _, cx| {
                let sheet = cx.entity().downgrade();
                panel.update(cx, |panel, _| panel.in_sheet(sheet));
                div().w_full().child(panel.clone()).into_any_element()
            })
            .into_any_element()
    }

    /// The way back from an agent to the thread it was reached from. Unlabelled,
    /// it is its arrow, named by its tooltip and its accessibility label.
    fn parent_link(&self, labelled: bool, cx: &mut Context<Self>) -> Option<AnyElement> {
        let (parent_id, title) = parent_session(self.workspace_store.read(cx))?;
        let name: SharedString = crate::tr!("agents.back_to_parent", title = title)
            .into_owned()
            .into();
        Some(
            Button::new("agent-parent-link")
                .debug_selector(|| "agent-parent-link".into())
                .on_mouse_down(gpui::MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .ghost()
                .xsmall()
                .icon(IconName::ArrowLeft)
                // The parent's title fades rather than pushing this thread's
                // title out of the header.
                .when(labelled, |link| {
                    link.max_w(px(160.)).child(
                        div()
                            .min_w_0()
                            .overflow_hidden()
                            .child(crate::widgets::fade_text(title.clone())),
                    )
                })
                .aria_label(name.clone())
                .tooltip(name)
                .on_click(cx.listener(move |this, _, _, cx| {
                    let parent_id = parent_id.clone();
                    this.workspace_store
                        .update(cx, |store, _| store.select_session(parent_id));
                }))
                .into_any_element(),
        )
    }

    fn compose_subagent_row(&self, entry: &TimelineEntry, cx: &mut Context<Self>) -> AnyElement {
        let active_id = self.workspace_store.read(cx).active_session_id();
        let mirror_id = active_id.as_deref().and_then(|active_id| {
            self.workspace_store
                .read(cx)
                .sidebar_sessions()
                .into_iter()
                .find(|meta| {
                    meta.parent_session_id.as_deref() == Some(active_id)
                        && meta.native_subagent.as_deref() == Some(entry.id.as_str())
                })
                .map(|meta| meta.id)
        });
        let on_open = mirror_id.map(|mirror_id| {
            let store = self.workspace_store.clone();
            Box::new(move |_: &ClickEvent, _: &mut Window, cx: &mut App| {
                store.update(cx, |store, _| store.select_session(mirror_id.clone()));
            }) as components::subagent::ClickHandler
        });
        components::subagent::subagent_row(entry, on_open, cx)
    }

    /// Show the "Copied!" confirmation on `key` for 2s; a second copy re-arms the timer.
    fn mark_copied(&mut self, key: String, cx: &mut Context<Self>) {
        self.copied.mark(key, |this| &mut this.copied, cx);
    }

    fn on_link_url(
        &mut self,
        action: &crate::pull_requests::LinkUrl,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(session_id) = self.workspace_store.read(cx).active_session_id() else {
            return;
        };
        let task = self.workspace_store.update(cx, |store, cx| {
            store.command(
                tcode_protocol::Command::LinkPullRequest {
                    session_id,
                    reference: action.0.clone(),
                },
                cx,
            )
        });
        cx.spawn_in(window, async move |_, cx| {
            let result = task.await;
            _ = cx.update(|window, cx| {
                window.push_notification(
                    match result {
                        Ok(tcode_protocol::CommandResponse::PullRequestLinked {
                            key,
                            already_linked,
                        }) => crate::overlay::Notification::info(
                            crate::tr!(
                                if already_linked {
                                    "pull_requests.already_linked"
                                } else {
                                    "pull_requests.linked_toast"
                                },
                                number = key.number.to_string()
                            )
                            .into_owned(),
                        ),
                        Ok(_) => return,
                        Err(error) => crate::overlay::Notification::error(
                            crate::tr!("pull_requests.link_failed", reason = error.message)
                                .into_owned(),
                        ),
                    },
                    cx,
                )
            });
        })
        .detach();
    }

    fn active_pull_requests(
        &self,
        cx: &App,
    ) -> Option<Vec<tcode_core::pull_request::ThreadPullRequestLink>> {
        let store = self.workspace_store.read(cx);
        let id = store.active_session_id()?;
        Some(store.pull_requests(&id).to_vec())
    }

    /// The pull request page a phone pushes over the thread.
    pub fn pull_request_view(&self, cx: &App) -> Entity<crate::pull_requests::PullRequestView> {
        self.pull_requests.read(cx).detail()
    }

    /// The phone's entry to the linked pull requests: one chip naming every
    /// number, which opens them in a bottom sheet.
    fn render_pull_request_pill(&self, cx: &App) -> Option<AnyElement> {
        let links = self.active_pull_requests(cx)?;
        let numbers = crate::pull_requests::numbers(&links, cx)?;
        let label = crate::pull_requests::badge_label(&links, cx)?;
        let panel = self.pull_requests.clone();
        Some(
            crate::widgets::Popover::new("phone-pull-requests")
                .bottom_sheet(crate::tr!("pull_requests.title").into_owned())
                .trigger(
                    Button::new("phone-pr-pill")
                        .debug_selector(|| "phone-pr-pill".into())
                        .ghost()
                        .outline()
                        .chip()
                        .aria_label(label)
                        .child(numbers),
                )
                .content(move |_, _, _| panel.clone())
                .into_any_element(),
        )
    }

    fn on_link_dialog(
        &mut self,
        _: &crate::pull_requests::OpenLinkDialog,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(id) = self.workspace_store.read(cx).active_session_id() {
            crate::pull_requests::open_link_dialog(self.workspace_store.clone(), id, window, cx);
        }
    }

    fn on_move_thread_dialog(
        &mut self,
        _: &OpenMoveThreadDialog,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(id) = self.workspace_store.read(cx).active_session_id() {
            crate::move_thread_dialog::open(self.workspace_store.clone(), id, window, cx);
        }
    }

    fn on_revoke_self_settle(
        &mut self,
        _: &RevokeSelfSettle,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(id) = self.workspace_store.read(cx).active_session_id() {
            self.workspace_store
                .update(cx, |store, _| store.revoke_self_settle(id));
        }
    }

    fn on_show_agent_tools(
        &mut self,
        _: &crate::pull_requests::ShowAgentTools,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        crate::pull_requests::open_agent_sees(self.workspace_store.clone(), window, cx);
    }

    /// The header, laid out for the width it is given in this frame: whether
    /// its split buttons are labelled depends on that width.
    fn render_header(
        &self,
        title: Option<String>,
        is_draft: bool,
        cwd: Option<PathBuf>,
        agents: Option<&crate::agents_panel::Agents>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let agents_cue = agents.map(|agents| agents.cue(cx));
        let header = cx.processor(move |this, width: gpui::Pixels, window, cx| {
            this.render_header_bar(
                title.clone(),
                is_draft,
                cwd.clone(),
                agents_cue,
                f32::from(width),
                window,
                cx,
            )
        });
        gpui::container_query(move |size, window, cx| header(size.width, window, cx))
            .flex_none()
            .h(px(crate::material::bar_height(
                crate::material::Bar::Strip,
                false,
            )))
            .into_any_element()
    }

    #[allow(clippy::too_many_arguments)]
    fn render_header_bar(
        &self,
        title: Option<String>,
        is_draft: bool,
        cwd: Option<PathBuf>,
        agents_cue: Option<Option<gpui::Hsla>>,
        width: f32,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let collapsed = self.window_state.read(cx).sidebar_collapsed;
        // Windows: with no right panel open this header is the window's
        // top-right corner, so it hosts the caption buttons — flush to the
        // right edge, past the header's usual inset.
        let (right_panel_open, right_tab) = self.workspace_store.read(cx).window_caption_state();
        let hosts_caption = window_caption::hosts_caption_for_state(
            window_caption::CaptionSurface::Chat,
            self.window_state.read(cx).route(),
            right_panel_open,
            right_tab,
        );
        let labelled_extra = header_extra_width(
            self.workspace_store.read(cx),
            collapsed,
            window.is_fullscreen(),
            true,
        );
        let labelled = width - labelled_extra >= HEADER_LABELS_MIN_WIDTH;
        let traffic_lights = window_caption::traffic_light_inset(window);
        let base = crate::material::bar("chat-header", crate::material::Bar::Strip, false, cx)
            .when(collapsed && traffic_lights > px(0.), |this| {
                this.pl(traffic_lights)
            })
            .when(hosts_caption, |this| this.pr_0())
            .min_w(px(HEADER_COMPACT_MIN_WIDTH
                + header_extra_width(
                    self.workspace_store.read(cx),
                    collapsed,
                    window.is_fullscreen(),
                    false,
                )
                + if hosts_caption {
                    window_caption::RIGHT_PANEL_CAPTION_WIDTH
                } else {
                    0.
                }));

        // The sidebar toggle: the header's first control, immediately left of
        // the title. It lives here rather than in the sidebar because a
        // collapsed sidebar occupies no width at all (`crate::shell`), so a
        // control mounted inside it would have nowhere to be.
        let sidebar_toggle = Button::new("toggle-sidebar")
            .debug_selector(|| "toggle-sidebar".into())
            // A control press must not arm the header window drag region.
            .on_mouse_down(gpui::MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .ghost()
            .small()
            .compact()
            .icon(if collapsed {
                IconName::PanelLeftOpen
            } else {
                IconName::PanelLeft
            })
            .tooltip(if collapsed {
                crate::tr!("sidebar.expand")
            } else {
                crate::tr!("sidebar.collapse")
            })
            .on_click(cx.listener(|this, _, _, cx| {
                let workspace_store = this.workspace_store.clone();
                this.window_state.update(cx, |state, cx| {
                    state.toggle_sidebar_collapsed(&workspace_store, cx)
                });
            }));

        // An agent's header leads back to the thread it was reached from.
        let parent_link = self.parent_link(labelled, cx);

        // A draft shows a muted "New thread" label; an open thread its title;
        // nothing active shows "No active thread". The title stretch carries no
        // controls, so it doubles as the window's native drag handle where the
        // platform needs one.
        let title_el = if is_draft {
            crate::material::bar_title(crate::tr!("chat.new_thread"), cx)
                .text_color(cx.theme().muted_foreground)
        } else {
            match &title {
                Some(title) => crate::material::bar_title(title.clone(), cx)
                    .debug_selector(|| "chat-title".into())
                    // Keep a few words of the title even when the diff panel and
                    // the git/Open buttons squeeze the header; without a floor it
                    // collapses to a lone "I…".
                    .min_w(px(120.)),
                None => crate::material::bar_title(crate::tr!("chat.no_active_thread"), cx)
                    .text_color(cx.theme().muted_foreground),
            }
        };

        // The right-side cluster (Open split-button + panel toggles) shows for
        // any active thread, including a draft.
        let show_actions = is_draft || title.is_some();
        let panel = self.workspace_store.read(cx).panel_state();
        let right_panel_open = panel.right_panel_open;
        let right_tab = panel.right_tab;
        let plan_showing = right_panel_open && right_tab == RightTab::Plan;
        let agents_showing = right_panel_open && right_tab == RightTab::Agents;
        let preview_showing = right_panel_open && right_tab == RightTab::Preview;
        let terminal_open = panel.terminal_open && !self.window_state.read(cx).compact;
        let diff_showing = right_panel_open && right_tab == RightTab::Diff;
        let agent_tools = self
            .workspace_store
            .read(cx)
            .session_status()
            .is_some_and(|status| status.injected_tools.is_some());
        let self_settles = self
            .workspace_store
            .read(cx)
            .session_status()
            .and_then(|status| status.control.as_ref())
            .is_some_and(|control| control.self_settle.is_some());
        let movable = !is_draft && {
            let store = self.workspace_store.read(cx);
            let active = store.active_session_id();
            store
                .sidebar_sessions()
                .iter()
                .any(|meta| Some(&meta.id) == active.as_ref() && meta.worktree.is_none())
        };
        let title_menu = {
            let title = title.clone();
            let cwd = cwd.clone();
            move |menu: PopupMenu, _: &mut Window, _: &mut Context<PopupMenu>| {
                let menu = menu.when_some(title.clone(), |menu, title| {
                    menu.menu(
                        crate::tr!("chat.copy_title").into_owned(),
                        Box::new(crate::widgets::menu::CopyText(title)),
                    )
                });
                let menu = menu
                    .separator()
                    .menu(
                        crate::tr!("pull_requests.link_menu").into_owned(),
                        Box::new(crate::pull_requests::OpenLinkDialog),
                    )
                    .when(movable, |menu| {
                        menu.menu(
                            crate::tr!("sidebar.ctx_move_thread").into_owned(),
                            Box::new(OpenMoveThreadDialog),
                        )
                    })
                    .when(self_settles, |menu| {
                        menu.menu(
                            crate::tr!("control.self_settle.revoke").into_owned(),
                            Box::new(RevokeSelfSettle),
                        )
                    })
                    .when(agent_tools, |menu| {
                        menu.menu(
                            crate::tr!("pull_requests.agent_sees").into_owned(),
                            Box::new(crate::pull_requests::ShowAgentTools),
                        )
                    });
                match &cwd {
                    Some(cwd) => menu.separator().path_items(&cwd.to_string_lossy(), None),
                    None => menu,
                }
            }
        };
        let header = window_drag_area("chat-header-drag", base, window, cx)
            .child(sidebar_toggle)
            .children(parent_link)
            .child(
                h_flex()
                    .flex_1()
                    .min_w(px(168.))
                    // The title keeps its floor; numbers past the stretch's end
                    // clip here rather than run under the git button.
                    .overflow_hidden()
                    .gap_2()
                    .items_center()
                    .child(
                        // Sized by its text, not `bar_title`'s zero basis, so the title
                        // runs its full length and the badge follows it.
                        window_caption::drag_region(title_el.flex_initial())
                            .context_menu(title_menu),
                    )
                    .when_some(
                        self.active_pull_requests(cx).and_then(|links| {
                            Some((
                                crate::pull_requests::numbers(&links, cx)?,
                                crate::pull_requests::badge_label(&links, cx)?,
                            ))
                        }),
                        |header, (numbers, label)| {
                            // Every number shows until the numbers reach half the
                            // title's stretch; past that they clip, so the title
                            // keeps the other half however many are linked.
                            header.child(
                                h_flex()
                                    .flex_none()
                                    .max_w(gpui::relative(0.5))
                                    .overflow_hidden()
                                    .child(
                                        Button::new("header-pr-badge")
                                            .debug_selector(|| "header-pr-badge".into())
                                            .ghost()
                                            .small()
                                            .compact()
                                            .selected(
                                                right_panel_open
                                                    && right_tab == RightTab::PullRequests,
                                            )
                                            .tooltip(crate::tr!("pull_requests.toggle"))
                                            .aria_label(label)
                                            .child(numbers)
                                            .on_click(cx.listener(|this, _, _, cx| {
                                                this.workspace_store.update(cx, |store, cx| {
                                                    store.toggle_pull_requests_panel(cx)
                                                })
                                            })),
                                    ),
                            )
                        },
                    ),
            )
            .when(show_actions, |this| {
                this.children(self.render_git_button(labelled, cx))
                    .children(
                        cwd.clone()
                            .map(|cwd| self.render_open_button(cwd, labelled, cx)),
                    )
                    .child(
                        h_flex()
                            .flex_none()
                            .gap_1()
                            .child(
                                Button::new("panel-layout")
                                    // These panel controls must not arm the header window drag region.
                                    .on_mouse_down(gpui::MouseButton::Left, |_, _, cx| {
                                        cx.stop_propagation()
                                    })
                                    .ghost()
                                    .small()
                                    .compact()
                                    .icon(IconName::PanelBottom)
                                    .selected(terminal_open)
                                    .tooltip(crate::tr!("chat.toggle_terminal"))
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.workspace_store
                                            .update(cx, |store, cx| store.toggle_terminal_panel(cx))
                                    })),
                            )
                            .child(
                                Button::new("plan-panel")
                                    .on_mouse_down(gpui::MouseButton::Left, |_, _, cx| {
                                        cx.stop_propagation()
                                    })
                                    .ghost()
                                    .small()
                                    .compact()
                                    .icon(IconName::Map)
                                    .selected(plan_showing)
                                    .tooltip(crate::tr!("chat.toggle_tasks"))
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.workspace_store
                                            .update(cx, |store, cx| store.toggle_plan_panel(cx));
                                    })),
                            )
                            .when_some(agents_cue, |row, cue| {
                                row.child(
                                    div()
                                        .relative()
                                        .child(
                                            Button::new("agents-panel")
                                                .on_mouse_down(
                                                    gpui::MouseButton::Left,
                                                    |_, _, cx| cx.stop_propagation(),
                                                )
                                                .ghost()
                                                .small()
                                                .compact()
                                                .icon(IconName::Bot)
                                                .selected(agents_showing)
                                                .tooltip(crate::tr!("agents.toggle"))
                                                .on_click(cx.listener(|this, _, _, cx| {
                                                    this.workspace_store.update(cx, |store, cx| {
                                                        store.toggle_agents_panel(cx)
                                                    });
                                                })),
                                        )
                                        .when_some(cue, |toggle, color| {
                                            toggle.child(
                                                div()
                                                    .absolute()
                                                    .top(px(3.))
                                                    .right(px(3.))
                                                    .size(px(6.))
                                                    .rounded_full()
                                                    .bg(color)
                                                    .debug_selector(|| "agents-cue".into()),
                                            )
                                        }),
                                )
                            })
                            .when(self.workspace_store.read(cx).offers_preview(), |row| {
                                row.child(
                                    Button::new("preview-panel")
                                        .on_mouse_down(gpui::MouseButton::Left, |_, _, cx| {
                                            cx.stop_propagation()
                                        })
                                        .ghost()
                                        .small()
                                        .compact()
                                        .icon(IconName::Globe)
                                        .selected(preview_showing)
                                        .tooltip(crate::tr!("chat.toggle_preview"))
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.workspace_store.update(cx, |store, cx| {
                                                store.toggle_preview_panel(cx)
                                            });
                                        })),
                                )
                            })
                            .child(
                                Button::new("diff-panel")
                                    .debug_selector(|| "diff-panel".into())
                                    .on_mouse_down(gpui::MouseButton::Left, |_, _, cx| {
                                        cx.stop_propagation()
                                    })
                                    .ghost()
                                    .small()
                                    .compact()
                                    .icon(IconName::PanelRight)
                                    .selected(diff_showing)
                                    .tooltip(crate::tr!("chat.toggle_diff"))
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.workspace_store
                                            .update(cx, |store, cx| store.toggle_diff_panel(cx));
                                    })),
                            ),
                    )
            })
            // Last child, so the header's own actions keep their places to the
            // left of it.
            .children(hosts_caption.then(|| window_caption::caption_controls(window, cx)));
        div()
            .id("chat-header-scroll")
            .debug_selector(|| "chat-header-scroll".into())
            .flex_none()
            .w_full()
            .h(px(crate::material::bar_height(
                crate::material::Bar::Strip,
                false,
            )))
            .child(header)
            .overflow_x_scroll_area()
            .into_any_element()
    }

    /// Git quick-action split button whose primary action and dropdown choices
    /// follow the current git status.
    /// Unlabelled, the main part is its icon, named by its tooltip (or the
    /// disabled hint) and its accessibility label.
    fn render_git_button(&self, labelled: bool, cx: &mut Context<Self>) -> Option<AnyElement> {
        let (quick, items) = self.workspace_store.read(cx).chat_git_controls()?;
        let border = cx.theme().border;

        let label: SharedString = crate::tr!(git_action_label_key(quick.label))
            .into_owned()
            .into();
        let main_icon = quick
            .action
            .map(git_action_icon)
            .unwrap_or_else(|| Icon::empty().path("icons/git-branch.svg"));
        let mut main = Button::new("git-main")
            .debug_selector(|| "git-main".into())
            .ghost()
            .small()
            .rounded_corners(segment_corners(true, cx))
            .disabled(quick.disabled)
            .aria_label(label.clone())
            .icon(main_icon)
            .when(labelled, |main| main.label(label.clone()));
        if quick.disabled {
            if let Some(hint) = quick.hint {
                main = main.tooltip(crate::tr!(git_hint_key(hint)).into_owned());
            }
        } else if let Some(action) = quick.action {
            main = main
                .when(!labelled, |main| main.tooltip(label.clone()))
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.trigger_git_action(action, window, cx);
                }));
        }

        // Dropdown listing the applicable subset. Menu rows dispatch through the
        // ChatView entity (the popover content runs at App level, not in a view
        // context, so `cx.listener` is unavailable here).
        let chat = cx.entity();
        let chevron = crate::material::overlay_popover("git-menu", cx)
            .anchor(Anchor::TopRight)
            .trigger(
                Button::new("git-menu-trigger")
                    .ghost()
                    .small()
                    .rounded_corners(segment_corners(false, cx))
                    .icon(IconName::ChevronDown),
            )
            .content(move |_state, _window, cx| {
                let muted = cx.theme().muted_foreground;
                let hover = cx.theme().muted;
                let popover = cx.entity();
                let mut menu = v_flex().w(px(210.)).p_1().gap_0p5();
                for (index, item) in items.clone().into_iter().enumerate() {
                    let label: SharedString = crate::tr!(git_action_label_key(item.action))
                        .into_owned()
                        .into();
                    let action = item.action;
                    let disabled = item.disabled;
                    let popover = popover.clone();
                    let chat = chat.clone();
                    let frame = crate::material::menu_item_frame(false, cx)
                        .w_full()
                        .items_center()
                        .text_body(false, cx)
                        .child(git_action_icon(action).text_color(muted))
                        .child(div().flex_1().child(label.clone()));
                    let mut row = if disabled {
                        frame
                            .id(("git-menu-item", index))
                            .role(Role::MenuItem)
                            .aria_label(label)
                    } else {
                        crate::material::accessible_clickable(
                            frame,
                            ("git-menu-item", index),
                            Role::MenuItem,
                            label,
                            cx,
                        )
                    };
                    if disabled {
                        row = row.text_color(muted);
                        if let Some(hint) = item.hint {
                            let text: SharedString =
                                crate::tr!(git_hint_key(hint)).into_owned().into();
                            row = row.tooltip(move |window, cx| {
                                Tooltip::new(text.clone()).build(window, cx)
                            });
                        }
                    } else {
                        row = row.cursor_pointer().hover(move |s| s.bg(hover)).on_click(
                            move |_, window, cx| {
                                popover.update(cx, |st, cx| st.dismiss(window, cx));
                                chat.update(cx, |this, cx| {
                                    this.trigger_git_action(action, window, cx)
                                });
                            },
                        );
                    }
                    menu = menu.child(row);
                }
                menu.into_any_element()
            });

        Some(
            h_flex()
                .flex_none()
                .items_center()
                .rounded(crate::material::radius_input(cx))
                .text_body(false, cx)
                .border_1()
                .border_color(border)
                .overflow_hidden()
                .child(main)
                // A control press must not arm the header window drag region.
                .on_mouse_down(gpui::MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .child(div().w_px().h(px(16.)).bg(border))
                .child(chevron)
                .into_any_element(),
        )
    }

    /// Dispatch a git quick-action: commit-style actions open the commit dialog;
    /// everything else runs in the background with a progress toast.
    fn trigger_git_action(
        &mut self,
        action: GitAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if action.opens_commit_dialog() {
            self.open_commit_dialog(action, window, cx);
        } else {
            self.workspace_store.update(cx, |store, _cx| {
                store.run_git_action(action, None, None, None)
            });
        }
    }

    /// Open the commit dialog for `action` (Commit or Commit & push).
    fn open_commit_dialog(
        &mut self,
        action: GitAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let dialog =
            cx.new(|cx| CommitDialog::new(self.workspace_store.clone(), action, window, cx));
        self.commit_dialog = Some(dialog.clone());
        window.open_dialog(cx, move |dlg, window, cx| {
            let content = dialog.clone();
            let footer_dialog = dialog.clone();
            dlg.title(crate::tr!("git.commit.title").into_owned())
                .wide()
                .content(move |content_el, _window, _cx| content_el.child(content.clone()))
                .footer(render_commit_footer(&footer_dialog, window, cx))
        });
    }

    /// Open the session cwd in Zed, or choose a directory action from the menu.
    /// Unlabelled, the main part is its icon, named by its tooltip and its
    /// accessibility label.
    fn render_open_button(
        &self,
        cwd: PathBuf,
        labelled: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let border = cx.theme().border;
        let main_cwd = cwd.clone();
        let menu_cwd = cwd;

        let chevron = crate::material::overlay_popover("open-menu", cx)
            .anchor(Anchor::TopRight)
            .trigger(
                Button::new("open-menu-trigger")
                    .ghost()
                    .small()
                    .rounded_corners(segment_corners(false, cx))
                    .icon(IconName::ChevronDown),
            )
            .content(move |_state, _window, cx| {
                let zed_cwd = menu_cwd.clone();
                let reveal_cwd = menu_cwd.clone();
                let copy_cwd = menu_cwd.clone();
                let popover = cx.entity();
                let p1 = popover.clone();
                let p2 = popover.clone();
                let p3 = popover.clone();
                let muted = cx.theme().muted_foreground;
                let hover = cx.theme().muted;
                let menu_item = move |id: &'static str, icon: IconName, label: SharedString| {
                    crate::material::accessible_clickable(
                        crate::material::menu_item_frame(false, cx),
                        id,
                        Role::MenuItem,
                        label.clone(),
                        cx,
                    )
                    .w_full()
                    .items_center()
                    .cursor_pointer()
                    .text_body(false, cx)
                    .hover(move |s| s.bg(hover))
                    .child(Icon::new(icon).text_color(muted))
                    .child(label)
                };
                v_flex()
                    .w(px(180.))
                    .p_1()
                    .gap_0p5()
                    .child(
                        menu_item(
                            "open-zed",
                            IconName::ExternalLink,
                            crate::tr!("chat.open_zed").into_owned().into(),
                        )
                        .on_click(move |_, window, cx| {
                            open_in_zed(&zed_cwd, window, cx);
                            p1.update(cx, |st, cx| st.dismiss(window, cx));
                        }),
                    )
                    .child(
                        menu_item(
                            "reveal-in-file-manager",
                            IconName::FolderOpen,
                            crate::tr!("chat.reveal_in_file_manager")
                                .into_owned()
                                .into(),
                        )
                        .on_click(move |_, window, cx| {
                            cx.reveal_path(&reveal_cwd);
                            p2.update(cx, |st, cx| st.dismiss(window, cx));
                        }),
                    )
                    .child(
                        menu_item(
                            "copy-path",
                            IconName::Copy,
                            crate::tr!("chat.copy_path").into_owned().into(),
                        )
                        .on_click(move |_, window, cx| {
                            cx.write_to_clipboard(ClipboardItem::new_string(
                                copy_cwd.display().to_string(),
                            ));
                            p3.update(cx, |st, cx| st.dismiss(window, cx));
                        }),
                    )
                    .into_any_element()
            });

        h_flex()
            .flex_none()
            // A control press must not arm the header window drag region.
            .on_mouse_down(gpui::MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .items_center()
            .rounded(crate::material::radius_input(cx))
            .text_body(false, cx)
            .border_1()
            .border_color(border)
            .overflow_hidden()
            .child(
                Button::new("open-main")
                    .ghost()
                    .small()
                    .rounded_corners(segment_corners(true, cx))
                    .aria_label(crate::tr!("chat.open"))
                    .icon(Icon::new(IconName::ExternalLink).text_color(cx.theme().muted_foreground))
                    .map(|main| {
                        if labelled {
                            main.label(crate::tr!("chat.open"))
                        } else {
                            main.tooltip(crate::tr!("chat.open"))
                        }
                    })
                    .on_click(cx.listener(move |_, _, window, cx| {
                        open_in_zed(&main_cwd, window, cx);
                    })),
            )
            .child(div().w_px().h(px(16.)).bg(border))
            .child(chevron)
            .into_any_element()
    }

    /// The phone's "new thread" empty state: which project the thread
    /// starts in, and which provider/model the first message reaches.
    fn render_compact_draft_empty(
        &self,
        cwd: &std::path::Path,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let store = self.workspace_store.read(cx);
        let project = store
            .projects()
            .into_iter()
            .find(|project| project.root == cwd)
            .map(|project| project.name)
            .unwrap_or_else(|| {
                cwd.file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_default()
            });
        let composer = store.composer_state();
        let model = composer
            .active_model
            .as_ref()
            .map(|active| {
                let name = composer
                    .active_model_spec
                    .as_ref()
                    .map(|spec| spec.display_name.clone())
                    .or_else(|| active.model.clone())
                    .unwrap_or_default();
                let provider = crate::settings::provider_label(active.provider);
                if name.is_empty() {
                    provider.to_string()
                } else {
                    format!("{provider} · {name}")
                }
            })
            .unwrap_or_default();
        crate::material::empty_state(
            Icon::empty().path("icons/message-square.svg"),
            crate::tr!("mobile.draft_empty_title", project = project),
            crate::tr!("mobile.draft_empty_body", model = model),
            cx,
        )
        .into_any_element()
    }

    fn render_empty_state(&self, _window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        // A phone reaches this state when the host has no threads. Project
        // creation and desktop shortcut hints belong to the desktop launcher.
        if !self.workspace_store.read(cx).scope().is_full() {
            return crate::material::empty_state(
                Icon::new(IconName::Folder),
                crate::tr!("member.empty_title"),
                crate::tr!("member.empty_description"),
                cx,
            )
            .into_any_element();
        }
        if self.window_state.read(cx).compact {
            return crate::material::empty_state(
                Icon::new(IconName::Folder),
                crate::tr!("mobile.projects_empty"),
                crate::tr!("mobile.projects_help"),
                cx,
            )
            .into_any_element();
        }
        let projects = self.workspace_store.read(cx).projects();
        let sessions = self.workspace_store.read(cx).sidebar_sessions();
        let hub_projects = start_hub_projects(&projects, &sessions);
        let add_project = Button::new("add-project-empty")
            .ghost()
            .small()
            .icon(
                Icon::empty()
                    .path("icons/folder-plus.svg")
                    .text_color(cx.theme().muted_foreground),
            )
            .label(crate::tr!("sidebar.add_project"))
            .on_click(cx.listener(|this, _, window, cx| {
                crate::add_project_dialog::open(this.workspace_store.clone(), window, cx);
            }));

        if projects.is_empty() {
            return crate::material::empty_state(
                Icon::new(IconName::Folder),
                crate::tr!("chat.no_projects_title"),
                crate::tr!("chat.no_projects_description"),
                cx,
            )
            .child(
                h_flex().gap_2().child(add_project).child(
                    Button::new("quick-chat-empty")
                        .small()
                        .label(crate::tr!("sidebar.quick_chat"))
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.workspace_store
                                .update(cx, |store, cx| store.start_scratch_draft(cx));
                        })),
                ),
            )
            .into_any_element();
        }
        let mut content = v_flex()
            .w_full()
            .max_w(px(420.))
            .px_4()
            .items_center()
            .gap_3()
            .child(div().text_title(cx).child(crate::tr!("chat.empty_title")));
        let mut launcher = v_flex().w_full().gap_1().child(
            div()
                .px_3()
                .text_size(px(10.5))
                .font_medium()
                .text_color(cx.theme().muted_foreground)
                .child(crate::material::tracked_uppercase(
                    crate::tr!("chat.start_hub_title").as_ref(),
                )),
        );
        for (project, last_activity) in hub_projects {
            let project_id = project.id.clone();
            let cwd = project.root.clone();
            let row_label = crate::tr!("sidebar.project", name = project.name.clone()).into_owned();
            launcher = launcher.child(
                crate::material::accessible_clickable(
                    crate::material::selectable_row(h_flex(), false, false, cx),
                    SharedString::from(format!("start-hub-project-{}", project.id)),
                    Role::Button,
                    row_label,
                    cx,
                )
                .cursor_pointer()
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.workspace_store.update(cx, |store, cx| {
                        store.start_draft(project_id.clone(), cwd.clone(), cx);
                    });
                }))
                .child(crate::project_icon::artwork(&project, 16.))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .text_body(false, cx)
                        .text_color(cx.theme().foreground)
                        .overflow_hidden()
                        .child(crate::widgets::fade_text(project.name)),
                )
                .when_some(last_activity, |row, last_activity| {
                    row.child(div().flex_none().text_secondary(false, cx).child(
                        crate::time::humanize_ago(now_secs().saturating_sub(last_activity)),
                    ))
                }),
            );
        }
        content = content.child(launcher).child(
            h_flex()
                .items_center()
                .justify_center()
                .gap_2()
                .child(add_project)
                .child(div().text_secondary(false, cx).child(crate::tr!(
                    "chat.palette_hint",
                    shortcut = format_secondary_shortcut("k")
                ))),
        );

        v_flex()
            .flex_1()
            .min_h_0()
            .items_center()
            .justify_center()
            .child(content)
            .into_any_element()
    }

    fn render_scroll_pill(&self, cx: &mut Context<Self>) -> AnyElement {
        // Absolute positioning ignores mx_auto without pinned horizontal
        // insets; span the region and center with flex instead.
        div()
            .absolute()
            .bottom(px(12.) + self.phone_strip_height)
            .left_0()
            .right_0()
            .flex()
            .justify_center()
            .child(
                floating_backing(crate::material::radius_button(cx), cx).child(
                    Button::new("scroll-to-end")
                        .debug_selector(|| "scroll-to-end".into())
                        .outline()
                        .small()
                        .icon(IconName::ChevronDown)
                        .label(crate::tr!("chat.scroll_end"))
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.list_state.set_follow_mode(FollowMode::Tail);
                            this.list_state.scroll_to_end();
                            cx.notify();
                        })),
                ),
            )
            .into_any_element()
    }
}

/// An opaque backing for a control floating over the timeline: an outline or
/// ghost button's fill is near transparent, and the chat text scrolling
/// beneath would show through it. Scrolling still reaches the timeline.
fn floating_backing(radius: gpui::Pixels, cx: &App) -> gpui::Div {
    div()
        .flex_none()
        .block_mouse_except_scroll()
        .rounded(radius)
        .bg(cx.theme().popover)
        .shadow_md()
}

/// The Markdown a message renders: an assistant message's text, or a user
/// message's visible text.
fn entry_markdown(entry: &TimelineEntry) -> Option<String> {
    match &entry.content {
        EntryContent::Item(ItemContent::AssistantMessage { text }) => Some(text.clone()),
        content => {
            let (text, _, context_len, _) = user_content(content)?;
            Some(plain_text_as_markdown(user_visible_text(text, context_len)))
        }
    }
}

/// The transcript's selection document: its entries are the messages at
/// their timeline indices, read from their resident Markdown when they have
/// one and parsed from the timeline when they are not resident.
fn transcript_selection(
    chat: gpui::WeakEntity<ChatView>,
    cx: &mut App,
) -> Entity<SelectionDocument> {
    cx.new(|_| {
        SelectionDocument::new(Rc::new(move |range, cx| {
            let Some(chat) = chat.upgrade() else {
                return Vec::new();
            };
            let chat = chat.read(cx);
            chat.workspace_store
                .read(cx)
                .with_active_timeline(|timeline| {
                    let (start, end) = (*range.start() as usize, *range.end() as usize);
                    timeline
                        .entries
                        .iter()
                        .enumerate()
                        .skip(start)
                        .take(end.saturating_sub(start) + 1)
                        .filter_map(|(index, entry)| {
                            let root = match chat.md_states.get(&entry.id) {
                                Some(md) => md.state.read(cx).parsed_root(),
                                None => Rc::new(
                                    crate::markdown::parse::parse_document(&entry_markdown(entry)?)
                                        .root,
                                ),
                            };
                            Some((index as u64, root))
                        })
                        .collect()
                })
                .unwrap_or_default()
        }))
    })
}

fn markdown_entries_for_residency(
    timeline: &Timeline,
    rows: &[TimelineRow],
    scope: &ResidencyScope,
) -> Vec<MarkdownEntry> {
    let mut entries = Vec::new();
    for (index, entry) in timeline.entries.iter().enumerate() {
        let Some(row) = row_of_entry(rows, index, entry.turn) else {
            continue;
        };
        if !scope.includes(row) {
            continue;
        }
        let markdown_bearing = matches!(
            entry.content,
            EntryContent::Item(ItemContent::AssistantMessage { .. })
        ) || user_content(&entry.content).is_some();
        if markdown_bearing {
            entries.push(MarkdownEntry {
                id: entry.id.clone(),
                row,
            });
        }
    }
    entries
}

/// The walk back into a page that replaced the reservation the reader had
/// scrolled into. See [`ChatView::apply_reservation_scroll_back`].
#[derive(Debug, Clone, Copy)]
struct ReservationScrollBack {
    distance: gpui::Pixels,
    /// A frame has laid the page out since the walk back was requested, so
    /// `scroll_by` can count its rows.
    measured: bool,
}

/// What the rows a frame painted change; see [`ChatView::painted_rows_change`].
struct PaintedRowsChange {
    tail_start: Option<usize>,
    visible: Option<Range<usize>>,
}

impl ChatView {
    /// Space above the first turn's content: the edge padding plus the history
    /// reservation. Both move to the new first row when a page lands.
    fn leading_space(&self) -> gpui::Pixels {
        px(TIMELINE_EDGE_PADDING) + self.history_placeholder_height
    }

    /// Walk back over a landed page inside the render that follows the frame
    /// which measured it. The walk is a relative scroll, so packets that moved
    /// the reader in between (a finger still panning, or a fling ticking on
    /// the frame) stay applied: those resolved against the painted anchor,
    /// and the anchor painted after this render carries the walk back.
    fn apply_reservation_scroll_back(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(back) = self.reservation_scroll_back.as_mut() else {
            return;
        };
        if !back.measured {
            back.measured = true;
            // The next frame may be idle otherwise; make it render.
            let chat = cx.entity().downgrade();
            window.on_next_frame(move |_, cx| {
                let _ = chat.update(cx, |_, cx| cx.notify());
            });
            return;
        }
        let back = self.reservation_scroll_back.take().expect("checked above");
        let list = &self.list_state;
        let anchor = list.logical_scroll_top();
        // Following the tail or resting past the end means the reader left
        // for the bottom in between; the walk back would pull them off it.
        if self.capture.is_none() && !list.is_following_tail() && anchor.item_ix < list.item_count()
        {
            list.scroll_by(-back.distance);
        }
    }
}

impl Render for ChatView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.timeline_stale {
            self.sync_markdown_states(cx);
        }
        self.inline_diffs.sweep();
        self.painted_rows.set(None);
        if self.carried_anchor {
            self.carried_anchor = !settle_carried_anchor(&self.list_state);
            if self.carried_anchor {
                // The next frame may be idle otherwise; make it render.
                let chat = cx.entity().downgrade();
                window.on_next_frame(move |_, cx| {
                    let _ = chat.update(cx, |_, cx| cx.notify());
                });
            }
        }
        let show_jump_to_latest = jump_to_latest_visible(&self.list_state);
        self.sync_markdown_scroll_position(cx);
        self.apply_reservation_scroll_back(window, cx);
        // Measure after this frame's list layout, including the initial tail
        // frame and frames caused by prepends. No scroll event is required.
        // The chat is only read, and the store updated only when the report
        // changes it: an update of either builds the chat again with the next
        // render of the view around it.
        let chat = cx.entity().downgrade();
        window.on_next_frame(move |_, cx| {
            let Some(chat) = chat.upgrade() else {
                return;
            };
            let chat = chat.read(cx);
            if chat.capture.is_some() {
                return;
            }
            let Some(screens) = history_screens_covered(&chat.list_state, chat.leading_space())
            else {
                return;
            };
            let following_tail = chat.list_state.is_following_tail();
            let store = chat.workspace_store.clone();
            if store
                .read(cx)
                .history_window_changes(screens, following_tail)
            {
                store.update(cx, |store, cx| {
                    store.update_history_window(screens, following_tail, cx);
                });
            }
        });
        // A screen's height: the window's, which this render already knows.
        // The list's own height is only measured by this frame's layout.
        let placeholder = if self.workspace_store.read(cx).history_available() {
            window.viewport_size().height
        } else {
            px(0.)
        };
        if placeholder != self.history_placeholder_height && !self.rows.is_empty() {
            let anchor = self.list_state.logical_scroll_top();
            let following = self.list_state.is_following_tail();
            self.list_state.remeasure_items(0..1);
            if anchor.item_ix == 0 && !following {
                self.list_state.scroll_to(ListOffset {
                    item_ix: 0,
                    offset_in_item: (anchor.offset_in_item + placeholder
                        - self.history_placeholder_height)
                        .max(px(0.)),
                });
            }
            self.history_placeholder_height = placeholder;
        }
        let active = self.workspace_store.read(cx).chat_active_session();

        let compact = self.window_state.read(cx).compact;
        // The composer follows the layout in place. Rebuilding it here would
        // throw away the draft, its selection and its pending attachments every
        // time the window crossed the breakpoint.
        self.composer
            .update(cx, |composer, cx| composer.set_compact(compact, cx));
        // The phone reads on T1 paper; the desktop keeps the glass canvas.
        // The header's title menu dispatches these, and the header is a sibling of `main`.
        let root = v_flex()
            .size_full()
            .min_w_0()
            .track_focus(&self.focus_handle)
            .bg(if compact {
                crate::material::content_surface(cx)
            } else {
                crate::material::canvas(cx)
            })
            .on_action(cx.listener(Self::on_link_dialog))
            .on_action(cx.listener(Self::on_move_thread_dialog))
            .on_action(cx.listener(Self::on_revoke_self_settle))
            .on_action(cx.listener(Self::on_show_agent_tools));

        if self.workspace_store.read(cx).chat_loading() {
            return root.child(
                v_flex()
                    .size_full()
                    .when_some(
                        self.workspace_store
                            .read(cx)
                            .history_error()
                            .map(str::to_owned),
                        |container, error| container.child(div().px_4().child(error)),
                    )
                    .child(crate::material::loading_skeleton(cx)),
            );
        }

        let Some((title, cwd, is_draft)) = active else {
            return root
                .when(!self.window_state.read(cx).compact, |el| {
                    el.child(self.render_header(None, false, None, None, cx))
                })
                .child(self.render_empty_state(window, cx));
        };

        let title = if is_draft { None } else { Some(title) };
        let agents = {
            let store = self.workspace_store.read(cx);
            store
                .active_session_id()
                .map(|id| crate::agents_panel::Agents::of(store, &id))
                .filter(|agents| !agents.is_empty())
        };
        let header = self.render_header(title, is_draft, Some(cwd.clone()), agents.as_ref(), cx);
        let phone_strip = self.render_phone_strip(agents.as_ref(), cx);
        let phone_strip_height = if phone_strip.is_some() {
            px(crate::material::bar_height(
                crate::material::Bar::Toolbar,
                true,
            ))
        } else {
            px(0.)
        };
        if phone_strip_height != self.phone_strip_height {
            if let Some(last) = self.rows.len().checked_sub(1) {
                self.list_state.remeasure_items(last..last + 1);
            }
            self.phone_strip_height = phone_strip_height;
        }
        let panel = self.workspace_store.read(cx).panel_state();
        let terminal_open = panel.terminal_open && !self.window_state.read(cx).compact;
        let terminal_height = panel.terminal_height;

        // Group entries by turn and render each turn section into the centered
        // content column. The column fills the available width up to
        // `CONTENT_MAX_WIDTH`; horizontal padding lives on the centering wrapper
        // (below) so the column shrinks gracefully — never clipping — when the
        // diff panel narrows the chat region.
        // The newest user / assistant message: their action rows stay visible
        // (hover is not the only way to reach Copy / native rewind).
        let (last_user_id, last_assistant_id) = self
            .workspace_store
            .read(cx)
            .with_active_timeline(|timeline| latest_message_ids(&timeline.entries))
            .unwrap_or_default();

        let item_count = self.rows.len();
        let item_cwd = cwd.clone();
        let timeline = list(
            self.list_state.clone(),
            cx.processor(move |this, index: usize, window, cx| {
                let Some(row) = this.rows.get(index).cloned() else {
                    return div().into_any_element();
                };
                // The trailer's turn span comes from the timeline, not its
                // rows: a pending steer after a message belongs to no row.
                // The rows can trail the live timeline by a frame (e.g.
                // adopting a running background thread whose timeline is
                // being re-folded), so the ranges are bounds-checked.
                let own_range = match &row.part {
                    RowPart::WorkLogHeader { run } => run.clone(),
                    _ => row.entry_range.clone(),
                };
                let Some((turn, own, trailer)) = this
                    .workspace_store
                    .read(cx)
                    .with_active_timeline(|timeline| {
                        let entries = &timeline.entries;
                        let trailer = if row.last_in_turn {
                            let start = entries.partition_point(|entry| entry.turn < row.turn);
                            let len =
                                entries[start..].partition_point(|entry| entry.turn == row.turn);
                            entries[start..start + len].to_vec()
                        } else {
                            Vec::new()
                        };
                        (
                            timeline.turns.get(row.turn).cloned().unwrap_or_default(),
                            entries
                                .get(own_range)
                                .map(<[_]>::to_vec)
                                .unwrap_or_default(),
                            trailer,
                        )
                    })
                else {
                    return div().into_any_element();
                };
                let rendered = this.render_row(
                    (
                        &row,
                        &turn,
                        &item_cwd,
                        &own,
                        &trailer,
                        (last_user_id.as_deref(), last_assistant_id.as_deref()),
                    ),
                    window,
                    cx,
                );
                // An expanded Work Log's parts keep the Work Log's own rhythm.
                let continued = this.rows.get(index + 1).is_some_and(|next| {
                    matches!(next.part, RowPart::WorkLogActivity | RowPart::WorkLogLive)
                });
                let painted = this.painted_rows.clone();
                // gpui-fast 0.1.8 can retain a reparented child without measuring it.
                // Key rows by their content so expanding a Work Log does not hand
                // the following message's layout nodes to a different row (#68 upstream).
                v_flex()
                    .id(("timeline-row", row.identity))
                    .debug_selector(move || format!("timeline-row-{index}"))
                    .on_prepaint(move |_, _, _| {
                        painted.set(Some(
                            painted.get().map_or((index, index + 1), |(start, end)| {
                                (start.min(index), end.max(index + 1))
                            }),
                        ));
                    })
                    .w_full()
                    .items_center()
                    .px(px(if this.window_state.read(cx).compact {
                        crate::material::COMPACT_PAGE_INSET
                    } else {
                        CONTENT_MIN_PADDING
                    }))
                    .when(this.highlighted_turn == Some(row.turn), |item| {
                        item.rounded(crate::material::radius_card(cx))
                            .bg(cx.theme().list_active)
                    })
                    .when(index == 0, |item| item.pt(px(TIMELINE_EDGE_PADDING)))
                    .map(|item| {
                        if index + 1 == item_count {
                            item.pb(px(TIMELINE_EDGE_PADDING) + this.phone_strip_height)
                        } else if row.last_in_turn {
                            item.pb(px(TURN_GAP))
                        } else if continued {
                            item.pb_1()
                        } else {
                            item.pb(px(SEGMENT_GAP))
                        }
                    })
                    // `min_w_0`: a turn holds nowrap content (diff rows, command
                    // output). Without it this flex item grows to that content
                    // and the column runs past the page inset instead of
                    // scrolling inside it.
                    .when(
                        index == 0 && this.history_placeholder_height > px(0.),
                        |item| {
                            item.child(
                                div()
                                    .w_full()
                                    .h(this.history_placeholder_height)
                                    .flex_none()
                                    .flex()
                                    .flex_col()
                                    .justify_end()
                                    .when(
                                        this.workspace_store.read(cx).history_loading(),
                                        |space| {
                                            space.child(
                                                div()
                                                    .id("history-activity")
                                                    .h(px(24.))
                                                    .flex_none()
                                                    .flex()
                                                    .items_center()
                                                    .justify_center()
                                                    .child(
                                                        crate::widgets::spinner::Spinner::new()
                                                            .xsmall()
                                                            .color(cx.theme().muted_foreground),
                                                    ),
                                            )
                                        },
                                    ),
                            )
                        },
                    )
                    .child(
                        div()
                            .w_full()
                            .min_w_0()
                            .max_w(px(CONTENT_MAX_WIDTH))
                            .child(rendered),
                    )
                    .into_any_element()
            }),
        )
        .with_sizing_behavior(gpui::ListSizingBehavior::Auto)
        .flex_1()
        .min_h_0();

        // A fresh phone draft shows its project/model context above the focused composer.
        let timeline: AnyElement = if compact && is_draft && item_count == 0 {
            self.render_compact_draft_empty(&cwd, cx)
        } else {
            // GPUI's scrollbar extent excludes List padding, while wheel and
            // tail-resume calculations include it. Keep the inset outside the
            // list so every input path shares the same bottom and pixel offset.
            v_flex()
                .flex_1()
                .min_h_0()
                .relative()
                .py(timeline_inset(cx))
                .child(crate::scroll::page_viewport(
                    "timeline-bounce",
                    &self.list_state,
                    SelectionArea::new(&self.selection, timeline)
                        .flex()
                        .flex_col()
                        .flex_1()
                        .min_h_0(),
                ))
                .when(!window.is_inspector_picking(cx), |timeline| {
                    timeline.child(Scrollbar::vertical(&self.list_state).id("timeline-scrollbar"))
                })
                .into_any_element()
        };

        let composer = self.composer.clone().into_any_element();
        let deliveries = self.workspace_store.read(cx).delivery_messages();
        let waiting = !self
            .workspace_store
            .read(cx)
            .connection_state()
            .is_connected();
        let delivery_rows = deliveries
            .into_iter()
            .map(|(key, text, failure, acknowledged)| {
                let retry_key = key.clone();
                let discard_key = key.clone();
                let retry_store = self.workspace_store.clone();
                let discard_store = self.workspace_store.clone();
                v_flex()
                    .id(SharedString::from(format!("delivery-{key}")))
                    .debug_selector(|| "pending-delivery-bubble".into())
                    .w_full()
                    .max_w(px(CONTENT_MAX_WIDTH))
                    .items_end()
                    .gap_1()
                    .child(
                        div()
                            .max_w_3_4()
                            .px(cx.theme().tokens.spacing.md)
                            .py(cx.theme().tokens.spacing.xs)
                            .text_token(&cx.theme().tokens.typography.md)
                            .rounded(crate::material::radius_composer(cx))
                            .bg(cx.theme().foreground.opacity(0.08))
                            .text_color(if acknowledged {
                                cx.theme().foreground
                            } else {
                                cx.theme().muted_foreground
                            })
                            .child(text),
                    )
                    .when(failure.is_some() || !acknowledged, |row| {
                        row.child(
                            div()
                                .text_secondary(false, cx)
                                .text_color(if failure.is_some() {
                                    cx.theme().danger
                                } else {
                                    cx.theme().muted_foreground
                                })
                                .child(failure.clone().unwrap_or_else(|| {
                                    crate::tr!(if waiting {
                                        "chat.waiting_connection"
                                    } else {
                                        "chat.sending"
                                    })
                                    .into_owned()
                                })),
                        )
                    })
                    .when(failure.is_some(), |row| {
                        row.child(
                            h_flex()
                                .gap_2()
                                .child(
                                    Button::new(SharedString::from(format!("retry-{key}")))
                                        .ghost()
                                        .small()
                                        .debug_selector(|| "retry-delivery".into())
                                        .label(crate::tr!("chat.retry_delivery"))
                                        .on_click(move |_, _, cx| {
                                            retry_store.update(cx, |store, _| {
                                                store.retry_delivery(&retry_key)
                                            })
                                        }),
                                )
                                .child(
                                    Button::new(SharedString::from(format!("discard-{key}")))
                                        .ghost()
                                        .small()
                                        .debug_selector(|| "discard-delivery".into())
                                        .label(crate::tr!("chat.discard_delivery"))
                                        .on_click(move |_, _, cx| {
                                            discard_store.update(cx, |store, _| {
                                                store.discard_delivery(&discard_key)
                                            })
                                        }),
                                ),
                        )
                    })
            })
            .collect::<Vec<_>>();

        let main = v_flex()
            .size_full()
            .min_h_0()
            .on_action(cx.listener(Self::on_link_url))
            .on_action(cx.listener(Self::on_rewind_turn))
            .on_action(cx.listener(Self::on_open_file_diff))
            .child(
                div()
                    .id("timeline")
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_h_0()
                    .relative()
                    .child(timeline)
                    .child(
                        gpui::canvas(
                            {
                                let list = self.list_state.clone();
                                let view = cx.entity().downgrade();
                                let painted = self.painted_rows.clone();
                                move |_, window, cx| {
                                    // List prepaint may change geometry after render
                                    // (resize, splice, or markdown remeasurement).
                                    // Reconcile the sibling control after that layout.
                                    if jump_to_latest_visible(&list) != show_jump_to_latest {
                                        let view = view.clone();
                                        window.defer(cx, move |_, cx| {
                                            let _ = view.update(cx, |_, cx| cx.notify());
                                        });
                                    }
                                    // Only update the chat when the frame
                                    // changed something: an update builds it
                                    // again with the next render of the view
                                    // around it.
                                    let painted = painted.get();
                                    window.defer(cx, move |_, cx| {
                                        let Some(view) = view.upgrade() else {
                                            return;
                                        };
                                        let chat = view.read(cx);
                                        let panels = chat.command_panels.borrow_mut().settle();
                                        let painted = painted.and_then(|(start, end)| {
                                            chat.painted_rows_change(start..end)
                                        });
                                        if panels.is_empty() && painted.is_none() {
                                            return;
                                        }
                                        view.update(cx, |chat, cx| {
                                            chat.apply_command_panel_changes(panels, cx);
                                            if let Some(painted) = painted {
                                                chat.adopt_painted_rows(painted, cx);
                                            }
                                        });
                                    });
                                }
                            },
                            |_, _, _, _| {},
                        )
                        .absolute()
                        .size_full(),
                    )
                    .when(show_jump_to_latest, |this| {
                        this.child(self.render_scroll_pill(cx))
                    })
                    .children(phone_strip),
            )
            .child(
                v_flex()
                    .w_full()
                    .max_h(px(200.))
                    .id("pending-deliveries")
                    .items_center()
                    .overflow_y_scroll()
                    .px_4()
                    .gap_2()
                    .children(delivery_rows),
            )
            .child(
                div()
                    .id("chat-composer")
                    .debug_selector(|| "chat-composer".into())
                    .w_full()
                    .flex_none()
                    .child(composer),
            );

        let body: AnyElement = if terminal_open {
            let drawer = self.terminal_drawer.clone();
            let drawer_resize = self.terminal_drawer.clone();
            let width = f32::from(window.bounds().size.width);
            if !drawer.read(cx).is_size(width, terminal_height) {
                drawer.update(cx, |drawer, cx| drawer.resize(width, terminal_height, cx));
            }
            gpui_base::v_resizable("chat-terminal-panels")
                .on_resize(move |state, _, cx| {
                    let height = state.read(cx).sizes().get(1).copied();
                    if let Some(height) = height {
                        drawer_resize
                            .update(cx, |drawer, cx| drawer.resize(width, f32::from(height), cx));
                    }
                })
                .child(gpui_base::resizable_panel().child(main))
                .child(
                    gpui_base::resizable_panel()
                        .flex_none()
                        .size(px(terminal_height))
                        .size_range(px(120.)..px(600.))
                        .child(self.terminal_drawer.clone()),
                )
                .into_any_element()
        } else {
            main.into_any_element()
        };
        root.when(!self.window_state.read(cx).compact, |el| el.child(header))
            .child(body)
    }
}

/// A recorded turn's injected context is named by what it holds.
fn injected_context_label(context: &str) -> String {
    let orchestrate = || crate::tr!("chat.orchestrate_skill").into_owned();
    match tcode_core::pull_request::strip_linking_instructions(context) {
        None => orchestrate(),
        Some(rest) => {
            let instructions = crate::tr!("pull_requests.instructions_disclosure").into_owned();
            if rest.trim().is_empty() {
                instructions
            } else {
                format!("{instructions} · {}", orchestrate())
            }
        }
    }
}

fn git_action_icon(action: GitAction) -> Icon {
    match action {
        GitAction::Push => Icon::new(IconName::ArrowUp),
        GitAction::Pull => Icon::empty().path("icons/download.svg"),
        _ => Icon::empty().path("icons/git-branch.svg"),
    }
}

/// The commit dialog's footer action row (Cancel / Commit[& push]). Built inside
/// the `open_dialog` builder so the buttons can close the dialog on click.
fn render_commit_footer(
    dialog: &Entity<CommitDialog>,
    _window: &mut Window,
    cx: &mut App,
) -> AnyElement {
    let confirm_label = dialog.update(cx, |d, cx| d.confirm_label(cx));
    let cancel_dialog = dialog.clone();
    let confirm_dialog = dialog.clone();
    crate::overlay::DialogActions::new()
        .child(
            Button::new("commit-cancel")
                .ghost()
                .label(crate::tr!("git.commit.cancel"))
                .on_click(move |_, window, cx| {
                    let _ = &cancel_dialog;
                    window.close_dialog(cx);
                }),
        )
        .child(
            Button::new("commit-confirm")
                .primary()
                .label(confirm_label)
                .on_click(move |_, window, cx| {
                    let should_close = confirm_dialog.update(cx, |d, cx| d.confirm(window, cx));
                    if should_close {
                        window.close_dialog(cx);
                    }
                }),
        )
        .into_any_element()
}

/// The two ends of a segmented control's border: a half takes the control's
/// radius on its outer side only, so its hover fill stays inside the border.
fn segment_corners(leading: bool, cx: &App) -> gpui::Corners<gpui::Pixels> {
    let (radius, flat) = (crate::material::radius_input(cx), px(0.));
    if leading {
        gpui::Corners {
            top_left: radius,
            top_right: flat,
            bottom_right: flat,
            bottom_left: radius,
        }
    } else {
        gpui::Corners {
            top_left: flat,
            top_right: radius,
            bottom_right: radius,
            bottom_left: flat,
        }
    }
}

/// The breathing room between the timeline and the header/composer. A phone
/// has none: its rows already inset themselves and the screen is short.
fn timeline_inset(cx: &App) -> gpui::Pixels {
    if crate::window_seam::is_mobile(cx) {
        px(0.)
    } else {
        px(TIMELINE_INSET)
    }
}

/// Desktop timeline inset above and below the list, in pixels.
const TIMELINE_INSET: f32 = 8.;

/// Padding inside the list on its first and last row, in pixels. Unlike
/// `TIMELINE_INSET` it scrolls with the content: the header and composer still
/// clip the timeline flush, but a reader at either end sees a little air
/// between the edge turn and the chrome. It lives on the rows rather than on
/// the List element, so the scrollbar, the wheel and tail-resume all measure
/// the same extent.
const TIMELINE_EDGE_PADDING: f32 = 8.;

#[cfg(test)]
mod tests {
    use super::{
        ASYNC_MARKDOWN_THRESHOLD_BYTES, AUTO_ACTIVITY_MIN_VISIBILITY, AutoActivityExpansion,
        AutoActivityExpansions,
        AutoActivityRecency::{ImmediatelySuperseded, Latest, Older},
        ChatView, ResidencyScope, RowPart, markdown_entries_for_residency,
    };
    use crate::store::WorkspaceStore;
    use crate::window_state::WindowState;
    use agent::{ItemContent, ItemStatus};
    use gpui::{AppContext as _, Entity, TestAppContext};
    use std::sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    };
    use std::time::{Duration, Instant};
    use tcode_core::session::{EntryContent, Timeline, TimelineEntry, TurnMeta};

    const AUTO_ACTIVITY_TEST_SESSION: &str = "session-a";
    static NEXT_RESIDENCY_TEST_ID: AtomicU64 = AtomicU64::new(0);

    /// Draws a full frame. A frame that reuses any view keeps every debug
    /// bound of the previous one, so an element no longer painted would still
    /// be found.
    fn draw(cx: &mut gpui::VisualTestContext) {
        cx.run_until_parked();
        cx.update(|window, cx| {
            window.refresh();
            let _ = window.draw(cx);
        });
    }

    /// Delivers frames for as long as the window asks for them, spaced as a
    /// display spaces them: gpui-base's scrollbar motion reads the wall clock,
    /// and GPUI's animations the test clock.
    fn settle_frames(cx: &mut gpui::VisualTestContext) {
        const FRAME_BUDGET: usize = 320;
        for _ in 0..FRAME_BUDGET {
            cx.run_until_parked();
            if cx.update(|window, cx| window.simulate_next_frame(cx)) == 0 {
                return;
            }
            std::thread::sleep(Duration::from_millis(16));
            cx.executor().advance_clock(Duration::from_millis(16));
        }
        panic!("the window kept asking for frames after the {FRAME_BUDGET}-frame budget");
    }

    #[test]
    fn activity_visibility_tracks_successors_and_the_remaining_minimum_window() {
        let first_seen = Instant::now();
        for (seen_as_latest, elapsed, delay, becomes_older) in [
            (true, 200, Some(300), false),
            (true, 500, None, false),
            (true, 1_000, None, false),
            (false, 0, Some(500), false),
            (true, 100, Some(400), true),
        ] {
            let mut expansions = AutoActivityExpansions::default();
            if seen_as_latest {
                let latest = expansions.observe(
                    AUTO_ACTIVITY_TEST_SESSION,
                    "activity",
                    true,
                    Latest,
                    first_seen,
                );
                assert!(latest.expanded);
                assert_eq!(latest.collapse, None);
                let still_latest = expansions.observe(
                    AUTO_ACTIVITY_TEST_SESSION,
                    "activity",
                    true,
                    Latest,
                    first_seen + Duration::from_millis(elapsed),
                );
                assert!(still_latest.expanded);
                assert_eq!(still_latest.collapse, None);
            }
            let superseded = expansions.observe(
                AUTO_ACTIVITY_TEST_SESSION,
                "activity",
                true,
                ImmediatelySuperseded,
                first_seen + Duration::from_millis(elapsed),
            );
            assert_eq!(superseded.expanded, delay.is_some());
            assert_eq!(
                superseded.collapse.map(|(_, delay)| delay),
                delay.map(Duration::from_millis)
            );
            if let Some((generation, _)) = superseded.collapse {
                let repeated = expansions.observe(
                    AUTO_ACTIVITY_TEST_SESSION,
                    "activity",
                    true,
                    ImmediatelySuperseded,
                    first_seen + Duration::from_millis(elapsed),
                );
                assert!(repeated.expanded);
                assert_eq!(
                    repeated.collapse, None,
                    "a repaint must not enqueue a second timer"
                );
                if becomes_older {
                    expansions.observe(
                        AUTO_ACTIVITY_TEST_SESSION,
                        "next",
                        true,
                        Latest,
                        first_seen + Duration::from_millis(100),
                    );
                    let older = expansions.observe(
                        AUTO_ACTIVITY_TEST_SESSION,
                        "activity",
                        true,
                        Older,
                        first_seen + Duration::from_millis(200),
                    );
                    assert!(!older.expanded);
                    assert_eq!(older.collapse, None);
                    assert!(!expansions.finish_collapse(
                        AUTO_ACTIVITY_TEST_SESSION,
                        "activity",
                        generation,
                    ));
                    let first_old = expansions.observe(
                        AUTO_ACTIVITY_TEST_SESSION,
                        "next",
                        true,
                        ImmediatelySuperseded,
                        first_seen + Duration::from_millis(200),
                    );
                    assert!(first_old.expanded);
                    assert_eq!(
                        first_old.collapse.map(|(_, delay)| delay),
                        Some(Duration::from_millis(400))
                    );
                    assert!(
                        expansions
                            .observe(
                                AUTO_ACTIVITY_TEST_SESSION,
                                "current",
                                true,
                                Latest,
                                first_seen + Duration::from_millis(200),
                            )
                            .expanded
                    );
                    continue;
                }
                assert!(expansions.finish_collapse(
                    AUTO_ACTIVITY_TEST_SESSION,
                    "activity",
                    generation
                ));
                assert!(!expansions.finish_collapse(
                    AUTO_ACTIVITY_TEST_SESSION,
                    "activity",
                    generation
                ));
                assert!(
                    !expansions
                        .observe(
                            AUTO_ACTIVITY_TEST_SESSION,
                            "activity",
                            true,
                            ImmediatelySuperseded,
                            first_seen + Duration::from_secs(1)
                        )
                        .expanded
                );
            }
        }
    }

    #[test]
    fn returning_session_restarts_latest_activity_visibility_window() {
        let mut expansions = AutoActivityExpansions::default();
        let first_seen = Instant::now();
        expansions.observe(
            AUTO_ACTIVITY_TEST_SESSION,
            "activity-command",
            true,
            Latest,
            first_seen,
        );

        let returned_at = first_seen + Duration::from_secs(10);
        expansions.activate_session(Some(AUTO_ACTIVITY_TEST_SESSION));
        expansions.hydrate_session_snapshot(
            AUTO_ACTIVITY_TEST_SESSION,
            ["activity-command".to_string()].into(),
        );
        let returned = expansions.observe(
            AUTO_ACTIVITY_TEST_SESSION,
            "activity-command",
            true,
            Latest,
            returned_at,
        );
        assert!(returned.expanded);

        let superseded = expansions.observe(
            AUTO_ACTIVITY_TEST_SESSION,
            "activity-command",
            true,
            ImmediatelySuperseded,
            returned_at + Duration::from_millis(200),
        );
        assert!(superseded.expanded);
        assert_eq!(
            superseded.collapse.map(|(_, delay)| delay),
            Some(Duration::from_millis(300))
        );
    }

    #[gpui::test]
    fn running_session_snapshot_renders_only_latest_command_expanded(cx: &mut TestAppContext) {
        use gpui::{VisualTestContext, px, size};

        let mut timeline = Timeline::default();
        timeline.turns = vec![TurnMeta {
            running: true,
            ..TurnMeta::default()
        }];
        // TurnMeta drives the live Work Log rendering under test. Keep the
        // timeline-wide ticker idle so no perpetual clock task outlives this
        // finite visual test.
        timeline.entries.push(entry("user", user_item("go")));
        for index in 1..=5 {
            timeline.entries.push(entry(
                &format!("command-{index}"),
                EntryContent::Item(ItemContent::CommandExecution {
                    command: format!("command {index}"),
                    output: format!("output {index}"),
                    exit_code: Some(0),
                    status: ItemStatus::Completed,
                }),
            ));
        }

        let (workspace_store, window_state, session_id) = seed_chat(cx, timeline);
        let (view, cx) = cx
            .add_window_view(|window, cx| ChatView::new(workspace_store, window_state, window, cx));
        let cx: &mut VisualTestContext = cx;
        cx.simulate_resize(size(px(1_024.), px(700.)));
        cx.update(|window, cx| {
            let _ = window.draw(cx);
        });

        let states = view.read_with(cx, |chat, _| {
            let entries = chat
                .auto_activity_expansions
                .entries_by_session
                .get(&session_id)
                .expect("rendered session expansion state");
            (1..=5)
                .map(|index| entries.get(&format!("activity-command-{index}")).copied())
                .collect::<Vec<_>>()
        });
        for (index, state) in states[..4].iter().enumerate() {
            assert_eq!(
                *state,
                Some(AutoActivityExpansion::Collapsed),
                "historical command {} reopened",
                index + 1
            );
        }
        assert!(matches!(
            states[4],
            Some(AutoActivityExpansion::Expanded { .. })
        ));
    }

    #[gpui::test]
    fn pending_steer_after_the_last_segment_renders_in_the_trailer(cx: &mut TestAppContext) {
        use gpui::{VisualTestContext, px, size};
        use tcode_core::session::SteeringStatus;

        let mut timeline = Timeline::default();
        timeline.turns = vec![TurnMeta {
            running: true,
            ..TurnMeta::default()
        }];
        timeline.entries.extend([
            entry("user", user_item("go")),
            entry("assistant", assistant("On it.")),
            entry(
                "steer",
                EntryContent::Steer {
                    text: "also do this".into(),
                    status: SteeringStatus::Pending,
                    context_len: None,
                    attachments: Vec::new(),
                },
            ),
        ]);

        let (workspace_store, window_state, _) = seed_chat(cx, timeline);
        let (_view, cx) = cx
            .add_window_view(|window, cx| ChatView::new(workspace_store, window_state, window, cx));
        let cx: &mut VisualTestContext = cx;
        cx.simulate_resize(size(px(1_024.), px(700.)));
        cx.update(|window, cx| {
            let _ = window.draw(cx);
        });

        assert!(cx.debug_bounds("steering-steer").is_some());
    }

    /// The host steers its notices into a running turn: steered or still
    /// pending, a notice is a disclosure row per notice, never the user's bubble.
    #[gpui::test]
    fn steered_notices_render_as_disclosures_not_bubbles(cx: &mut TestAppContext) {
        use gpui::{VisualTestContext, px, size};
        use tcode_core::session::SteeringStatus;

        let steer = |text: String, status| EntryContent::Steer {
            text,
            status,
            context_len: None,
            attachments: Vec::new(),
        };
        let mut timeline = Timeline::default();
        timeline.turns = vec![TurnMeta {
            running: true,
            ..TurnMeta::default()
        }];
        timeline.entries.extend([
            entry("user", user_item("go")),
            entry(
                "wake",
                steer(
                    tcode_core::pull_request_watch::merged_message(5, "https://x/pull/5"),
                    SteeringStatus::Accepted,
                ),
            ),
            entry("assistant", assistant("On it.")),
            entry(
                "results",
                steer(
                    "[orchestrate] thread a (\"First\") reported.\nResult (reported via report_result):\none\n\n[orchestrate] thread b (\"Second\") failed.\nthe provider closed".into(),
                    SteeringStatus::Pending,
                ),
            ),
        ]);

        let (workspace_store, window_state, _) = seed_chat(cx, timeline);
        let (_view, cx) = cx
            .add_window_view(|window, cx| ChatView::new(workspace_store, window_state, window, cx));
        let cx: &mut VisualTestContext = cx;
        cx.simulate_resize(size(px(1_024.), px(700.)));
        draw(cx);

        for row in [
            "disclosure-pull-request-update-wake",
            "disclosure-orchestrate-callback-results-0",
            "disclosure-orchestrate-callback-results-1",
        ] {
            assert!(cx.debug_bounds(row).is_some(), "{row} is not a disclosure");
        }
        for bubble in ["steering-wake", "steering-results"] {
            assert!(cx.debug_bounds(bubble).is_none(), "{bubble} is a bubble");
        }
    }

    #[gpui::test]
    fn a_paused_reader_on_the_live_window_stays_put_as_it_folds(cx: &mut TestAppContext) {
        use gpui::{ListOffset, Modifiers, VisualTestContext, px, size};
        use tcode_core::session::SteeringStatus;

        let tool = |id: &str| {
            entry(
                id,
                EntryContent::Item(ItemContent::ToolCall {
                    presentation: None,
                    image_reads: Vec::new(),
                    name: "read".into(),
                    input: serde_json::json!({ "path": id }),
                    output: None,
                    status: ItemStatus::Completed,
                }),
            )
        };
        let mut timeline = Timeline::default();
        timeline.turns = vec![TurnMeta {
            running: true,
            ..TurnMeta::default()
        }];
        timeline.entries.push(entry("user", user_item("go")));
        for index in 0..20 {
            timeline.entries.push(tool(&format!("tool-{index}")));
        }
        // Steers queued behind the live window keep content below it.
        for index in 0..12 {
            timeline.entries.push(entry(
                &format!("steer-{index}"),
                EntryContent::Steer {
                    text: format!("queued steer {index}"),
                    status: SteeringStatus::Pending,
                    context_len: None,
                    attachments: Vec::new(),
                },
            ));
        }

        let (store, window_state, session_id) = seed_chat(cx, timeline.clone());
        let (view, cx) =
            cx.add_window_view(|window, cx| ChatView::new(store.clone(), window_state, window, cx));
        let cx: &mut VisualTestContext = cx;
        cx.simulate_resize(size(px(1_024.), px(700.)));
        draw(cx);
        view.update(cx, |chat, cx| {
            chat.list_state.scroll_to(ListOffset::default());
            cx.notify();
        });
        draw(cx);
        let header = cx
            .debug_bounds("worklog-header-0-tool-0")
            .expect("collapsed work log");
        cx.simulate_click(header.center(), Modifiers::default());
        draw(cx);
        // The reader rests two activities into the live window.
        view.update(cx, |chat, cx| {
            let live = chat
                .rows
                .iter()
                .position(|row| row.part == RowPart::WorkLogLive)
                .expect("an expanded live work log");
            chat.list_state.scroll_to(ListOffset {
                item_ix: live,
                offset_in_item: px(70.),
            });
            cx.notify();
        });
        draw(cx);
        let reading = cx
            .debug_bounds("activity-row-tool-18")
            .expect("the activity under the reader")
            .top();
        let top = |cx: &mut VisualTestContext| {
            cx.debug_bounds("activity-row-tool-18")
                .map(|bounds| bounds.top())
        };

        let update = |timeline: &Timeline, cx: &mut VisualTestContext| {
            store.update(cx, |store, cx| {
                store.set_session_replica_for_test(session_id.clone(), timeline.clone(), cx);
                cx.notify();
            });
            draw(cx);
            draw(cx);
        };
        timeline.entries.push(tool("tool-20"));
        timeline.entries.push(tool("tool-21"));
        update(&timeline, cx);
        assert_eq!(
            top(cx),
            Some(reading),
            "new activities folding the live window moved the reader"
        );

        timeline.turns[0].running = false;
        update(&timeline, cx);
        // Settled, the activity takes the folded rows' indent but keeps its
        // line.
        assert_eq!(
            top(cx),
            Some(reading),
            "the live window settling moved the reader"
        );
    }

    #[gpui::test]
    fn collapsing_work_log_preserves_following_message_bounds(cx: &mut TestAppContext) {
        use gpui::{Modifiers, px, size};

        let mut timeline = Timeline::default();
        timeline.turns = vec![TurnMeta {
            start_ts: Some(1_000),
            end_ts: Some(2_600),
            ..TurnMeta::default()
        }];
        timeline.entries = vec![
            entry("user", user_item("Check the layout.")),
            entry(
                "command",
                EntryContent::Item(ItemContent::CommandExecution {
                    command: "printf 'layout checked\\n'".into(),
                    output: "layout checked\n".into(),
                    exit_code: Some(0),
                    status: ItemStatus::Completed,
                }),
            ),
            entry(
                "answer",
                assistant(
                    "The layout check is complete. The final assistant message stays in the centred content column.\n\nThe footer belongs below this last line.",
                ),
            ),
        ];
        let (store, window_state, _) = seed_chat(cx, timeline);
        let (_view, cx) =
            cx.add_window_view(|window, cx| ChatView::new(store, window_state, window, cx));
        for (mode, width) in [
            (crate::theme::ThemeMode::Light, 1_400.),
            (crate::theme::ThemeMode::Light, 393.),
            (crate::theme::ThemeMode::Dark, 1_400.),
            (crate::theme::ThemeMode::Dark, 393.),
        ] {
            cx.update(|window, cx| crate::theme::change_mode(mode, Some(window), cx));
            cx.simulate_resize(size(px(width), px(900.)));
            draw(cx);
            let before = [
                cx.debug_bounds("markdown-block-0").expect("answer start"),
                cx.debug_bounds("markdown-block-1")
                    .expect("answer last line"),
            ];
            let footer_before = cx.debug_bounds("turn-time-row").expect("turn footer");
            for expanded in [true, false] {
                let header = cx
                    .debug_bounds("worklog-header-0-command")
                    .expect("work log toggle");
                cx.simulate_click(header.center(), Modifiers::default());
                draw(cx);
                assert_eq!(
                    cx.debug_bounds("activity-row-command").is_some(),
                    expanded,
                    "the click must toggle the work log"
                );
            }
            let after = [
                cx.debug_bounds("markdown-block-0").expect("answer start"),
                cx.debug_bounds("markdown-block-1")
                    .expect("answer last line"),
            ];
            let footer_after = cx.debug_bounds("turn-time-row").expect("turn footer");
            assert_eq!(
                after, before,
                "collapsing moved or resized the answer at {width}px in {mode:?}"
            );
            assert_eq!(footer_after, footer_before, "collapsing moved the footer");
            assert!(
                footer_after.top() >= after[1].bottom(),
                "the footer overlaps the answer's last line: {after:?}, {footer_after:?}"
            );
        }
    }

    #[gpui::test]
    fn an_expanded_work_log_lays_out_only_the_activities_on_screen(cx: &mut TestAppContext) {
        use gpui::{FollowMode, ListOffset, Modifiers, VisualTestContext, px, size};

        let mut timeline = Timeline::default();
        timeline.turns = vec![TurnMeta::default()];
        timeline.entries.push(entry("user", user_item("go")));
        for index in 0..400 {
            timeline.entries.push(command(&format!("command-{index}")));
        }
        timeline
            .entries
            .push(entry("assistant", assistant("Done.")));

        let (workspace_store, window_state, _) = seed_chat(cx, timeline);
        let (view, cx) = cx
            .add_window_view(|window, cx| ChatView::new(workspace_store, window_state, window, cx));
        let cx: &mut VisualTestContext = cx;
        cx.simulate_resize(size(px(1_024.), px(700.)));
        draw(cx);

        let header = cx
            .debug_bounds("worklog-header-0-command-0")
            .expect("collapsed work log");
        cx.simulate_click(header.center(), Modifiers::default());
        draw(cx);
        view.update(cx, |chat, cx| {
            chat.list_state.scroll_to(ListOffset::default());
            cx.notify();
        });
        draw(cx);
        assert!(cx.debug_bounds("activity-row-command-0").is_some());
        assert!(
            cx.debug_bounds("activity-row-command-399").is_none(),
            "an expanded work log must not lay out all 400 activities"
        );

        view.update(cx, |chat, cx| {
            chat.list_state.set_follow_mode(FollowMode::Tail);
            cx.notify();
        });
        draw(cx);
        assert!(cx.debug_bounds("activity-row-command-399").is_some());
        assert!(cx.debug_bounds("activity-row-command-0").is_none());

        // Opening one activity keeps the measured heights of the turn's
        // other rows, however far away.
        view.update(cx, |chat, cx| {
            chat.list_state.scroll_to(ListOffset::default());
            cx.notify();
        });
        draw(cx);
        let last = view.read_with(cx, |chat, _| chat.rows.len() - 1);
        let measured = |cx: &mut VisualTestContext| {
            view.read_with(cx, |chat, _| {
                chat.list_state.bounds_for_item(last).is_some()
            })
        };
        assert!(measured(cx));
        let row = cx
            .debug_bounds("activity-row-command-0")
            .expect("the first activity");
        cx.simulate_click(row.center(), Modifiers::default());
        draw(cx);
        assert!(cx.debug_bounds("activity-detail").is_some());
        assert!(
            measured(cx),
            "opening one activity remeasured the whole turn"
        );
    }

    #[gpui::test]
    fn a_live_file_edit_lays_out_only_the_diff_lines_on_screen(cx: &mut TestAppContext) {
        use gpui::{Modifiers, ScrollDelta, ScrollWheelEvent, TouchPhase, VisualTestContext};
        use gpui::{point, px, size};

        let mut diff = format!("@@ -1,400 +1,401 @@\n+// {}\n", "wide ".repeat(80));
        for line in 0..400 {
            diff.push_str(&format!("-fn old_{line}() {{}}\n+fn new_{line}() {{}}\n"));
        }
        let mut timeline = Timeline::default();
        timeline.turns = vec![TurnMeta {
            running: true,
            ..TurnMeta::default()
        }];
        timeline.entries = vec![entry("user", user_item("go"))];
        for note in 0..20 {
            timeline.entries.push(entry(
                &format!("note-{note}"),
                assistant("A note above the edit."),
            ));
        }
        timeline.entries.push(entry(
            "edit",
            EntryContent::Item(ItemContent::FileChange {
                changes: vec![agent::FileChange {
                    path: "src/lib.rs".into(),
                    kind: agent::FileChangeKind::Modify,
                    diff: Some(diff),
                }],
                status: ItemStatus::InProgress,
            }),
        ));
        let (store, window_state, session_id) = seed_chat(cx, timeline.clone());
        let (view, cx) =
            cx.add_window_view(|window, cx| ChatView::new(store.clone(), window_state, window, cx));
        let cx: &mut VisualTestContext = cx;
        cx.simulate_resize(size(px(600.), px(900.)));
        draw(cx);

        assert!(cx.debug_bounds("file-edit-diff-line-0").is_some());
        assert!(
            cx.debug_bounds("file-edit-diff-line-300").is_none(),
            "an inline diff must not lay out all 801 lines"
        );

        // Both axes scroll inside the diff; the timeline stays put.
        let timeline_top = |cx: &mut VisualTestContext| {
            let top = view.read_with(cx, |chat, _| chat.list_state.logical_scroll_top());
            (top.item_ix, top.offset_in_item)
        };
        let before = timeline_top(cx);
        assert!(
            view.read_with(cx, |chat, _| chat.list_state.max_offset_for_scrollbar().y) > px(0.)
        );
        let diff_bounds = cx.debug_bounds("file-edit-diff").expect("inline diff");
        let wheel = |delta, cx: &mut VisualTestContext| {
            for (touch_phase, delta) in [
                (TouchPhase::Started, point(px(0.), px(0.))),
                (TouchPhase::Moved, delta),
                (TouchPhase::Ended, point(px(0.), px(0.))),
            ] {
                cx.simulate_event(ScrollWheelEvent {
                    position: diff_bounds.center(),
                    delta: ScrollDelta::Pixels(delta),
                    modifiers: Modifiers::default(),
                    touch_phase,
                });
            }
            draw(cx);
        };
        wheel(point(px(0.), px(-2_000.)), cx);
        assert!(cx.debug_bounds("file-edit-diff-line-0").is_none());
        // `debug_bounds` takes a `'static` selector.
        let shown: &'static str = (1..801)
            .map(|ix| &*format!("file-edit-diff-line-{ix}").leak())
            .find(|selector| cx.debug_bounds(selector).is_some())
            .expect("a line scrolled into the diff");
        let line = cx.debug_bounds(shown).unwrap();
        wheel(point(px(-200.), px(0.)), cx);
        let scrolled = cx.debug_bounds(shown).unwrap();
        assert!(
            scrolled.left() < line.left(),
            "{scrolled:?} did not scroll left of {line:?}"
        );
        assert_eq!(scrolled.top(), line.top());
        assert_eq!(timeline_top(cx), before);

        // The provider reports a different patch for the same edit.
        timeline.entries[21] = entry(
            "edit",
            EntryContent::Item(ItemContent::FileChange {
                changes: vec![agent::FileChange {
                    path: "src/lib.rs".into(),
                    kind: agent::FileChangeKind::Modify,
                    diff: Some("@@ -1 +1 @@\n-fn old() {}\n+fn new() {}\n".into()),
                }],
                status: ItemStatus::InProgress,
            }),
        );
        store.update(cx, |store, cx| {
            store.set_session_replica_for_test(session_id, timeline, cx);
        });
        draw(cx);
        assert!(cx.debug_bounds("file-edit-diff-line-1").is_some());
        assert!(cx.debug_bounds("file-edit-diff-line-2").is_none());
    }

    #[gpui::test]
    fn markdown_on_screen_above_an_expanded_work_log_is_built(cx: &mut TestAppContext) {
        use gpui::{FollowMode, ListOffset, Modifiers, VisualTestContext, px, size};

        let mut timeline = Timeline::default();
        timeline.turns = vec![TurnMeta::default()];
        timeline.entries = vec![entry("user", user_item("go"))];
        for index in 0..100 {
            timeline.entries.push(entry(
                &format!("note-{index}"),
                assistant(&format!("Note {index}.")),
            ));
        }
        timeline
            .entries
            .push(entry("early", assistant("Looking around first.")));
        for index in 0..40 {
            timeline.entries.push(command(&format!("command-{index}")));
        }
        timeline.entries.push(entry("late", assistant("Done.")));
        let (store, window_state, _) = seed_chat(cx, timeline);
        let (view, cx) =
            cx.add_window_view(|window, cx| ChatView::new(store, window_state, window, cx));
        let cx: &mut VisualTestContext = cx;
        let draw = |cx: &mut VisualTestContext| {
            cx.run_until_parked();
            cx.update(|window, cx| {
                let _ = window.draw(cx);
            });
            cx.run_until_parked();
            cx.update(|window, cx| {
                let _ = window.draw(cx);
            });
        };
        cx.simulate_resize(size(px(1_024.), px(1_900.)));
        draw(cx);
        let header = cx
            .debug_bounds("worklog-header-0-command-0")
            .expect("collapsed work log");
        cx.simulate_click(header.center(), Modifiers::default());
        draw(cx);

        // Reading the start of the thread retires the message; returning to
        // the latest shows it again, dozens of short rows above the end.
        view.update(cx, |chat, cx| {
            chat.list_state.scroll_to(ListOffset::default());
            cx.notify();
        });
        draw(cx);
        assert!(!view.read_with(cx, |chat, _| chat.has_resident_markdown_state("early")));
        view.update(cx, |chat, cx| {
            chat.list_state.set_follow_mode(FollowMode::Tail);
            cx.notify();
        });
        draw(cx);

        let row = view.read_with(cx, |chat, cx| chat.markdown_row("early", cx).unwrap());
        assert_eq!(row, 101);
        assert!(
            cx.debug_bounds("timeline-row-101").is_some(),
            "the message is on screen"
        );
        assert!(view.read_with(cx, |chat, _| chat.has_resident_markdown_state("early")));
    }

    #[gpui::test]
    fn manually_collapsed_activity_stays_closed_while_new_activity_opens(cx: &mut TestAppContext) {
        use gpui::{px, size};

        let command = ItemContent::CommandExecution {
            command: "echo hello".into(),
            output: "hello\n".into(),
            exit_code: Some(0),
            status: ItemStatus::Completed,
        };
        let file_edit = ItemContent::FileChange {
            changes: vec![agent::FileChange {
                path: "src/lib.rs".into(),
                kind: agent::FileChangeKind::Modify,
                diff: Some("@@ -1 +1 @@\n-fn old() {}\n+fn new() {}\n".into()),
            }],
            status: ItemStatus::Completed,
        };
        for (item, selector, detail_selector) in [
            (command, "activity-row-first", "activity-detail"),
            (file_edit, "file-edit-row", "file-edit-diff"),
        ] {
            let mut timeline = Timeline::default();
            // Render a live turn without starting the perpetual timeline ticker.
            timeline.turns = vec![TurnMeta {
                running: true,
                ..TurnMeta::default()
            }];
            timeline.entries = vec![
                entry("user", user_item("go")),
                entry("first", EntryContent::Item(item.clone())),
            ];
            let (store, window_state, session_id) = seed_chat(cx, timeline.clone());
            let (_, cx) = cx.add_window_view(|window, cx| {
                ChatView::new(store.clone(), window_state, window, cx)
            });
            cx.simulate_resize(size(px(1_024.), px(900.)));
            draw(cx);

            assert!(cx.debug_bounds(detail_selector).is_some());
            let header = cx.debug_bounds(selector).expect("activity toggle");
            cx.simulate_click(header.center(), gpui::Modifiers::default());
            draw(cx);
            assert!(
                cx.debug_bounds(detail_selector).is_none(),
                "{selector} did not close"
            );

            let mut updated = item.clone();
            match &mut updated {
                ItemContent::CommandExecution { output, .. } => output.push_str("more output\n"),
                ItemContent::FileChange { changes, .. } => {
                    changes[0]
                        .diff
                        .as_mut()
                        .unwrap()
                        .push_str("+fn another() {}\n");
                }
                _ => unreachable!(),
            }
            timeline.entries[1] = entry("first", EntryContent::Item(updated));
            store.update(cx, |store, cx| {
                store.set_session_replica_for_test(session_id.clone(), timeline.clone(), cx);
                cx.notify();
            });
            draw(cx);
            assert!(
                cx.debug_bounds(detail_selector).is_none(),
                "{selector} reopened on an activity update"
            );

            let header = cx.debug_bounds(selector).unwrap();
            cx.simulate_click(header.center(), gpui::Modifiers::default());
            draw(cx);
            assert!(cx.debug_bounds(detail_selector).is_some());
            let header = cx.debug_bounds(selector).unwrap();
            cx.simulate_click(header.center(), gpui::Modifiers::default());
            draw(cx);
            assert!(cx.debug_bounds(detail_selector).is_none());

            timeline
                .entries
                .push(entry("next", EntryContent::Item(item)));
            store.update(cx, |store, cx| {
                store.set_session_replica_for_test(session_id.clone(), timeline.clone(), cx);
                cx.notify();
            });
            draw(cx);
            assert!(
                cx.debug_bounds(detail_selector).is_some(),
                "the next {selector} did not open automatically"
            );
        }
    }

    /// A running command's panel shows the screens the host renders at the
    /// panel's width, pushed as the output grows, over one subscription per
    /// width that collapsing the panel or leaving the thread releases.
    #[gpui::test]
    fn a_running_command_shows_the_screens_the_host_pushes_for_its_width(cx: &mut TestAppContext) {
        use crate::terminal_drawer::TERMINAL_CELL_HEIGHT;
        use gpui::{VisualTestContext, px, size};
        use tcode_protocol::{Topic, terminal::TerminalColor};

        let (host, session_id, _) = seeded_chat_host(cx, Timeline::default(), false);
        let seeded_id = session_id.clone();
        let other_id = smol::block_on(host.update_state_for_test(move |state, cx| {
            state.residents.live.get_mut(&seeded_id).unwrap().timeline = Timeline::default();
            for (ts, event) in [
                agent::AgentEvent::TurnStarted {
                    turn_id: "turn".into(),
                },
                agent::AgentEvent::ItemCompleted(agent::ThreadItem {
                    id: "user".into(),
                    parent_item_id: None,
                    content: ItemContent::UserMessage {
                        text: "build it".into(),
                        context_len: None,
                        attachments: Vec::new(),
                    },
                }),
                agent::AgentEvent::ItemStarted(agent::ThreadItem {
                    id: "run".into(),
                    parent_item_id: None,
                    content: ItemContent::CommandExecution {
                        command: "make".into(),
                        output: String::new(),
                        exit_code: None,
                        status: ItemStatus::InProgress,
                    },
                }),
            ]
            .iter()
            .enumerate()
            {
                state.record_event_for_replica_test(&seeded_id, ts as u64, event, cx);
            }
            let other = state.start_draft("other-thread".into(), std::env::temp_dir(), cx);
            let active = state.residents.live.get_mut(&other).expect("second draft");
            active.draft = false;
            let meta = active.meta.clone();
            state.sessions.push(meta);
            other
        }))
        .expect("record the running command");
        let mut driver = crate::store::tests::LocalHostDriver::new(&host);
        let store = cx.new(|cx| {
            WorkspaceStore::new_attached(
                driver.link.clone(),
                crate::store::WorkspaceAttachment::Local,
                None,
                None,
                false,
                cx,
            )
        });
        driver.wait_until(cx, &store, "workspace baseline", |cx| {
            store.read_with(cx, |store, _| store.baseline_ready())
        });
        store.update(cx, |store, _| store.select_session(session_id.clone()));
        driver.wait_until(cx, &store, "chat baseline", |cx| {
            store.read_with(cx, |store, _| !store.chat_loading())
        });
        let window_state = cx.new(|_| WindowState::new(false));
        let (_, cx) =
            cx.add_window_view(|window, cx| ChatView::new(store.clone(), window_state, window, cx));
        let cx: &mut VisualTestContext = cx;
        cx.simulate_resize(size(px(1_024.), px(900.)));

        let link = driver.link.clone();
        let output_topics = || {
            link.subscribed_topics()
                .into_iter()
                .filter_map(|topic| match topic {
                    Topic::CommandOutput {
                        session_id,
                        item_id,
                        cols,
                    } => Some((session_id, item_id, cols)),
                    _ => None,
                })
                .collect::<Vec<_>>()
        };
        let watched = |cx: &mut VisualTestContext| {
            store.read_with(cx, |store, _| {
                store
                    .command_output("run")
                    .and_then(|output| output.watched())
            })
        };
        let frame = |cx: &mut VisualTestContext| {
            store.read_with(cx, |store, _| {
                store
                    .command_output("run")
                    .and_then(|output| output.frame().cloned())
            })
        };
        let mut settle = |cx: &mut VisualTestContext,
                          description: &str,
                          ready: &dyn Fn(&mut VisualTestContext) -> bool| {
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                driver.step(cx, &store);
                draw(cx);
                if ready(cx) {
                    return;
                }
                assert!(Instant::now() < deadline, "waiting for {description}");
                std::thread::sleep(Duration::from_millis(1));
            }
        };
        // Nothing drives this turn on the host, so its Work Log reads as
        // finished and folded: open it and the command, as a reader would.
        settle(cx, "the Work Log", &|cx| {
            cx.debug_bounds("worklog-header-0-run").is_some()
        });
        let header = cx.debug_bounds("worklog-header-0-run").unwrap();
        cx.simulate_click(header.center(), gpui::Modifiers::default());
        settle(cx, "the command row", &|cx| {
            cx.debug_bounds("activity-row-run").is_some()
        });
        if cx.debug_bounds("activity-detail").is_none() {
            let row = cx.debug_bounds("activity-row-run").unwrap();
            cx.simulate_click(row.center(), gpui::Modifiers::default());
        }
        settle(cx, "the panel to measure its width", &|cx| {
            watched(cx).is_some()
        });
        let cols = watched(cx).unwrap();
        let subscribed = vec![(session_id.clone(), "run".to_string(), cols)];
        assert_eq!(output_topics(), subscribed);

        let mut long_line = "x".repeat(usize::from(cols));
        long_line.push_str("yz");
        let growth = [
            "10%\r".to_string(),
            "50%\r".into(),
            "100%\n".into(),
            format!("{long_line}\n"),
            "\u{1b}[32mok\u{1b}[0m\n".into(),
        ];
        for (index, text) in growth.into_iter().enumerate() {
            let seeded_id = session_id.clone();
            smol::block_on(host.update_state_for_test(move |state, cx| {
                state.record_event_for_replica_test(
                    &seeded_id,
                    10 + index as u64,
                    &agent::AgentEvent::Delta {
                        item_id: "run".into(),
                        kind: agent::DeltaKind::CommandOutput,
                        text,
                    },
                    cx,
                );
            }))
            .expect("grow the command output");
            settle(cx, "one host turn", &|_| true);
            assert_eq!(output_topics(), subscribed, "growth re-subscribed");
        }
        settle(cx, "the screen after the last growth", &|cx| {
            frame(cx).is_some_and(|frame| frame.rows == 4)
        });
        let screen = frame(cx).unwrap();
        let text = |row: usize| {
            screen.visible[row]
                .cells
                .iter()
                .map(|cell| cell.text.as_str())
                .collect::<String>()
        };
        assert_eq!(screen.cols, cols);
        assert_eq!(text(0), "100%", "the progress bar was not redrawn in place");
        assert_eq!(text(1), "x".repeat(usize::from(cols)));
        assert_eq!(text(2), "yz");
        assert_eq!(text(3), "ok");
        assert_eq!(
            screen.style(&screen.visible[3].cells[0]).fg,
            TerminalColor::Indexed(2)
        );
        assert_eq!(output_topics(), subscribed);
        // The panel is as tall as the host's screen, not as the raw output's
        // three lines.
        let output = cx
            .debug_bounds("command-output-run")
            .expect("the output grid");
        assert_eq!(output.size.height, px(4. * TERMINAL_CELL_HEIGHT));

        cx.simulate_resize(size(px(600.), px(900.)));
        settle(cx, "the narrower screen", &|cx| {
            watched(cx).is_some_and(|watched| watched < cols)
                && frame(cx).is_some_and(|frame| Some(frame.cols) == watched(cx))
        });
        let narrower = watched(cx).unwrap();
        assert_eq!(
            output_topics(),
            vec![(session_id.clone(), "run".to_string(), narrower)]
        );

        let header = cx.debug_bounds("activity-row-run").expect("command row");
        cx.simulate_click(header.center(), gpui::Modifiers::default());
        settle(cx, "the collapsed panel to release its screen", &|_| {
            output_topics().is_empty()
        });
        let header = cx.debug_bounds("activity-row-run").expect("command row");
        cx.simulate_click(header.center(), gpui::Modifiers::default());
        settle(cx, "the reopened panel", &|_| output_topics().len() == 1);

        store.update(cx, |store, _| store.select_session(other_id));
        settle(cx, "leaving the thread", &|_| output_topics().is_empty());
        driver.shutdown(cx, &store, &host);
    }

    /// A `tcode_control` anchor opens its thread at the turn of the entry it
    /// names: here the user message of a turn above the window the thread
    /// opens with, in a log whose streamed replies the host merges on the
    /// wire, so no held record index stands for a log position.
    #[gpui::test]
    fn an_anchor_link_opens_its_thread_at_the_anchored_turn(cx: &mut TestAppContext) {
        use gpui::{VisualTestContext, px, size};

        const TURNS: u64 = 12;
        const DELTAS: u64 = 40;
        let (host, session_id, _) = seeded_chat_host(cx, Timeline::default(), false);
        let seeded_id = session_id.clone();
        // Turn `t` takes the rows from `t * (DELTAS + 5)`; its user message is
        // the second, and the position a `tcode_control` anchor names it by.
        let rows = smol::block_on(host.update_state_for_test(move |state, cx| {
            let mut events = Vec::new();
            for turn in 0..TURNS {
                let answer = format!("answer-{turn}");
                events.push(agent::AgentEvent::TurnStarted {
                    turn_id: turn.to_string(),
                });
                events.push(agent::AgentEvent::ItemCompleted(agent::ThreadItem {
                    id: format!("user-{turn}"),
                    parent_item_id: None,
                    content: ItemContent::UserMessage {
                        text: format!("Question {turn}"),
                        context_len: None,
                        attachments: Vec::new(),
                    },
                }));
                events.push(agent::AgentEvent::ItemStarted(agent::ThreadItem {
                    id: answer.clone(),
                    parent_item_id: None,
                    content: ItemContent::AssistantMessage {
                        text: String::new(),
                    },
                }));
                events.extend((0..DELTAS).map(|_| agent::AgentEvent::Delta {
                    item_id: answer.clone(),
                    kind: agent::DeltaKind::AssistantText,
                    text: "word ".into(),
                }));
                events.push(agent::AgentEvent::ItemCompleted(agent::ThreadItem {
                    id: answer,
                    parent_item_id: None,
                    content: ItemContent::AssistantMessage {
                        text: "word ".repeat(DELTAS as usize),
                    },
                }));
                events.push(agent::AgentEvent::TurnCompleted {
                    turn_id: turn.to_string(),
                    status: agent::TurnStatus::Completed,
                    usage: None,
                });
            }
            for (ts, event) in events.iter().enumerate() {
                state.record_event_for_replica_test(&seeded_id, ts as u64, event, cx);
            }
            events.len() as u64
        }))
        .expect("record the streamed turns");
        let mut driver = crate::store::tests::LocalHostDriver::new(&host);
        let store = cx.new(|cx| {
            WorkspaceStore::new_attached(
                driver.link.clone(),
                crate::store::WorkspaceAttachment::Local,
                None,
                None,
                false,
                cx,
            )
        });
        driver.wait_until(cx, &store, "workspace baseline", |cx| {
            store.read_with(cx, |store, _| store.baseline_ready())
        });
        let link = driver.link.clone();
        let page = driver.await_with_events(
            cx,
            &store,
            link.query(tcode_protocol::Query::SessionHistoryPage {
                session_id: session_id.clone(),
                before: u64::MAX,
                limit: 1,
            }),
        );
        assert!(
            matches!(
                page,
                Ok(tcode_protocol::QueryResponse::SessionHistoryPage { end, .. }) if end == rows
            ),
            "the recorded events are the log's only rows: {page:?}"
        );
        let window_state = cx.new(|_| WindowState::new(false));
        let (_, cx) =
            cx.add_window_view(|window, cx| ChatView::new(store.clone(), window_state, window, cx));
        let cx: &mut VisualTestContext = cx;
        cx.simulate_resize(size(px(1_024.), px(700.)));

        let opened = |cx: &mut VisualTestContext| {
            let view = cx.update(|window, _| window.root::<ChatView>().flatten())?;
            let (row, highlighted) = view.read_with(cx, |chat, _| {
                let row = chat.list_state.logical_scroll_top().item_ix;
                (
                    chat.rows.get(row).map(|row| row.turn),
                    chat.highlighted_turn,
                )
            });
            let turn = row.filter(|turn| highlighted == Some(*turn))?;
            store
                .read_with(cx, |store, _| {
                    store.with_active_timeline(|timeline| {
                        timeline
                            .entries
                            .iter()
                            .find(|entry| {
                                entry.turn == turn
                                    && matches!(
                                        entry.content,
                                        EntryContent::Item(ItemContent::UserMessage { .. })
                                    )
                            })
                            .map(|entry| entry.id.clone())
                    })
                })
                .flatten()
        };
        // Turn 1 is above the opening window; turn 9 is inside it, in the
        // thread already open.
        for turn in [1, 9] {
            let expected = format!("user-{turn}");
            store.update(cx, |store, cx| {
                let position = turn * (DELTAS + 5) + 1;
                store.select_session_at_position(session_id.clone(), position, cx);
            });
            let deadline = Instant::now() + Duration::from_secs(5);
            while opened(cx).as_deref() != Some(expected.as_str()) {
                assert!(Instant::now() < deadline, "waiting for {expected}");
                driver.step(cx, &store);
                draw(cx);
                std::thread::sleep(Duration::from_millis(1));
            }
        }
        driver.shutdown(cx, &store, &host);
    }

    #[test]
    fn pending_collapse_is_scoped_to_its_session() {
        let first_seen = Instant::now();
        for elapsed in [100, 500] {
            let mut expansions = AutoActivityExpansions::default();
            expansions.observe("session-a", "activity-command", true, Latest, first_seen);
            let superseded = expansions.observe(
                "session-a",
                "activity-command",
                true,
                ImmediatelySuperseded,
                first_seen + Duration::from_millis(elapsed),
            );
            assert_eq!(superseded.expanded, elapsed < 500);
            assert_eq!(superseded.collapse.is_some(), elapsed < 500);
            let other = expansions.observe(
                "session-b",
                "activity-command",
                true,
                Latest,
                first_seen + Duration::from_millis(600),
            );
            assert!(other.expanded);
            if let Some((generation, _)) = superseded.collapse {
                assert!(expansions.finish_collapse("session-a", "activity-command", generation));
            }
            let other = expansions.observe(
                "session-b",
                "activity-command",
                true,
                Latest,
                first_seen + Duration::from_millis(700),
            );
            assert!(
                other.expanded,
                "session B after session A collapsed at {elapsed}ms"
            );
            assert_eq!(other.collapse, None);
            let revisited = expansions.observe(
                "session-a",
                "activity-command",
                true,
                ImmediatelySuperseded,
                first_seen + Duration::from_millis(700),
            );
            assert!(
                !revisited.expanded,
                "session A after returning at {elapsed}ms"
            );
            assert_eq!(revisited.collapse, None);
        }
    }

    #[test]
    fn reactivated_activity_cancels_its_stale_delayed_collapse() {
        let mut expansions = AutoActivityExpansions::default();
        let first_seen = Instant::now();
        expansions.observe(
            AUTO_ACTIVITY_TEST_SESSION,
            "activity-command",
            true,
            Latest,
            first_seen,
        );
        let (generation, _) = expansions
            .observe(
                AUTO_ACTIVITY_TEST_SESSION,
                "activity-command",
                true,
                ImmediatelySuperseded,
                first_seen + Duration::from_millis(100),
            )
            .collapse
            .expect("supersession should schedule a collapse");

        assert!(
            expansions
                .observe(
                    AUTO_ACTIVITY_TEST_SESSION,
                    "activity-command",
                    true,
                    Latest,
                    first_seen + Duration::from_millis(200),
                )
                .expanded
        );
        assert!(!expansions.finish_collapse(
            AUTO_ACTIVITY_TEST_SESSION,
            "activity-command",
            generation
        ));
        assert!(
            !expansions
                .observe(
                    AUTO_ACTIVITY_TEST_SESSION,
                    "activity-command",
                    true,
                    ImmediatelySuperseded,
                    first_seen + AUTO_ACTIVITY_MIN_VISIBILITY,
                )
                .expanded
        );

        assert!(
            !expansions
                .observe(
                    AUTO_ACTIVITY_TEST_SESSION,
                    "activity-command",
                    false,
                    Latest,
                    first_seen + AUTO_ACTIVITY_MIN_VISIBILITY,
                )
                .expanded
        );
    }

    #[test]
    fn residency_markdown_entries_are_bounded_by_candidate_windows() {
        let mut timeline = synthetic_markdown_timeline(200);
        timeline.turns[5].running = true;
        timeline.turn_running = true;
        // Three rows per synthetic turn: the user bubble, the reasoning
        // run and the assistant message.
        let rows = super::model::index_rows(
            &timeline.turns,
            &timeline.entries,
            &std::collections::HashSet::new(),
        );
        assert_eq!(rows.len(), 600);
        let scope = ResidencyScope::new(600, 120..144, None, true);

        let candidates = markdown_entries_for_residency(&timeline, &rows, &scope);

        // Rows 48..216 and the four tail rows hold 115 user and assistant
        // messages; a reasoning run holds no Markdown.
        assert_eq!(candidates.len(), 115);
        assert!(candidates.len() < timeline.entries.len());
        // A running turn far from the viewport is history like any other.
        assert!(!candidates.iter().any(|entry| entry.id == "assistant-5"));
        assert!(candidates.iter().any(|entry| entry.id == "assistant-199"));
    }

    #[gpui::test]
    fn compact_composer_keeps_effort_and_settings_visible(cx: &mut TestAppContext) {
        use gpui::px;
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        let mut timeline = synthetic_markdown_timeline(1);
        timeline.usage = Some(agent::TokenUsage {
            used_tokens: Some(100_000),
            context_window: Some(200_000),
            ..Default::default()
        });
        // Seed a catalog-backed model: the generic chat fixture has no model
        // options and therefore cannot detect a missing effort control.
        cx.update(crate::theme::init);
        let data_root = crate::store::tests::scratch_root("effort-layout");
        let host = tcode_runtime::pipe::spawn_host(
            tcode_services::store::SessionStore::open_at(data_root).unwrap(),
            tcode_runtime::pipe::HostServices::default(),
        )
        .unwrap();
        let (session_id, timeline) = smol::block_on(host.update_state_for_test(|state, cx| {
            state.providers.model_catalogs.insert(
                agent::ProviderKind::Codex,
                vec![agent::ModelSpec {
                    id: "effort-layout".into(),
                    display_name: "A model with a long display name".into(),
                    is_default: true,
                    options: vec![agent::OptionDescriptor::Select {
                        id: "reasoningEffort".into(),
                        label: "Reasoning effort".into(),
                        options: vec![agent::SelectOption {
                            value: "xhigh".into(),
                            label: "Extra High".into(),
                            description: None,
                            unavailable: None,
                        }],
                        default_value: Some("xhigh".into()),
                        role: Default::default(),
                        apply: Default::default(),
                        recommended: None,
                        permissive: None,
                    }],
                }],
            );
            let id = state.start_draft("effort-layout".into(), std::env::temp_dir(), cx);
            let active = state.residents.live.get_mut(&id).unwrap();
            active.meta.provider = agent::ProviderKind::Codex;
            active.meta.model = Some("effort-layout".into());
            active.timeline = timeline;
            (id, active.timeline.clone())
        }))
        .unwrap();
        let mut driver = crate::store::tests::LocalHostDriver::new(&host);
        let store = cx.new(|cx| {
            WorkspaceStore::new_attached(
                driver.link.clone(),
                crate::store::WorkspaceAttachment::Local,
                None,
                None,
                false,
                cx,
            )
        });
        driver.wait_until(cx, &store, "workspace baseline", |cx| {
            store.read_with(cx, |store, _| {
                store.baseline_ready()
                    && store
                        .provider_model_catalog(agent::ProviderKind::Codex)
                        .iter()
                        .any(|model| model.id == "effort-layout")
            })
        });
        store.update(cx, |store, _| store.select_session(session_id.clone()));
        driver.wait_until(cx, &store, "selected conversation", |cx| {
            store.read_with(cx, |store, _| {
                store.baseline_ready() && !store.chat_loading()
            })
        });
        store.update(cx, |store, cx| {
            store.set_session_replica_for_test(session_id, timeline, cx);
        });
        let window_state = cx.new(|_| WindowState::new(false).with_compact(true));
        let (view, cx) =
            cx.add_window_view(|window, cx| ChatView::new(store.clone(), window_state, window, cx));
        for locale in ["en", "zh-CN"] {
            crate::set_locale(locale);
            view.update(cx, |_, cx| cx.notify());
            for (width, height) in [(360., 780.), (393., 852.)] {
                cx.simulate_resize(gpui::size(px(width), px(height)));
                cx.update(|window, cx| {
                    let _ = window.draw(cx);
                });
                let permission = cx.debug_bounds("permission-chip").expect("approval option");
                let effort = cx
                    .debug_bounds("traits-chip")
                    .expect("standalone effort option");
                let meter = cx
                    .debug_bounds("context-meter")
                    .expect("compact context meter");
                let model = cx.debug_bounds("model-picker").expect("model picker");
                let send = cx.debug_bounds("send-message").expect("send button");
                let card = cx.debug_bounds("composer-card").expect("input card");
                let drawer = cx
                    .debug_bounds("composer-settings-drawer")
                    .expect("attached settings drawer");
                assert_eq!(
                    send.top(),
                    model.top(),
                    "Send shares the model row at {width}"
                );
                assert_eq!(permission.top(), meter.top());
                assert!(permission.right() <= meter.left());
                assert_eq!(effort.top(), model.top(), "Effort sits on the model row");
                assert!(permission.left() >= px(0.) && meter.right() <= px(width));
                assert!(
                    model.right() <= effort.left() && effort.right() <= send.left(),
                    "controls overlap at {width}: model={model:?}, effort={effort:?}, send={send:?}"
                );
                assert!(send.right() <= card.right());
                assert!(effort.size.width >= px(44.) && effort.size.height >= px(44.));
                assert!(
                    drawer.top() >= card.bottom(),
                    "settings sit below the input card"
                );
                assert!(permission.left() >= drawer.left() && meter.right() <= drawer.right());
                assert!(meter.top() >= drawer.top() && meter.bottom() <= drawer.bottom());
                assert!(meter.size.width >= px(44.) && meter.size.height >= px(44.));
            }
        }
        driver.shutdown(cx, &store, &host);
    }

    #[gpui::test]
    fn timeline_scrollbar_drag_pauses_tail_following(cx: &mut TestAppContext) {
        use gpui::{
            Context, IntoElement, Modifiers, MouseButton, MouseDownEvent, MouseUpEvent, Render,
            Window, point, px,
        };

        struct ChatRoot(Entity<ChatView>);
        impl Render for ChatRoot {
            fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
                self.0.clone()
            }
        }

        // A phone and a tablet: the shell's layout rule, applied by hand since
        // no shell is mounted here.
        cx.update(|cx| crate::window_seam::override_mobile_for_test(cx, true));
        for (width, height) in [(393., 852.), (1024., 768.)] {
            let (store, window_state, _) = seed_chat(cx, synthetic_markdown_timeline(30));
            let (view, cx) = cx.add_window_view(|window, cx| {
                ChatRoot(cx.new(|cx| ChatView::new(store, window_state.clone(), window, cx)))
            });
            cx.simulate_resize(gpui::size(px(width), px(height)));
            cx.update(|window, cx| {
                let compact = crate::window_seam::window_is_compact(window, cx);
                window_state.update(cx, |state, _| state.compact = compact);
            });
            cx.run_until_parked();
            cx.update(|window, cx| {
                let _ = window.draw(cx);
            });
            let list = view.read_with(cx, |view, cx| view.0.read(cx).list_state.clone());
            let viewport = list.viewport_bounds();
            let tail = list.scroll_px_offset_for_scrollbar().y;
            assert!(list.is_following_tail());
            assert!(list.max_offset_for_scrollbar().y > viewport.size.height);

            let thumb = point(viewport.right() - px(5.), viewport.bottom() - px(8.));
            cx.simulate_mouse_move(thumb, None, Modifiers::default());
            cx.update(|window, cx| {
                let _ = window.draw(cx);
            });
            cx.simulate_event(MouseDownEvent {
                position: thumb,
                button: MouseButton::Left,
                ..Default::default()
            });
            let target = point(thumb.x, viewport.center().y);
            cx.simulate_mouse_move(target, Some(MouseButton::Left), Modifiers::default());
            cx.simulate_event(MouseUpEvent {
                position: target,
                button: MouseButton::Left,
                ..Default::default()
            });
            cx.update(|window, cx| {
                let _ = window.draw(cx);
            });

            assert!(
                list.scroll_px_offset_for_scrollbar().y > tail + viewport.size.height,
                "dragging the visible scrollbar must scroll the conversation at width {width}"
            );
            assert!(!list.is_following_tail());

            // A finger on the thumb drags the scrollbar too, instead of
            // panning the conversation underneath it.
            list.set_follow_mode(gpui::FollowMode::Tail);
            list.scroll_to_end();
            cx.update(|window, cx| {
                let _ = window.draw(cx);
            });
            assert!(list.is_following_tail());
            cx.simulate_mouse_move(thumb, None, Modifiers::default());
            cx.update(|window, cx| {
                let _ = window.draw(cx);
            });
            for (phase, position) in [
                (gpui::TouchPhase::Started, thumb),
                (gpui::TouchPhase::Moved, target),
                (gpui::TouchPhase::Ended, target),
            ] {
                cx.simulate_event(gpui::TouchDragEvent {
                    phase,
                    start_position: thumb,
                    position,
                });
            }
            cx.update(|window, cx| {
                let _ = window.draw(cx);
            });
            assert!(
                list.scroll_px_offset_for_scrollbar().y > tail + viewport.size.height,
                "touch-dragging the scrollbar must scroll the conversation at width {width}"
            );
            assert!(!list.is_following_tail());
        }
    }

    #[gpui::test]
    fn touch_pan_pauses_tail_following_and_shows_the_pill(cx: &mut TestAppContext) {
        use gpui::{
            Context, IntoElement, PlatformInput, Render, TouchEvent, TouchId, TouchPhase, Window,
            point, px,
        };
        struct TouchChat(Entity<ChatView>);
        impl Render for TouchChat {
            fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
                self.0.clone()
            }
        }
        let (store, window_state, _) = seed_chat(cx, synthetic_markdown_timeline(30));
        window_state.update(cx, |state, _| state.compact = true);
        let (root, cx) = cx.add_window_view(|window, cx| {
            TouchChat(cx.new(|cx| ChatView::new(store, window_state, window, cx)))
        });
        cx.simulate_resize(gpui::size(px(393.), px(852.)));
        cx.run_until_parked();
        cx.update(|window, cx| {
            let _ = window.draw(cx);
        });
        let list = root.read_with(cx, |root, cx| root.0.read(cx).list_state.clone());
        let viewport = list.viewport_bounds();
        let height = viewport.size.height;
        assert!(list.is_following_tail());
        assert!(cx.debug_bounds("scroll-to-end").is_none());
        let touch = |phase, y: gpui::Pixels, cx: &mut gpui::VisualTestContext| {
            cx.update(|window, cx| {
                window.dispatch_event(
                    PlatformInput::Touch(TouchEvent {
                        id: TouchId(1),
                        phase,
                        position: point(viewport.center().x, y),
                        predicted_position: None,
                        force: None,
                    }),
                    cx,
                );
            });
            draw(cx);
        };

        // The finger moves down: earlier content comes into view and following
        // pauses on the first step. Cancel avoids release momentum.
        let top = viewport.top() + px(10.);
        touch(TouchPhase::Started, top, cx);
        touch(TouchPhase::Moved, top + px(40.), cx);
        assert!(!list.is_following_tail(), "the first step pauses following");
        touch(TouchPhase::Moved, top + height * 3., cx);
        touch(TouchPhase::Cancelled, top + height * 3., cx);
        assert!(!list.is_following_tail());
        assert!(
            cx.debug_bounds("scroll-to-end").is_some(),
            "the pill follows the list geometry after a touch pan"
        );

        // Panning back to the end resumes following and hides the pill.
        let bottom = viewport.bottom() - px(10.);
        touch(TouchPhase::Started, bottom, cx);
        touch(TouchPhase::Moved, bottom - height * 4., cx);
        touch(TouchPhase::Cancelled, bottom - height * 4., cx);
        assert!(list.is_following_tail());
        assert!(cx.debug_bounds("scroll-to-end").is_none());
    }

    #[cfg(any(target_os = "macos", target_os = "ios", target_os = "android"))]
    #[gpui::test]
    fn timeline_bounces_at_its_edges_without_moving_the_list(cx: &mut TestAppContext) {
        use gpui::{
            Context, IntoElement, PlatformInput, Render, TouchEvent, TouchId, TouchPhase, Window,
            point, px,
        };
        struct TouchChat(Entity<ChatView>);
        impl Render for TouchChat {
            fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
                self.0.clone()
            }
        }
        let (store, window_state, _) = seed_chat(cx, synthetic_markdown_timeline(30));
        window_state.update(cx, |state, _| state.compact = true);
        let (root, cx) = cx.add_window_view(|window, cx| {
            TouchChat(cx.new(|cx| ChatView::new(store, window_state, window, cx)))
        });
        cx.simulate_resize(gpui::size(px(393.), px(852.)));
        cx.run_until_parked();
        cx.update(|window, cx| {
            let _ = window.draw(cx);
        });
        let list = root.read_with(cx, |root, cx| root.0.read(cx).list_state.clone());
        assert!(list.max_offset_for_scrollbar().y > px(0.));
        for at_end in [false, true] {
            settle_frames(cx);
            list.set_follow_mode(gpui::FollowMode::Normal);
            if at_end {
                list.scroll_to_end();
            } else {
                list.scroll_to(gpui::ListOffset {
                    item_ix: 0,
                    offset_in_item: px(0.),
                });
            }
            draw(cx);
            let resting = list.viewport_bounds();
            let offset = list.scroll_px_offset_for_scrollbar().y;
            let row_selector = if at_end {
                "timeline-row-89"
            } else {
                "timeline-row-0"
            };
            let row = cx.debug_bounds(row_selector).expect("edge row");
            let composer = cx.debug_bounds("chat-composer").expect("composer");
            let touch = |phase, y: gpui::Pixels, cx: &mut gpui::VisualTestContext| {
                cx.update(|window, cx| {
                    window.dispatch_event(
                        PlatformInput::Touch(TouchEvent {
                            id: TouchId(1),
                            phase,
                            position: point(resting.center().x, y),
                            predicted_position: None,
                            force: None,
                        }),
                        cx,
                    );
                    let _ = window.draw(cx);
                });
            };
            let direction = if at_end { -1. } else { 1. };
            let start = resting.center().y;
            touch(TouchPhase::Started, start, cx);
            touch(TouchPhase::Moved, start + px(200. * direction), cx);
            let stretched = list.viewport_bounds();
            assert!(
                (stretched.top() - resting.top()) * direction > px(0.),
                "an outward pan stretches the timeline: {stretched:?} vs {resting:?}"
            );
            assert_eq!(stretched.size, resting.size);
            assert_eq!(
                list.scroll_px_offset_for_scrollbar().y,
                offset,
                "the stretch is a displacement, not a scroll"
            );
            let stretched_row = cx.debug_bounds(row_selector).expect("edge row");
            assert_eq!(
                stretched_row.top() - row.top(),
                stretched.top() - resting.top()
            );
            assert_eq!(
                cx.debug_bounds("chat-composer").expect("composer"),
                composer,
                "fixed chrome stays outside the stretch"
            );
            touch(TouchPhase::Cancelled, start + px(200. * direction), cx);
            assert_eq!(list.scroll_px_offset_for_scrollbar().y, offset);
        }
    }

    #[gpui::test]
    fn a_drag_right_after_a_flick_scrolls_the_timeline(cx: &mut TestAppContext) {
        use gpui::{point, px};
        let (store, window_state, _) = seed_chat(cx, synthetic_markdown_timeline(30));
        let (view, cx) =
            cx.add_window_view(|window, cx| ChatView::new(store, window_state, window, cx));
        cx.simulate_resize(gpui::size(px(393.), px(852.)));
        draw(cx);
        let list = view.read_with(cx, |chat, _| chat.list_state.clone());
        settle_frames(cx);
        list.set_follow_mode(gpui::FollowMode::Normal);
        list.scroll_to(gpui::ListOffset {
            item_ix: 0,
            offset_in_item: px(0.),
        });
        draw(cx);
        let resting = list.viewport_bounds();
        // One trackpad packet per displayed frame.
        let packet = |phase, dy: f32, cx: &mut gpui::VisualTestContext| {
            cx.simulate_event(gpui::ScrollWheelEvent {
                position: resting.center(),
                delta: gpui::ScrollDelta::Pixels(point(px(0.), px(dy))),
                touch_phase: phase,
                ..Default::default()
            });
            cx.executor().advance_clock(Duration::from_millis(16));
            cx.run_until_parked();
            cx.update(|window, cx| window.simulate_next_frame(cx));
        };
        packet(gpui::TouchPhase::Started, 0., cx);
        for _ in 0..6 {
            packet(gpui::TouchPhase::Moved, -70., cx);
        }
        packet(gpui::TouchPhase::Ended, 0., cx);
        let flicked = list.scroll_px_offset_for_scrollbar().y;
        assert!(flicked < px(0.) && flicked > -list.max_offset_for_scrollbar().y);
        packet(gpui::TouchPhase::Started, 0., cx);
        for _ in 0..4 {
            packet(gpui::TouchPhase::Moved, -50., cx);
        }
        assert_eq!(
            list.scroll_px_offset_for_scrollbar().y,
            flicked - px(200.),
            "the drag scrolls the conversation"
        );
        assert_eq!(list.viewport_bounds(), resting, "nothing is displaced");
    }

    #[gpui::test]
    fn timeline_tail_has_one_composer_gap(cx: &mut TestAppContext) {
        use gpui::{Context, IntoElement, Render, Window, px};
        struct ChatRoot(Entity<ChatView>);
        impl Render for ChatRoot {
            fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
                self.0.clone()
            }
        }

        for (mobile, running) in [(false, false), (false, true), (true, false), (true, true)] {
            for (width, height) in [(393., 852.), (1024., 768.)] {
                cx.update(|cx| crate::window_seam::override_mobile_for_test(cx, mobile));
                let expected_gap = if mobile { 0. } else { super::TIMELINE_INSET };
                let mut timeline = synthetic_markdown_timeline(30);
                timeline.turn_running = running;
                timeline.turns.last_mut().unwrap().running = running;
                let (store, window_state, _) = seed_chat(cx, timeline);
                let (view, cx) = cx.add_window_view(|window, cx| {
                    ChatRoot(cx.new(|cx| ChatView::new(store, window_state.clone(), window, cx)))
                });
                cx.simulate_resize(gpui::size(px(width), px(height)));
                // The shell's layout rule, applied by hand since no shell is
                // mounted here: only the mobile phone is compact.
                cx.update(|window, cx| {
                    let compact = crate::window_seam::window_is_compact(window, cx);
                    window_state.update(cx, |state, _| state.compact = compact);
                });
                let list = view.read_with(cx, |view, cx| view.0.read(cx).list_state.clone());
                list.scroll_to_end();
                cx.run_until_parked();
                cx.update(|window, cx| {
                    let _ = window.draw(cx);
                });
                let last = cx
                    .debug_bounds("timeline-row-89")
                    .expect("last timeline row");
                assert!(
                    f32::from(last.bottom() - list.viewport_bounds().bottom()).abs() <= 1.,
                    "the gap must stay outside the list's viewport"
                );
                let composer = cx.debug_bounds("chat-composer").expect("composer");
                let gap = f32::from(composer.top() - last.bottom());
                assert!(
                    (gap - expected_gap).abs() <= 1.,
                    "{width}×{height}, running={running}, mobile={mobile}: expected {expected_gap}px gap, got {gap}px"
                );
                let card = cx
                    .debug_bounds("composer-card")
                    .expect("visible composer card");
                assert_eq!(card.top(), composer.top(), "no extra composer top inset");
                if running {
                    let status = cx
                        .debug_bounds("working-status-29")
                        .expect("running status");
                    // The row's own edge padding sits below the status.
                    assert_eq!(
                        status.bottom(),
                        last.bottom() - px(super::TIMELINE_EDGE_PADDING)
                    );
                }
            }
        }
    }

    #[gpui::test]
    fn compact_running_status_clears_composer_and_keyboard(cx: &mut TestAppContext) {
        use gpui::{Context, IntoElement, ParentElement, Render, Styled, Window, div, px};
        struct OccludedChat {
            chat: Entity<ChatView>,
            bottom: gpui::Pixels,
        }
        impl Render for OccludedChat {
            fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
                // Reserve the system-occluded area, as the shell's window seam does.
                div().size_full().pb(self.bottom).child(self.chat.clone())
            }
        }
        let mut timeline = synthetic_markdown_timeline(30);
        Arc::make_mut(timeline.entries.last_mut().unwrap()).content =
            assistant(&"A long running reply keeps its final status visible.\n\n".repeat(40));
        timeline.turn_running = true;
        timeline.turns.last_mut().unwrap().running = true;
        let (store, window_state, _) = seed_chat(cx, timeline);
        window_state.update(cx, |state, _| state.compact = true);
        let (root, cx) = cx.add_window_view(|window, cx| OccludedChat {
            chat: cx.new(|cx| ChatView::new(store, window_state, window, cx)),
            bottom: px(34.),
        });
        cx.simulate_resize(gpui::size(px(393.), px(852.)));
        let list = root.read_with(cx, |root, cx| root.chat.read(cx).list_state.clone());
        for bottom in [34., 320., 34.] {
            root.update(cx, |root, cx| {
                root.bottom = px(bottom);
                cx.notify();
            });
            for following in [true, false] {
                list.set_follow_mode(if following {
                    gpui::FollowMode::Tail
                } else {
                    gpui::FollowMode::Normal
                });
                list.scroll_to_end();
                cx.run_until_parked();
                cx.update(|window, cx| {
                    let _ = window.draw(cx);
                });
                let status = cx
                    .debug_bounds("working-status-29")
                    .expect("running status after scroll_to_end");
                let composer = cx.debug_bounds("chat-composer").expect("composer");
                assert!(status.bottom() <= composer.top());
                assert!(status.bottom() <= list.viewport_bounds().bottom());
                cx.simulate_event(gpui::ScrollWheelEvent {
                    position: list.viewport_bounds().center(),
                    delta: gpui::ScrollDelta::Pixels(gpui::point(px(0.), px(1200.))),
                    touch_phase: gpui::TouchPhase::Started,
                    ..Default::default()
                });
                cx.update(|window, cx| {
                    let _ = window.draw(cx);
                });
                cx.simulate_event(gpui::ScrollWheelEvent {
                    position: list.viewport_bounds().center(),
                    delta: gpui::ScrollDelta::Pixels(gpui::point(px(0.), px(-100000.))),
                    touch_phase: gpui::TouchPhase::Moved,
                    ..Default::default()
                });
                cx.update(|window, cx| {
                    let _ = window.draw(cx);
                });
                let status = cx
                    .debug_bounds("working-status-29")
                    .expect("running status row");
                let composer = cx.debug_bounds("chat-composer").expect("composer");
                let meter = cx
                    .debug_bounds("context-meter")
                    .expect("second composer row");
                let model = cx.debug_bounds("model-picker").expect("first composer row");
                assert!(meter.top() >= model.bottom());
                assert!(
                    status.bottom() <= composer.top(),
                    "status {status:?} overlaps composer {composer:?}, inset {bottom}, following {following}"
                );
                assert!(
                    status.bottom() <= list.viewport_bounds().bottom(),
                    "status is clipped by timeline"
                );
                assert!(status.top() >= list.viewport_bounds().top());
                assert!(composer.bottom() <= px(852. - bottom));
            }
        }
    }

    #[gpui::test]
    fn wheel_event_between_a_prepend_and_its_frame_keeps_the_reading_position(
        cx: &mut TestAppContext,
    ) {
        use gpui::{Context, IntoElement, Render, VisualTestContext, Window, point, px};

        struct TouchChat(Entity<ChatView>);
        impl Render for TouchChat {
            fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
                self.0.clone()
            }
        }
        let full = synthetic_markdown_timeline(120);
        let mut tail = full.clone();
        tail.turns.drain(..20);
        tail.entries.retain(|entry| entry.turn >= 20);
        for entry in &mut tail.entries {
            Arc::make_mut(entry).turn -= 20;
        }
        let (store, window_state, session_id) = seed_chat(cx, tail);
        let (root, cx) = cx.add_window_view(|window, cx| {
            TouchChat(cx.new(|cx| ChatView::new(store.clone(), window_state, window, cx)))
        });
        let view = root.read_with(cx, |root, _| root.0.clone());
        cx.simulate_resize(gpui::size(px(393.), px(852.)));
        draw(cx);
        let list = view.read_with(cx, |chat, _| chat.list_state.clone());
        let height = list.viewport_bounds().size.height;
        let scroll = |distance, cx: &mut VisualTestContext| {
            cx.simulate_event(gpui::ScrollWheelEvent {
                position: list.viewport_bounds().center(),
                delta: gpui::ScrollDelta::Pixels(point(px(0.), distance)),
                touch_phase: gpui::TouchPhase::Moved,
                ..Default::default()
            });
        };
        for _ in 0..12 {
            scroll(height / 4., cx);
            draw(cx);
        }
        assert!(!list.is_following_tail());
        let before = list.logical_scroll_top();
        let before_px = list.scroll_px_offset_for_scrollbar().y;

        // An earlier history page arrives from the host between two frames,
        // while a trackpad pan is still delivering packets. The test app
        // redraws dirty windows whenever effects flush, so the store update,
        // its observers and the packet share one flush: the deferred packet
        // runs after the observers and before the frame, as on a display link.
        let wheel = gpui::ScrollWheelEvent {
            position: list.viewport_bounds().center(),
            delta: gpui::ScrollDelta::Pixels(point(px(0.), px(40.))),
            touch_phase: gpui::TouchPhase::Moved,
            ..Default::default()
        };
        cx.update(|window, cx| {
            store.update(cx, |store, cx| {
                store.set_session_replica_for_test(session_id, full, cx);
                cx.notify();
            });
            window.defer(cx, move |window, cx| {
                window.dispatch_event(gpui::InputEvent::to_platform_input(wheel), cx);
            });
        });
        draw(cx);

        // The row the reader was on sits 60 rows lower and exactly 40px
        // further down the viewport: both the page and the pan applied.
        let row_top = list
            .bounds_for_item(before.item_ix + 60)
            .expect("the reader's row stays on screen")
            .top();
        assert_eq!(
            row_top - list.viewport_bounds().top(),
            px(40.) - before.offset_in_item,
            "a 40px pan and a 60-row page both apply (offset {:?} -> {:?})",
            before.offset_in_item,
            list.logical_scroll_top().offset_in_item
        );
        assert_eq!(list.scroll_px_offset_for_scrollbar().y, before_px + px(40.));
    }

    #[gpui::test]
    fn late_updates_preserve_user_scroll_intent(cx: &mut TestAppContext) {
        use gpui::{Context, IntoElement, Render, Window, px};
        struct TouchChat(Entity<ChatView>);
        impl Render for TouchChat {
            fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
                self.0.clone()
            }
        }
        for phase in [gpui::TouchPhase::Started, gpui::TouchPhase::Moved] {
            let mut timeline = synthetic_markdown_timeline(30);
            timeline.mark_idle();
            let (store, window_state, session_id) = seed_chat(cx, timeline.clone());
            let (root, cx) = cx.add_window_view(|window, cx| {
                TouchChat(cx.new(|cx| ChatView::new(store.clone(), window_state, window, cx)))
            });
            cx.simulate_resize(gpui::size(px(393.), px(852.)));
            draw(cx);
            let list = root.read_with(cx, |root, cx| root.0.read(cx).list_state.clone());
            let bottom = list.scroll_px_offset_for_scrollbar().y;
            let scroll = |delta, cx: &mut gpui::VisualTestContext| {
                cx.simulate_event(gpui::ScrollWheelEvent {
                    position: list.viewport_bounds().center(),
                    delta: gpui::ScrollDelta::Pixels(gpui::point(px(0.), delta)),
                    touch_phase: phase,
                    ..Default::default()
                });
            };
            scroll(px(120.), cx);
            draw(cx);
            assert_eq!(list.scroll_px_offset_for_scrollbar().y, bottom + px(120.));
            assert!(!list.is_following_tail());
            assert!(cx.debug_bounds("scroll-to-end").is_none());
            let anchor = list.logical_scroll_top();
            for update in 1..=30 {
                cx.executor().advance_clock(Duration::from_millis(100));
                Arc::make_mut(timeline.entries.last_mut().unwrap()).content =
                    assistant(&"Late output frame.\n\n".repeat(update));
                store.update(cx, |store, cx| {
                    store.set_session_replica_for_test(session_id.clone(), timeline.clone(), cx);
                    cx.notify();
                });
                draw(cx);
                assert!(!list.is_following_tail(), "late update {update}, {phase:?}");
                assert_eq!(list.logical_scroll_top().item_ix, anchor.item_ix);
                assert_eq!(
                    list.logical_scroll_top().offset_in_item,
                    anchor.offset_in_item
                );
            }
            scroll(px(-100000.), cx);
            draw(cx);
            assert!(
                list.is_following_tail(),
                "return to bottom {phase:?}: offset {:?}, max {:?}, anchor {:?}",
                list.scroll_px_offset_for_scrollbar(),
                list.max_offset_for_scrollbar(),
                list.logical_scroll_top()
            );
            store.update(cx, |store, cx| {
                store.set_session_replica_for_test(session_id, synthetic_markdown_timeline(31), cx);
                cx.notify();
            });
            draw(cx);
            assert!(list.is_following_tail());
            assert_eq!(list.logical_scroll_top().item_ix, list.item_count());
        }
    }

    #[gpui::test]
    fn scroll_pill_over_inline_image_does_not_open_lightbox(cx: &mut TestAppContext) {
        use gpui::{Modifiers, MouseButton, ScrollDelta, ScrollWheelEvent, TouchPhase, point, px};

        let path = std::env::temp_dir().join(format!(
            "tcode-pill-image-{}-{}.png",
            std::process::id(),
            NEXT_RESIDENCY_TEST_ID.fetch_add(1, Ordering::Relaxed)
        ));
        image::RgbaImage::from_pixel(400, 1000, image::Rgba([40, 80, 120, 255]))
            .save(&path)
            .unwrap();
        let mut timeline = synthetic_markdown_timeline(30);
        Arc::make_mut(timeline.entries.last_mut().unwrap()).content = assistant(&format!(
            "![shot]({})\n\n{}",
            path.display(),
            "Text after the screenshot.\n\n".repeat(80)
        ));
        let (host, session_id, timeline) = seeded_chat_host(cx, timeline, false);
        let seeded_id = session_id.clone();
        smol::block_on(host.update_state_for_test(move |state, cx| {
            state.residents.live.get_mut(&seeded_id).unwrap().timeline = Timeline::default();
            let mut ts = 0;
            for turn in 0..timeline.turns.len() {
                let mut events = vec![agent::AgentEvent::TurnStarted {
                    turn_id: turn.to_string(),
                }];
                events.extend(
                    timeline
                        .entries
                        .iter()
                        .filter(|entry| entry.turn == turn)
                        .map(|entry| {
                            let EntryContent::Item(content) = &entry.content else {
                                panic!("image fixture contains provider items");
                            };
                            agent::AgentEvent::ItemCompleted(agent::ThreadItem {
                                id: entry.id.clone(),
                                parent_item_id: None,
                                content: content.clone(),
                            })
                        }),
                );
                events.push(agent::AgentEvent::TurnCompleted {
                    turn_id: turn.to_string(),
                    status: agent::TurnStatus::Completed,
                    usage: None,
                });
                for event in events {
                    state.record_event_for_replica_test(&seeded_id, ts, &event, cx);
                    ts += 1;
                }
            }
        }))
        .expect("record inline image conversation");
        let mut driver = crate::store::tests::LocalHostDriver::new(&host);
        let store = cx.new(|cx| {
            WorkspaceStore::new_attached(
                driver.link.clone(),
                crate::store::WorkspaceAttachment::Local,
                None,
                None,
                false,
                cx,
            )
        });
        store.update(cx, |store, _| store.select_session(session_id.clone()));
        driver.wait_until(cx, &store, "chat baseline", |cx| {
            store.read_with(cx, |store, _| {
                store.baseline_ready() && !store.chat_loading()
            })
        });
        let window_state = cx.new(|_| WindowState::new(false));
        struct LoadedImageChat {
            chat: Entity<ChatView>,
            sources: Vec<gpui::ImageSource>,
            ready: std::rc::Rc<std::cell::Cell<bool>>,
        }
        impl gpui::Render for LoadedImageChat {
            fn render(
                &mut self,
                window: &mut gpui::Window,
                cx: &mut gpui::Context<Self>,
            ) -> impl gpui::IntoElement {
                let mut loaded = true;
                for source in &self.sources {
                    let gpui::ImageSource::Custom(load) = source else {
                        unreachable!("host images load through the query plane");
                    };
                    match load(window, cx) {
                        Some(Ok(_)) => {}
                        Some(Err(error)) => panic!("inline host image load failed: {error:?}"),
                        None => loaded = false,
                    }
                }
                self.ready.set(loaded);
                self.chat.clone()
            }
        }
        let ready = std::rc::Rc::new(std::cell::Cell::new(false));
        let image_sources = vec![crate::store::image_preview(
            path.clone(),
            crate::store::MAX_PREVIEW_PIXELS as f32,
            false,
        )];
        let (root, cx) = cx.add_window_view(|window, cx| {
            let chat = cx.new(|cx| ChatView::new(store.clone(), window_state, window, cx));
            let surface = cx.new(|_| LoadedImageChat {
                chat,
                sources: image_sources,
                ready: ready.clone(),
            });
            gpui_base::Root::new(surface, window, cx)
        });
        let view = root.read_with(cx, |root, cx| {
            root.view()
                .clone()
                .downcast::<LoadedImageChat>()
                .unwrap()
                .read(cx)
                .chat
                .clone()
        });
        cx.simulate_resize(gpui::size(px(393.), px(600.)));
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            driver.step(cx, &store);
            draw(cx);
            if ready.get() {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "inline host image finishes loading"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
        let list = view.read_with(cx, |chat, _| chat.list_state.clone());
        let row = cx.debug_bounds("timeline-row-89").unwrap_or_else(|| {
            panic!(
                "last assistant row: selected={:?}, list_items={}",
                store.read_with(cx, |store, _| store.active_session_id()),
                list.item_count()
            )
        });
        cx.simulate_event(ScrollWheelEvent {
            position: list.viewport_bounds().center(),
            delta: ScrollDelta::Pixels(point(px(0.), list.viewport_bounds().top() - row.top())),
            touch_phase: TouchPhase::Moved,
            ..Default::default()
        });
        draw(cx);
        let pill = cx
            .debug_bounds("scroll-to-end")
            .expect("pill above the image");
        let click = |position, cx: &mut gpui::VisualTestContext| {
            cx.simulate_mouse_move(position, None, Modifiers::default());
            cx.simulate_mouse_down(position, MouseButton::Left, Modifiers::default());
            cx.simulate_mouse_up(position, MouseButton::Left, Modifiers::default());
            draw(cx);
        };
        click(point(pill.center().x, pill.top() - px(40.)), cx);
        let deadline = Instant::now() + Duration::from_secs(5);
        while cx.debug_bounds("image-viewer-image").is_none() {
            driver.step(cx, &store);
            draw(cx);
            assert!(
                Instant::now() < deadline,
                "exposed image finishes its explicit read"
            );
            std::thread::yield_now();
        }
        assert!(
            cx.debug_bounds("image-viewer-image").is_some(),
            "exposed image opens the lightbox"
        );
        cx.simulate_keystrokes("escape");
        draw(cx);
        assert!(cx.debug_bounds("image-viewer-image").is_none());
        assert!(!list.is_following_tail());

        let pill = cx.debug_bounds("scroll-to-end").unwrap();
        click(pill.center(), cx);
        assert!(list.is_following_tail(), "pill click must reach the tail");
        assert!(
            cx.debug_bounds("image-viewer-image").is_none(),
            "pill click opened the image underneath"
        );
        driver.shutdown(cx, &store, &host);
        std::fs::remove_file(path).unwrap();
    }

    #[gpui::test]
    fn jump_to_latest_survives_unmeasured_history(cx: &mut TestAppContext) {
        use gpui::{Context, IntoElement, Render, VisualTestContext, Window, point, px};

        struct TouchChat(Entity<ChatView>);
        impl Render for TouchChat {
            fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
                self.0.clone()
            }
        }
        let full = synthetic_markdown_timeline(120);
        let mut tail = full.clone();
        tail.turns.drain(..20);
        tail.entries.retain(|entry| entry.turn >= 20);
        for entry in &mut tail.entries {
            Arc::make_mut(entry).turn -= 20;
        }
        assert_eq!(tail.entries.len(), 300);
        let (store, window_state, session_id) = seed_chat(cx, tail);
        let (root, cx) = cx.add_window_view(|window, cx| {
            TouchChat(cx.new(|cx| ChatView::new(store.clone(), window_state, window, cx)))
        });
        let view = root.read_with(cx, |root, _| root.0.clone());
        cx.simulate_resize(gpui::size(px(393.), px(852.)));
        draw(cx);
        let list = view.read_with(cx, |chat, _| chat.list_state.clone());
        let height = list.viewport_bounds().size.height;
        assert!(height > px(0.));
        assert!(cx.debug_bounds("scroll-to-end").is_none());
        let scroll = |distance, phase, cx: &mut VisualTestContext| {
            cx.simulate_event(gpui::ScrollWheelEvent {
                position: list.viewport_bounds().center(),
                delta: gpui::ScrollDelta::Pixels(point(px(0.), distance)),
                touch_phase: phase,
                ..Default::default()
            });
            draw(cx);
        };
        // Small wheel deltas must accumulate, rather than snapping back while
        // the viewport is still inside the one-screen follow threshold.
        for _ in 0..12 {
            scroll(height / 4., gpui::TouchPhase::Moved, cx);
        }
        assert!(
            cx.debug_bounds("scroll-to-end").is_some(),
            "jump to latest must appear three screens above the tail (position {:?}, end {:?})",
            list.logical_scroll_top(),
            list.is_scrolled_to_end()
        );
        assert!(!list.is_following_tail());
        // Reach a nonzero intra-row offset through the same real wheel path;
        // a row-boundary anchor would miss prepend regressions that reset it.
        scroll(
            list.logical_scroll_top().offset_in_item - px(7.),
            gpui::TouchPhase::Moved,
            cx,
        );
        let before = list.logical_scroll_top();
        assert!(before.item_ix > 0);
        assert_eq!(before.offset_in_item, px(7.));
        store.update(cx, |store, cx| {
            store.set_session_replica_for_test(session_id.clone(), full, cx);
            cx.notify();
        });
        draw(cx);
        assert!(
            cx.debug_bounds("scroll-to-end").is_some(),
            "prepend hides jump"
        );
        assert_eq!(list.logical_scroll_top().item_ix, before.item_ix + 60);
        assert_eq!(
            list.logical_scroll_top().offset_in_item,
            before.offset_in_item
        );

        let button = cx.debug_bounds("scroll-to-end").unwrap();
        cx.simulate_click(button.center(), gpui::Modifiers::default());
        draw(cx);
        assert!(cx.debug_bounds("scroll-to-end").is_none());
        assert!(list.is_following_tail());
        store.update(cx, |store, cx| {
            store.set_session_replica_for_test(
                session_id.clone(),
                synthetic_markdown_timeline(121),
                cx,
            );
            cx.notify();
        });
        draw(cx);
        assert!(cx.debug_bounds("scroll-to-end").is_none());
        assert!(list.is_following_tail());
        assert_eq!(
            list.logical_scroll_top().item_ix,
            list.item_count(),
            "new turn did not move the tail anchor"
        );

        // A touch pan reaches the list's own wheel handler. No store
        // notification drives this UI.
        scroll(height * 3., gpui::TouchPhase::Started, cx);
        assert!(
            cx.debug_bounds("scroll-to-end").is_some(),
            "a touch pan hides jump"
        );
        assert!(!list.is_following_tail());
        let before = list.logical_scroll_top();
        store.update(cx, |store, cx| {
            store.set_session_replica_for_test(
                session_id.clone(),
                synthetic_markdown_timeline(122),
                cx,
            );
            cx.notify();
        });
        draw(cx);
        assert!(cx.debug_bounds("scroll-to-end").is_some());
        assert_eq!(
            list.logical_scroll_top().item_ix,
            before.item_ix,
            "new turn steals reading position"
        );
        assert_eq!(
            list.logical_scroll_top().offset_in_item,
            before.offset_in_item
        );
        assert!(!list.is_following_tail());

        let mut streaming = synthetic_markdown_timeline(122);
        let tail = Arc::make_mut(streaming.entries.last_mut().unwrap());
        tail.content = assistant(&"Streaming paragraph.\n\n".repeat(20));
        store.update(cx, |store, cx| {
            store.set_session_replica_for_test(session_id.clone(), streaming, cx);
            cx.notify();
        });
        draw(cx);
        assert!(cx.debug_bounds("scroll-to-end").is_some());
        assert_eq!(list.logical_scroll_top().item_ix, before.item_ix);
        assert_eq!(
            list.logical_scroll_top().offset_in_item,
            before.offset_in_item
        );
        assert!(
            !list.is_following_tail(),
            "streaming steals reading position"
        );

        scroll(-height * 100., gpui::TouchPhase::Moved, cx);
        assert!(cx.debug_bounds("scroll-to-end").is_none());
        // Reaching the bottom resumes following, independently of pill visibility.
        assert!(list.is_following_tail());
        store.update(cx, |store, cx| {
            store.set_session_replica_for_test(session_id, synthetic_markdown_timeline(123), cx);
            cx.notify();
        });
        draw(cx);
        assert!(list.is_following_tail());
        assert!(cx.debug_bounds("scroll-to-end").is_none());
    }

    #[gpui::test]
    fn history_window_is_measured_on_open_while_following_tail(cx: &mut TestAppContext) {
        use gpui::px;
        for (turns, short) in [(1, true), (60, false)] {
            let (store, window_state, _) = seed_chat(cx, synthetic_markdown_timeline(turns));
            let (view, cx) =
                cx.add_window_view(|window, cx| ChatView::new(store, window_state, window, cx));
            cx.simulate_resize(gpui::size(px(393.), px(852.)));
            cx.update(|window, cx| {
                let _ = window.draw(cx);
            });
            let list = view.read_with(cx, |chat, _| chat.list_state.clone());
            assert!(list.is_following_tail(), "no scroll has happened on open");
            let screens = super::history_screens_covered(&list, px(0.)).expect("laid out viewport");
            assert_eq!(
                screens < 6.,
                short,
                "the initial window uses the scroll-ahead threshold"
            );
            list.set_follow_mode(gpui::FollowMode::Normal);
            list.scroll_to(gpui::ListOffset {
                item_ix: 0,
                offset_in_item: px(100.),
            });
            assert_eq!(
                super::history_screens_covered(&list, px(200.)),
                Some(0.),
                "reserved incoming space is not loaded history"
            );
        }
    }

    /// A scrolling screenshot moves the timeline tile by tile from wherever it
    /// was, learns how far it really moved once layout has settled, and leaves
    /// it where it was.
    #[gpui::test]
    fn scrolling_capture_moves_the_timeline_within_its_content_and_restores_it(
        cx: &mut TestAppContext,
    ) {
        use gpui::{FollowMode, ListOffset, px};
        let (store, window_state, _) = seed_chat(cx, synthetic_markdown_timeline(60));
        let (view, cx) =
            cx.add_window_view(|window, cx| ChatView::new(store, window_state, window, cx));
        cx.simulate_resize(gpui::size(px(393.), px(852.)));
        let draw = |cx: &mut gpui::VisualTestContext| {
            cx.update(|window, cx| {
                let _ = window.draw(cx);
            });
        };
        // The rows on screen are built and measured before a capture starts.
        for _ in 0..4 {
            draw(cx);
            cx.run_until_parked();
        }
        assert!(view.read_with(cx, |chat, _| chat.capture_settled()));
        let list = view.read_with(cx, |chat, _| chat.list_state.clone());
        assert!(list.is_following_tail());
        assert!(
            super::capture_scrolled(&list) > px(852.),
            "sixty turns scroll well past one screen"
        );
        // Rows measured for the first time move the list; the capturer scrolls
        // again after each frame until nothing moves.
        let settle = |offset: gpui::Pixels, cx: &mut gpui::VisualTestContext| {
            for _ in 0..8 {
                let scroll = view
                    .update(cx, |chat, cx| chat.capture_scroll(offset, cx))
                    .expect("a capture is in progress");
                if !scroll.moved {
                    return scroll.reached;
                }
                draw(cx);
            }
            panic!("the timeline kept moving");
        };

        // Following the tail: the capture starts at the bottom.
        assert!(view.update(cx, |chat, cx| chat.capture_begin(cx)));
        let viewport = view.read_with(cx, |chat, _| chat.capture_viewport());
        assert!(viewport.is_some_and(|bounds| bounds.size.height > px(0.)));
        assert!(
            !list.is_following_tail(),
            "layout must not snap back to the end"
        );
        assert_eq!(
            settle(px(0.), cx),
            px(0.),
            "the first tile is the screen itself"
        );
        assert_eq!(settle(px(-400.), cx), px(-400.));
        assert!(
            list.logical_scroll_top().item_ix < 176,
            "a screen above the last rows"
        );
        assert_eq!(
            settle(px(400.), cx),
            px(0.),
            "nothing lies below the end of the content"
        );
        let top = settle(px(-1.0e6), cx);
        assert!(
            top < px(-852.) && top > px(-1.0e6),
            "the tile above the first turn stops at the top: {top:?}"
        );
        assert_eq!(list.logical_scroll_top().item_ix, 0);
        view.update(cx, |chat, cx| chat.capture_end(cx));
        assert!(list.is_following_tail());
        assert_eq!(
            view.update(cx, |chat, cx| chat.capture_scroll(px(0.), cx)),
            None,
            "tiles after the end are refused"
        );

        // Reading in the middle: the anchor comes back exactly, still paused.
        list.set_follow_mode(FollowMode::Normal);
        let anchor = ListOffset {
            item_ix: 10,
            offset_in_item: px(7.),
        };
        list.scroll_to(anchor);
        view.update(cx, |_, cx| cx.notify());
        draw(cx);
        assert!(view.update(cx, |chat, cx| chat.capture_begin(cx)));
        assert_eq!(settle(px(0.), cx), px(0.));
        assert_eq!(settle(px(300.), cx), px(300.));
        assert!(list.logical_scroll_top().item_ix >= 10);
        view.update(cx, |chat, cx| chat.capture_end(cx));
        let restored = list.logical_scroll_top();
        assert_eq!(restored.item_ix, anchor.item_ix);
        assert_eq!(restored.offset_in_item, anchor.offset_in_item);
        assert!(!list.is_following_tail());
    }

    /// A long streamed conversation opens with one partial turn: the snapshot
    /// holds only its last records. The first page completes that turn and
    /// adds an earlier one; the reader's position must survive rather than
    /// resetting to the tail.
    #[gpui::test]
    fn completing_the_only_partial_turn_keeps_the_reading_anchor(cx: &mut TestAppContext) {
        use gpui::{FollowMode, ListOffset, px};
        let mut full = synthetic_markdown_timeline(2);
        // Tall enough that the reader can scroll into the loaded content.
        let long_answer = "A paragraph of the answer.\n\n".repeat(80);
        full.entries.pop();
        full.entries
            .push(at_turn(entry("assistant-1", assistant(&long_answer)), 1));
        let mut tail = Timeline::default();
        tail.turns = vec![TurnMeta::default()];
        tail.entries = vec![entry("assistant-1", assistant(&long_answer))];
        let (store, window_state, session_id, _driver) = seed_chat_with_history(cx, tail, true);
        let (view, cx) =
            cx.add_window_view(|window, cx| ChatView::new(store.clone(), window_state, window, cx));
        cx.simulate_resize(gpui::size(px(1024.), px(700.)));
        for _ in 0..2 {
            view.update(cx, |_, cx| cx.notify());
            cx.update(|window, cx| {
                let _ = window.draw(cx);
            });
        }
        let (list, placeholder, leading) = view.read_with(cx, |chat, _| {
            (
                chat.list_state.clone(),
                chat.history_placeholder_height,
                chat.leading_space(),
            )
        });
        assert!(placeholder > px(0.), "earlier history is still unloaded");
        list.set_follow_mode(FollowMode::Normal);
        list.scroll_to(ListOffset {
            item_ix: 0,
            offset_in_item: leading + px(20.),
        });
        view.update(cx, |_, cx| cx.notify());
        cx.update(|window, cx| {
            let _ = window.draw(cx);
        });
        store.update(cx, |store, cx| {
            store.set_session_replica_for_test(session_id, full, cx);
            cx.notify();
        });
        cx.update(|window, cx| {
            let _ = window.draw(cx);
        });
        let anchor = list.logical_scroll_top();
        // The page adds the earlier turn's three rows and the user bubble
        // above the answer.
        assert_eq!(
            (anchor.item_ix, anchor.offset_in_item),
            (5, px(20.)),
            "the completed turn keeps its reader at the same content offset"
        );
        assert!(
            !list.is_following_tail(),
            "the page must not reset the list to its tail"
        );
    }

    /// A reader who scrolls far up a long thread and comes back sees rows at
    /// every step. Once they have stayed at the tail, the pages fetched above
    /// it are released without moving the tail: the conversation is again the
    /// one the tail needed, nothing is fetched back while they stay, and the
    /// next scroll up asks for the page above that window.
    #[gpui::test]
    fn pages_read_far_up_are_released_once_the_reader_stays_at_the_tail(cx: &mut TestAppContext) {
        use gpui::{VisualTestContext, point, px};
        use tcode_core::session::StoredEvent;
        use tcode_protocol::{
            ClientPayload, EventEnvelope, HostMessage, Query, QueryResponse, ServerEvent, Topic,
        };
        use tcode_runtime::pipe::{HostServices, spawn_host};
        use tcode_services::store::SessionStore;

        cx.update(crate::theme::init);
        cx.update(crate::markdown::init);
        let data_root = std::env::temp_dir().join(format!(
            "tcode-chat-release-test-{}-{}",
            std::process::id(),
            NEXT_RESIDENCY_TEST_ID.fetch_add(1, Ordering::Relaxed)
        ));
        let host = spawn_host(
            SessionStore::open_at(data_root.clone()).expect("test session store"),
            HostServices::default(),
        )
        .expect("spawn test host");
        let mut status = smol::block_on(host.update_state_for_test(|state, cx| {
            let id = state.start_draft("long".into(), std::env::temp_dir(), cx);
            state.session_status_snapshot(&id).unwrap()
        }))
        .unwrap();
        host.shutdown_blocking().unwrap();
        let _ = std::fs::remove_dir_all(data_root);
        status.session_id = "long".into();
        status.draft = false;
        let mut meta = tcode_core::project::SessionMeta::new(
            agent::ProviderKind::Codex,
            std::env::temp_dir(),
            None,
        );
        meta.id = "long".into();

        // Turn `t` is the four records at `4 * t` of a 400-turn log.
        let turns = |range: std::ops::Range<u64>| -> Vec<StoredEvent> {
            range
                .flat_map(|turn| {
                    [
                        agent::AgentEvent::TurnStarted {
                            turn_id: turn.to_string(),
                        },
                        agent::AgentEvent::ItemCompleted(agent::ThreadItem {
                            id: format!("user-{turn}"),
                            parent_item_id: None,
                            content: ItemContent::UserMessage {
                                text: format!("Question {turn}"),
                                context_len: None,
                                attachments: vec![],
                            },
                        }),
                        agent::AgentEvent::ItemCompleted(agent::ThreadItem {
                            id: format!("answer-{turn}"),
                            parent_item_id: None,
                            content: ItemContent::AssistantMessage {
                                text: format!("Answer {turn}.\n\n").repeat(3),
                            },
                        }),
                        agent::AgentEvent::TurnCompleted {
                            turn_id: turn.to_string(),
                            status: agent::TurnStatus::Completed,
                            usage: None,
                        },
                    ]
                    .map(StoredEvent::from)
                })
                .collect()
        };

        let (to_host, outgoing) = async_channel::unbounded::<String>();
        let (incoming, from_host) = async_channel::unbounded::<String>();
        let link = tcode_client::HostLink::new(to_host, from_host);
        let executor = cx.background_executor.clone();
        let _pump = cx.background_executor.spawn({
            let link = link.clone();
            async move {
                link.pump_with_timer(|| executor.timer(Duration::from_millis(25)))
                    .await;
            }
        });
        let send = |message: HostMessage| {
            incoming
                .try_send(tcode_protocol::encode_line(&message).unwrap())
                .unwrap();
        };
        let event = |topic: Topic, event: ServerEvent| {
            send(HostMessage::Event(EventEnvelope {
                request_id: None,
                topic,
                event,
            }));
        };
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
        store.update(cx, |store, _| store.select_session("long".into()));
        event(
            Topic::Settings,
            ServerEvent::SettingsSnapshot(Default::default()),
        );
        event(
            Topic::Index,
            ServerEvent::IndexSnapshot(tcode_protocol::IndexSnapshot {
                summary: Default::default(),
                sessions: vec![meta],
                projects: vec![],
            }),
        );
        event(
            Topic::SessionStatus {
                session_id: "long".into(),
            },
            ServerEvent::SessionStatusReplaced(Box::new(status)),
        );
        event(
            Topic::SessionEvents {
                session_id: "long".into(),
            },
            ServerEvent::SessionSnapshot {
                from: 4 * 390,
                end: 4 * 400,
                records: turns(390..400),
                total: 4 * 400,
                total_turns: 400,
                truncated: false,
            },
        );
        let window_state = cx.new(|_| WindowState::new(false));
        let (view, cx) =
            cx.add_window_view(|window, cx| ChatView::new(store.clone(), window_state, window, cx));
        cx.simulate_resize(gpui::size(px(393.), px(852.)));

        // Present a frame, as the display link does, and answer the page it
        // asked for, ten turns above `before`; then let the store's single
        // request gate reopen.
        let frame = |cx: &mut VisualTestContext| -> Option<u64> {
            store.update(cx, |store, cx| store.drain_host_events_for_test(cx));
            draw(cx);
            cx.update(|window, cx| {
                window.simulate_next_frame(cx);
            });
            cx.run_until_parked();
            let pages: Vec<(u64, u64)> = std::iter::from_fn(|| outgoing.try_recv().ok())
                .map(|line| tcode_protocol::decode_client_line(&line).unwrap())
                .filter_map(|message| match message.payload {
                    ClientPayload::Query(Query::SessionHistoryPage { before, .. }) => {
                        Some((message.id, before))
                    }
                    _ => None,
                })
                .collect();
            assert!(pages.len() <= 1, "one page in flight: {pages:?}");
            let (id, before) = *pages.first()?;
            send(HostMessage::QueryResult {
                id,
                result: Ok(QueryResponse::SessionHistoryPage {
                    records: turns(before / 4 - 10..before / 4),
                    from: before - 40,
                    end: before,
                    truncated: false,
                }),
            });
            cx.run_until_parked();
            cx.executor().advance_clock(Duration::from_millis(250));
            Some(before)
        };
        let settle = |cx: &mut VisualTestContext| -> Vec<u64> {
            let pages: Vec<u64> = std::iter::from_fn(|| frame(cx)).take(40).collect();
            assert!(frame(cx).is_none(), "the prefetch settles");
            pages
        };
        let replica = |cx: &mut VisualTestContext| {
            store.read_with(cx, |store, _| store.with_active_timeline(Timeline::clone))
        };
        let list = view.read_with(cx, |chat, _| chat.list_state.clone());
        // One trackpad gesture.
        let scroll = |distance: gpui::Pixels, cx: &mut VisualTestContext| {
            let position = list.viewport_bounds().center();
            for (touch_phase, distance) in [
                (gpui::TouchPhase::Started, px(0.)),
                (gpui::TouchPhase::Moved, distance),
                (gpui::TouchPhase::Ended, px(0.)),
            ] {
                cx.simulate_event(gpui::ScrollWheelEvent {
                    position,
                    delta: gpui::ScrollDelta::Pixels(point(px(0.), distance)),
                    touch_phase,
                    ..Default::default()
                });
            }
        };

        let tail_pages = settle(cx);
        assert!(list.is_following_tail());
        assert!(!tail_pages.is_empty(), "ten turns do not fill six screens");
        let tail = replica(cx).expect("the tail window");
        let tail_from = tail_pages.last().unwrap() - 40;

        let height = list.viewport_bounds().size.height;
        let mut pages_away = 0;
        while pages_away < 6 {
            scroll(height * 3., cx);
            while frame(cx).is_some() {
                pages_away += 1;
                view.read_with(cx, |chat, cx| {
                    assert!(!chat.workspace_store.read(cx).chat_loading());
                    assert!(!chat.rows.is_empty(), "the conversation stays on screen");
                });
            }
            assert!(!list.is_following_tail());
        }
        let read = replica(cx).unwrap();
        assert!(read.turns.len() > tail.turns.len());

        let back = |cx: &mut VisualTestContext| {
            // The pill shows once a frame has seen the reader away.
            draw(cx);
            let pill = cx.debug_bounds("scroll-to-end").expect("jump to latest");
            cx.simulate_click(pill.center(), gpui::Modifiers::none());
            assert!(frame(cx).is_none());
            assert!(list.is_following_tail());
        };
        back(cx);
        cx.executor().advance_clock(Duration::from_secs(20));
        assert!(frame(cx).is_none());
        assert_eq!(
            replica(cx).unwrap(),
            read,
            "a glance at the tail keeps them"
        );
        scroll(height * 1.5, cx);
        assert!(frame(cx).is_none());
        assert!(!list.is_following_tail());
        cx.executor().advance_clock(Duration::from_secs(20));
        assert!(frame(cx).is_none());
        assert_eq!(replica(cx).unwrap(), read, "reading above keeps them");

        back(cx);
        let last_row = |cx: &mut VisualTestContext| {
            cx.debug_bounds(format!("timeline-row-{}", list.item_count() - 1).leak())
                .expect("the last row is on screen")
        };
        let rows = list.item_count();
        let bottom = last_row(cx).bottom();
        cx.executor().advance_clock(Duration::from_secs(30));
        assert!(frame(cx).is_none(), "the tail's own pages are still held");
        assert_eq!(replica(cx).unwrap(), tail);
        assert!(list.is_following_tail());
        assert!(list.item_count() < rows);
        assert_eq!(last_row(cx).bottom(), bottom, "the tail stays where it was");
        assert!(settle(cx).is_empty());

        scroll(height * 3., cx);
        assert_eq!(frame(cx), Some(tail_from), "the page above the tail window");

        // Left with that page held, the thread reopens from its cursor with
        // the window its tail needed.
        while outgoing.try_recv().is_ok() {}
        store.update(cx, |store, _| {
            store.select_session("other".into());
            store.select_session("long".into());
        });
        let events_topic = Topic::SessionEvents {
            session_id: "long".into(),
        };
        let mut resumed_id = None;
        let resumed: Vec<_> = std::iter::from_fn(|| outgoing.try_recv().ok())
            .filter_map(|line| {
                let message = tcode_protocol::decode_client_line(&line).unwrap();
                match message.payload {
                    ClientPayload::Subscribe(subscription)
                        if subscription.topic == events_topic =>
                    {
                        resumed_id = Some(message.id);
                        Some(subscription.after)
                    }
                    _ => None,
                }
            })
            .collect();
        assert_eq!(resumed, [Some(4 * 400)]);
        send(HostMessage::Event(EventEnvelope {
            request_id: resumed_id,
            topic: events_topic,
            event: ServerEvent::SessionSnapshot {
                from: 4 * 400,
                end: 4 * 400,
                records: vec![],
                total: 4 * 400,
                total_turns: 400,
                truncated: false,
            },
        }));
        cx.run_until_parked();
        store.update(cx, |store, cx| store.drain_host_events_for_test(cx));
        assert_eq!(replica(cx).unwrap(), tail);
    }

    #[gpui::test]
    fn pan_packet_after_a_page_lands_keeps_the_walk_back_into_it(cx: &mut TestAppContext) {
        use gpui::{FollowMode, ListOffset, point, px};
        for pan_delta in [0., 40.] {
            let full = synthetic_markdown_timeline(60);
            let mut tail = synthetic_markdown_timeline(60);
            tail.turns.drain(..20);
            tail.entries.retain(|entry| entry.turn >= 20);
            for entry in &mut tail.entries {
                std::sync::Arc::make_mut(entry).turn -= 20;
            }
            let (store, window_state, session_id, _driver) = seed_chat_with_history(cx, tail, true);
            let (view, cx) = cx.add_window_view(|window, cx| {
                ChatView::new(store.clone(), window_state, window, cx)
            });
            cx.simulate_resize(gpui::size(px(393.), px(852.)));
            for _ in 0..2 {
                view.update(cx, |_, cx| cx.notify());
                cx.update(|window, cx| {
                    let _ = window.draw(cx);
                });
            }
            let (list, placeholder, leading) = view.read_with(cx, |chat, _| {
                (
                    chat.list_state.clone(),
                    chat.history_placeholder_height,
                    chat.leading_space(),
                )
            });
            assert!(placeholder > px(100.));
            list.set_follow_mode(FollowMode::Normal);
            list.scroll_to(ListOffset {
                item_ix: 0,
                offset_in_item: leading - px(100.),
            });
            view.update(cx, |_, cx| cx.notify());
            cx.update(|window, cx| {
                let _ = window.draw(cx);
            });
            let content_top = list.bounds_for_item(0).unwrap().top() + leading;
            assert!(content_top > list.viewport_bounds().top());
            store.update(cx, |store, cx| {
                store.set_session_replica_for_test(session_id, full, cx);
                store.suppress_history_prefetch_for_test();
                cx.notify();
            });
            // The frame that lands the page measures it.
            cx.update(|window, cx| {
                let _ = window.draw(cx);
            });
            // The finger is still moving: a pan packet resolves against that
            // frame's anchor before the next frame walks back into the page.
            if pan_delta != 0. {
                cx.simulate_event(gpui::ScrollWheelEvent {
                    position: list.viewport_bounds().center(),
                    delta: gpui::ScrollDelta::Pixels(point(px(0.), px(pan_delta))),
                    touch_phase: gpui::TouchPhase::Moved,
                    ..Default::default()
                });
            }
            cx.update(|window, cx| {
                window.simulate_next_frame(cx);
                let _ = window.draw(cx);
            });
            let anchor = list.logical_scroll_top();
            assert!(
                anchor.item_ix < 60 && anchor.offset_in_item >= px(0.),
                "pan {pan_delta}: {anchor:?}"
            );
            let after = list
                .bounds_for_item(60)
                .expect("previous first turn remains on screen")
                .top();
            assert!(
                (after - (content_top + px(pan_delta))).abs() < px(1.),
                "the walk-back and the pan both apply: {content_top:?} + {pan_delta} -> {after:?}"
            );
            assert!(!list.is_following_tail());
        }
    }

    #[gpui::test]
    fn upward_scroll_moves_only_by_the_input_distance(cx: &mut TestAppContext) {
        use gpui::{ScrollDelta, ScrollWheelEvent, point, px, size};

        for width in [393., 1024.] {
            let mut timeline = synthetic_markdown_timeline(3);
            for entry in &mut timeline.entries {
                if let EntryContent::Item(ItemContent::AssistantMessage { text }) =
                    &mut Arc::make_mut(entry).content
                {
                    *text = (0..40)
                        .map(|n| {
                            format!(
                                "## Section {n}\n\nA paragraph with **bold** text and `code`.\n\n"
                            )
                        })
                        .collect();
                }
            }
            let (store, window_state, _) = seed_chat(cx, timeline);
            let (view, cx) =
                cx.add_window_view(|window, cx| ChatView::new(store, window_state, window, cx));
            cx.simulate_resize(size(px(width), px(700.)));
            cx.update(|window, cx| {
                let _ = window.draw(cx);
            });
            let list = view.read_with(cx, |chat, _| chat.list_state.clone());
            list.set_offset_from_scrollbar(point(
                px(0.),
                list.scroll_px_offset_for_scrollbar().y + px(40.),
            ));
            view.update(cx, |_, cx| cx.notify());
            cx.update(|window, cx| {
                let _ = window.draw(cx);
            });
            for step in 0..160 {
                let item = list.logical_scroll_top().item_ix;
                let before = list.bounds_for_item(item).unwrap().top();
                cx.simulate_event(ScrollWheelEvent {
                    position: list.viewport_bounds().center(),
                    delta: ScrollDelta::Pixels(point(px(0.), px(40.))),
                    touch_phase: gpui::TouchPhase::Moved,
                    ..Default::default()
                });
                cx.update(|window, cx| {
                    let _ = window.draw(cx);
                });
                let after = list
                    .bounds_for_item(item)
                    .expect("reading anchor stays visible")
                    .top();
                assert!(
                    (f32::from(after - before) - 40.).abs() < 1.,
                    "step {step}: upward input of 40px moved from {before:?} to {after:?}"
                );
            }
        }
    }

    #[gpui::test]
    fn chat_view_applies_markdown_residency_decisions(cx: &mut TestAppContext) {
        use gpui::{FollowMode, ListOffset, VisualTestContext, px, size};

        // The user bubble of turn 40: three rows per synthetic turn.
        const TARGET: usize = 120;
        let timeline = synthetic_markdown_timeline(240);
        let (workspace_store, window_state, _) = seed_chat(cx, timeline);
        let (view, cx) = cx
            .add_window_view(|window, cx| ChatView::new(workspace_store, window_state, window, cx));
        let cx: &mut VisualTestContext = cx;
        cx.simulate_resize(size(px(1_024.), px(700.)));
        cx.update(|window, cx| {
            let _ = window.draw(cx);
        });
        let list_state = view.read_with(cx, |chat, _| chat.list_state.clone());
        assert!(!view.read_with(cx, |chat, _| {
            chat.has_resident_markdown_state("assistant-40")
        }));

        list_state.set_follow_mode(FollowMode::Normal);
        list_state.scroll_to(ListOffset {
            item_ix: TARGET,
            offset_in_item: px(7.),
        });
        view.update(cx, |_, cx| cx.notify());
        cx.update(|window, cx| {
            let _ = window.draw(cx);
        });

        let (resident, old_rebuilt, distant_tail_evicted, composer_tail_resident) =
            view.read_with(cx, |chat, _| {
                (
                    chat.resident_markdown_state_count(),
                    ["user-40", "assistant-40"]
                        .iter()
                        .all(|id| chat.has_resident_markdown_state(id)),
                    !chat.has_resident_markdown_state("assistant-230"),
                    ["assistant-238", "assistant-239"]
                        .iter()
                        .all(|id| chat.has_resident_markdown_state(id)),
                )
            });
        assert!(old_rebuilt, "the old visible turn was not lazily rebuilt");
        assert!(
            distant_tail_evicted,
            "a tail state outside the hysteresis band remained resident"
        );
        assert!(
            composer_tail_resident,
            "the composer-adjacent tail was evicted while viewing old turns"
        );
        assert!(
            resident <= 96,
            "old-turn window retained {resident} MarkdownStates; expected at most 96"
        );
        // The messages of the 64 rows built around the target and at the
        // tail: a reasoning run holds no Markdown.
        assert_eq!(resident, 43);
        let scroll_top = list_state.logical_scroll_top();
        assert_eq!(scroll_top.item_ix, TARGET);
        assert_eq!(
            scroll_top.offset_in_item,
            px(7.),
            "rebuilding the scroll-top turn changed the list anchor"
        );
    }

    #[gpui::test]
    fn large_markdown_becomes_resident_asynchronously_and_remeasures_turn(cx: &mut TestAppContext) {
        for (id, text, asynchronous) in [
            ("small", "small **streaming** reply".to_string(), false),
            ("large", large_markdown("async content"), true),
        ] {
            assert_eq!(text.len() >= ASYNC_MARKDOWN_THRESHOLD_BYTES, asynchronous);
            let timeline = single_assistant_timeline(id, &text);
            let (workspace_store, window_state, _) = seed_chat(cx, timeline);
            let view = cx
                .add_window(|window, cx| ChatView::new(workspace_store, window_state, window, cx));
            let view = view.root(cx).expect("chat window should have a root");
            view.read_with(cx, |chat, _| {
                assert_eq!(chat.has_resident_markdown_state(id), !asynchronous, "{id}");
                assert_eq!(
                    chat.pending_md_builds.contains_key(id),
                    asynchronous,
                    "{id}"
                );
                if asynchronous {
                    assert!(!chat.markdown_remeasured_rows.contains(&0));
                } else {
                    assert_eq!(chat.resident_markdown_source(id), Some(text.as_str()));
                }
            });
            cx.run_until_parked();
            view.read_with(cx, |chat, cx| {
                assert!(chat.has_resident_markdown_state(id), "{id}");
                assert_eq!(chat.resident_markdown_source(id), Some(text.as_str()));
                assert!(!chat.pending_md_builds.contains_key(id));
                let rendered = chat
                    .md_states
                    .get(id)
                    .unwrap()
                    .state
                    .read(cx)
                    .rendered_text();
                assert!(rendered.contains(if asynchronous {
                    "async content"
                } else {
                    "small streaming reply"
                }));
                if asynchronous {
                    assert!(chat.markdown_remeasured_rows.contains(&0));
                }
            });
        }
    }

    #[gpui::test]
    fn in_flight_markdown_update_finishes_with_latest_text(cx: &mut TestAppContext) {
        let original = large_markdown("original");
        let latest = large_markdown("edited");
        let timeline = single_assistant_timeline("large", &original);
        let (workspace_store, window_state, session_id) = seed_chat(cx, timeline);
        let view = cx.add_window(|window, cx| {
            ChatView::new(workspace_store.clone(), window_state, window, cx)
        });
        let view = view.root(cx).expect("chat window should have a root");
        let generation = view.read_with(cx, |chat, _| {
            chat.pending_md_builds
                .get("large")
                .expect("large build should be pending")
                .generation
        });

        workspace_store.update(cx, |store, cx| {
            store.set_session_replica_for_test(
                session_id,
                single_assistant_timeline("large", &latest),
                cx,
            );
        });
        view.update(cx, |chat, cx| chat.sync_markdown_states(cx));
        view.read_with(cx, |chat, _| {
            assert_eq!(chat.pending_md_builds.len(), 1);
            assert_eq!(
                chat.pending_md_builds["large"].generation, generation,
                "a text update spawned a duplicate in-flight job"
            );
        });

        cx.run_until_parked();
        view.read_with(cx, |chat, _| {
            assert_eq!(
                chat.resident_markdown_source("large"),
                Some(latest.as_str())
            );
            assert!(chat.pending_md_builds.is_empty());
        });
    }

    #[gpui::test]
    fn session_switch_does_not_resurrect_in_flight_markdown(cx: &mut TestAppContext) {
        let text = large_markdown("stale session");
        let timeline = single_assistant_timeline("large", &text);
        let (workspace_store, window_state, _, mut driver) =
            seed_chat_with_history(cx, timeline, false);
        let view = cx.add_window(|window, cx| {
            ChatView::new(workspace_store.clone(), window_state, window, cx)
        });
        let view = view.root(cx).expect("chat window should have a root");
        assert!(view.read_with(cx, |chat, _| chat.pending_md_builds.contains_key("large")));

        workspace_store.update(cx, |store, _| {
            store.select_session("replacement-session".into());
        });
        view.update(cx, |chat, cx| chat.sync_markdown_states(cx));
        driver.wait_until(cx, &workspace_store, "replacement history snapshot", |cx| {
            workspace_store.read_with(cx, |store, _| store.with_active_timeline(|_| ()).is_some())
        });
        workspace_store.update(cx, |store, cx| {
            store.set_session_replica_for_test(
                "replacement-session".into(),
                Timeline::default(),
                cx,
            );
        });
        cx.run_until_parked();

        view.read_with(cx, |chat, _| {
            assert!(!chat.has_resident_markdown_state("large"));
            assert!(!chat.pending_md_builds.contains_key("large"));
        });
    }

    /// A single running turn holding hundreds of interim messages opens
    /// with the Markdown of the rows near the tail parsed, not the whole
    /// turn's.
    #[gpui::test]
    fn a_long_running_turn_parses_only_the_markdown_near_the_viewport(cx: &mut TestAppContext) {
        use gpui::px;
        let mut timeline = Timeline::default();
        timeline.turn_running = true;
        timeline.turns = vec![TurnMeta {
            running: true,
            ..Default::default()
        }];
        timeline.entries.push(entry("user", user_item("go")));
        for step in 0..150 {
            timeline.entries.push(command(&format!("cmd-{step}")));
            timeline
                .entries
                .push(entry(&format!("note-{step}"), assistant("interim note")));
        }
        let (store, window_state, _) = seed_chat(cx, timeline);
        let (view, cx) =
            cx.add_window_view(|window, cx| ChatView::new(store, window_state, window, cx));
        cx.simulate_resize(gpui::size(px(393.), px(852.)));
        cx.update(|window, cx| {
            let _ = window.draw(cx);
        });
        cx.run_until_parked();
        let (rows, resident) = view.read_with(cx, |chat, _| {
            (chat.rows.len(), chat.resident_markdown_state_count())
        });
        assert_eq!(rows, 1 + 2 * 150);
        assert!(
            resident < 60,
            "{resident} Markdown documents resident for a 150-note turn; the rows near the tail suffice"
        );
        assert!(view.read_with(cx, |chat, _| chat.has_resident_markdown_state("note-149")));
        assert!(!view.read_with(cx, |chat, _| chat.has_resident_markdown_state("note-0")));
        assert!(cx.debug_bounds("timeline-row-300").is_some());
    }

    #[gpui::test]
    fn long_markdown_paints_middle_blocks_when_scrolled_in_chat_outer_list(
        cx: &mut TestAppContext,
    ) {
        use gpui::{VisualTestContext, point, px, size};
        use tcode_runtime::pipe::{HostServices, spawn_host};
        use tcode_services::store::SessionStore;

        const DEMO_MARKDOWN: &str = r#"# H1

## H2

This paragraph has **bold**, *italic*, ~~strikethrough~~, and `inline code`.

```rust
fn main() {
    let language = "Rust";
    let message = format!("Hello from {language}!");
    println!("{message}");
}
```

```typescript
const count: number = 3;
interface Demo {
  title: string;
  enabled: boolean;
}
const demo: Demo = { title: "TypeScript", enabled: true };
```

```python
def greet(name: str) -> str:
    message = f"Hello, {name}!"
    return message

print(greet("Python"))
```

```go
package main
func main() {
    message := "Hello from Go"
    println(message)
}
```

```toml
[demo]
title = "TOML sample"
enabled = true
count = 3
```

```kotlin
fun main() {
    val language: String = "Kotlin"
    println("Hello from $language")
}
```

```text
xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx
```

| Left | Center | Right |
| :--- | :----: | ----: |
| alpha | beta | 100 |
| gamma | delta | 200 |

| Name | Status | Owner | Description | Count | Notes |
| :--- | :----: | :--- | :---------- | ----: | :---- |
| Short | Ready | UI | This deliberately long table cell contains about sixty characters total. | 12 | wraps |
| Longer component name | Pending | Visual QA | Brief | 3 | compact |

- [x] Render headings
- [ ] Inspect every pixel

1. Ordered parent
   - Unordered child
     1. Nested ordered child
2. Second ordered item

- Unordered parent
  - Nested bullet
    1. Ordered grandchild

> First line of the blockquote.
> Second line of the blockquote.

Visit [Example](https://example.com) or the bare URL https://tcode.dev for more.

---

This paragraph has a soft
line break, followed by a hard break.\
This begins after the hard break."#;

        cx.update(crate::theme::init);
        cx.update(crate::markdown::init);
        let data_root = crate::store::tests::scratch_root("markdown-outer-list-test");
        let store = SessionStore::open_at(data_root).expect("test session store");
        let host = spawn_host(store, HostServices::default()).expect("spawn test host");
        let (session_id, timeline) =
            smol::block_on(host.update_state_for_test(move |state, cx| {
                let id = state.start_draft("markdown-test".into(), std::env::temp_dir(), cx);
                let active = state.residents.live.get_mut(&id).expect("selected draft");
                active.timeline = Timeline::default();
                active.timeline.turns = vec![TurnMeta::default()];
                active.timeline.entries = vec![
                    entry("user", user_item("render a long document")),
                    entry("assistant", assistant(DEMO_MARKDOWN)),
                ];
                active.draft = false;
                (active.meta.id.clone(), active.timeline.clone())
            }))
            .expect("seed markdown host");
        let mut driver = crate::store::tests::LocalHostDriver::new(&host);
        let workspace_store = cx.new(|cx| {
            WorkspaceStore::new_attached(
                driver.link.clone(),
                crate::store::WorkspaceAttachment::Local,
                None,
                None,
                false,
                cx,
            )
        });
        driver.wait_until(cx, &workspace_store, "workspace baseline", |cx| {
            workspace_store.read_with(cx, |store, _| store.baseline_ready())
        });
        workspace_store.update(cx, |store, _| store.select_session(session_id.clone()));
        driver.wait_until(cx, &workspace_store, "selected conversation", |cx| {
            workspace_store.read_with(cx, |store, _| {
                store.baseline_ready() && !store.chat_loading()
            })
        });
        workspace_store.update(cx, |store, cx| {
            store.set_session_replica_for_test(session_id, timeline, cx);
        });
        let window_state = cx.new(|_| WindowState::new(false));

        let (view, cx) = cx.add_window_view(|window, cx| {
            ChatView::new(workspace_store.clone(), window_state, window, cx)
        });
        let cx: &mut VisualTestContext = cx;
        cx.simulate_resize(size(px(1_024.), px(700.)));
        cx.run_until_parked();
        cx.update(|window, cx| {
            let _ = window.draw(cx);
        });
        cx.run_until_parked();
        cx.update(|window, cx| {
            let _ = window.draw(cx);
        });

        let outer_list = view.read_with(cx, |chat, _| chat.list_state.clone());
        let max_scroll = outer_list.max_offset_for_scrollbar().y;
        assert!(
            max_scroll > px(800.),
            "long markdown did not contribute its full height to the chat list: {max_scroll:?}"
        );
        let scroll_step = px(40.);
        let steps = (f32::from(max_scroll / 2.) / f32::from(scroll_step)).ceil() as usize;
        for step in 0..steps {
            let distance = (scroll_step * (step + 1) as f32).min(max_scroll / 2.);
            outer_list.set_offset_from_scrollbar(point(px(0.), -(max_scroll - distance)));
            view.update(cx, |_, cx| cx.notify());
            cx.update(|window, cx| {
                let _ = window.draw(cx);
            });
        }

        let viewport = outer_list.viewport_bounds();
        let middle = cx.debug_bounds("markdown-block-6");
        assert!(
            middle.is_some_and(|bounds| bounds.intersects(&viewport)),
            "middle markdown block did not paint at the chat list's middle offset; bounds={middle:?}, max_scroll={max_scroll:?}, viewport={viewport:?}"
        );
        assert!(
            cx.debug_bounds("markdown-block-18").is_none(),
            "offscreen tail block was painted; block virtualization did not cull it"
        );
        driver.shutdown(cx, &workspace_store, &host);
    }

    /// Reasoning is shown as plain text, so a streaming reasoning run parses
    /// no Markdown: neither when it opens nor as its deltas arrive.
    #[gpui::test]
    fn a_streaming_reasoning_run_parses_no_markdown(cx: &mut TestAppContext) {
        use gpui::{VisualTestContext, px, size};
        let running = |thought: &str| {
            let mut timeline = Timeline::default();
            timeline.turn_running = true;
            timeline.turns = vec![TurnMeta {
                running: true,
                ..TurnMeta::default()
            }];
            timeline.entries.extend([
                entry("user", user_item("go")),
                entry("reasoning", reasoning(thought)),
            ]);
            timeline
        };
        let (store, window_state, session_id) = seed_chat(cx, running("Let me **look**"));
        let (view, cx) =
            cx.add_window_view(|window, cx| ChatView::new(store.clone(), window_state, window, cx));
        let cx: &mut VisualTestContext = cx;
        cx.simulate_resize(size(px(1_024.), px(700.)));
        draw(cx);
        for delta in ["Let me **look** at", "Let me **look** at the `code`"] {
            store.update(cx, |store, cx| {
                store.set_session_replica_for_test(session_id.clone(), running(delta), cx);
                cx.notify();
            });
            draw(cx);
        }
        view.read_with(cx, |chat, cx| {
            let row = chat
                .markdown_row("reasoning", cx)
                .expect("the reasoning row");
            assert!(!chat.has_resident_markdown_state("reasoning"));
            assert!(
                !chat.markdown_remeasured_rows.contains(&row),
                "the reasoning row was remeasured for Markdown"
            );
            assert!(chat.has_resident_markdown_state("user"));
        });
    }

    /// Unloaded history reserves a screen above the first turn from the first
    /// frame, so the timeline's extent is settled by it.
    #[gpui::test]
    fn the_history_reservation_is_settled_by_the_first_frame(cx: &mut TestAppContext) {
        use gpui::{VisualTestContext, px};
        let (store, window_state, _, _driver) =
            seed_chat_with_history(cx, synthetic_markdown_timeline(1), true);
        // Opening the window draws its first frame.
        let (view, cx) =
            cx.add_window_view(|window, cx| ChatView::new(store, window_state, window, cx));
        let cx: &mut VisualTestContext = cx;
        let list = view.read_with(cx, |chat, _| chat.list_state.clone());
        let first = list.max_offset_for_scrollbar().y;
        view.update(cx, |_, cx| cx.notify());
        cx.update(|window, cx| {
            let _ = window.draw(cx);
        });
        assert_eq!(list.max_offset_for_scrollbar().y, first);
        assert!(
            first > px(0.),
            "one short turn scrolls into the reservation above it"
        );
    }

    /// A thread with nothing new to show is drawn again from what it drew
    /// when the view around it renders again, as the shell does for its own
    /// reasons, and asks for no frame of its own.
    #[gpui::test]
    fn an_idle_chat_is_reused_when_the_view_around_it_renders(cx: &mut TestAppContext) {
        use gpui::{
            Context, IntoElement, ParentElement as _, Render, Styled as _, VisualTestContext,
            Window, div, px, size,
        };
        struct Around(Entity<ChatView>);
        impl Render for Around {
            fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
                div().size_full().child(self.0.clone())
            }
        }
        let (store, window_state, _) = seed_chat(cx, synthetic_markdown_timeline(6));
        let (around, cx) = cx.add_window_view(|window, cx| {
            Around(cx.new(|cx| ChatView::new(store, window_state, window, cx)))
        });
        let cx: &mut VisualTestContext = cx;
        cx.simulate_resize(size(px(1_024.), px(900.)));
        settle_frames(cx);

        for _ in 0..2 {
            cx.update(|window, _| window.reset_layout_stats());
            around.update(cx, |_, cx| cx.notify());
            cx.run_until_parked();
            assert_eq!(
                cx.update(|window, _| window.layout_stats().views_built),
                1,
                "the chat was built again with the view around it"
            );
            assert_eq!(
                cx.update(|window, cx| window.simulate_next_frame(cx)),
                0,
                "the idle chat asked for another frame"
            );
        }
    }

    /// Whether the header's split buttons carry their labels follows the width
    /// it is laid out at, from the frame a resize draws.
    #[gpui::test]
    fn header_labels_follow_the_width_of_the_frame(cx: &mut TestAppContext) {
        use gpui::{VisualTestContext, px, size};
        let (store, window_state, _) = seed_chat(cx, synthetic_markdown_timeline(1));
        let (_view, cx) =
            cx.add_window_view(|window, cx| ChatView::new(store, window_state, window, cx));
        let cx: &mut VisualTestContext = cx;
        draw(cx);
        // A resize draws its frame at the new size.
        let git_main = |width: f32, cx: &mut VisualTestContext| {
            cx.simulate_resize(size(px(width), px(700.)));
            cx.debug_bounds("git-main")
                .expect("the git button")
                .size
                .width
        };
        let labelled = git_main(1_024., cx);
        let narrow = git_main(600., cx);
        assert!(narrow < labelled, "a 600pt header kept its labels");
        assert_eq!(
            git_main(1_024., cx),
            labelled,
            "a 1024pt header lost its labels"
        );
    }

    fn entry(id: &str, content: EntryContent) -> Arc<TimelineEntry> {
        Arc::new(TimelineEntry {
            id: id.to_string(),
            content,
            ts: None,
            turn: 0,
        })
    }

    fn user_item(text: &str) -> EntryContent {
        EntryContent::Item(ItemContent::UserMessage {
            text: text.into(),
            context_len: None,
            attachments: Vec::new(),
        })
    }

    fn assistant(text: &str) -> EntryContent {
        EntryContent::Item(ItemContent::AssistantMessage { text: text.into() })
    }

    fn reasoning(text: &str) -> EntryContent {
        EntryContent::Item(ItemContent::Reasoning { text: text.into() })
    }

    fn command(id: &str) -> Arc<TimelineEntry> {
        entry(
            id,
            EntryContent::Item(ItemContent::CommandExecution {
                command: id.to_string(),
                output: String::new(),
                exit_code: Some(0),
                status: agent::ItemStatus::Completed,
            }),
        )
    }

    fn at_turn(mut entry: Arc<TimelineEntry>, turn: usize) -> Arc<TimelineEntry> {
        Arc::make_mut(&mut entry).turn = turn;
        entry
    }

    fn synthetic_markdown_timeline(turn_count: usize) -> Timeline {
        let mut timeline = Timeline::default();
        timeline.turns = vec![TurnMeta::default(); turn_count];
        for turn in 0..turn_count {
            timeline.entries.extend([
                at_turn(
                    entry(&format!("user-{turn}"), user_item(&format!("User {turn}"))),
                    turn,
                ),
                at_turn(
                    entry(
                        &format!("reasoning-{turn}"),
                        reasoning(&format!("Reasoning {turn}")),
                    ),
                    turn,
                ),
                at_turn(
                    entry(
                        &format!("assistant-{turn}"),
                        assistant(&format!("Assistant {turn}")),
                    ),
                    turn,
                ),
            ]);
        }
        timeline
    }

    fn single_assistant_timeline(id: &str, text: &str) -> Timeline {
        let mut timeline = Timeline::default();
        timeline.turns = vec![TurnMeta::default()];
        timeline.entries.push(entry(id, assistant(text)));
        timeline
    }

    fn large_markdown(marker: &str) -> String {
        let mut text = format!("# {marker}\n\n");
        while text.len() <= ASYNC_MARKDOWN_THRESHOLD_BYTES {
            text.push_str("- a sufficiently substantial Markdown list item\n");
        }
        text
    }

    fn seed_chat(
        cx: &mut TestAppContext,
        timeline: Timeline,
    ) -> (Entity<WorkspaceStore>, Entity<WindowState>, String) {
        let (store, window_state, session_id, _) = seed_chat_with_history(cx, timeline, false);
        (store, window_state, session_id)
    }

    fn seeded_chat_host(
        cx: &mut TestAppContext,
        timeline: Timeline,
        paged: bool,
    ) -> (tcode_runtime::pipe::SpawnedHost, String, Timeline) {
        use tcode_runtime::pipe::{HostServices, spawn_host};
        use tcode_services::store::SessionStore;

        cx.update(crate::theme::init);
        cx.update(crate::markdown::init);
        let data_root = crate::store::tests::scratch_root("chat-residency-test");
        let store = SessionStore::open_at(data_root).expect("test session store");
        let host = spawn_host(store, HostServices::default()).expect("spawn test host");
        let (session_id, timeline) =
            smol::block_on(host.update_state_for_test(move |state, cx| {
                let id =
                    state.start_draft("markdown-residency-test".into(), std::env::temp_dir(), cx);
                if paged {
                    for ts in 0..2000 {
                        state.record_event_for_replica_test(
                            &id,
                            ts,
                            &agent::AgentEvent::Warning {
                                message: "short".into(),
                            },
                            cx,
                        );
                    }
                }
                let active = state.residents.live.get_mut(&id).expect("selected draft");
                active.timeline = timeline;
                active.draft = false;
                let result = (active.meta.id.clone(), active.timeline.clone());
                let meta = active.meta.clone();
                state.sessions.push(meta);
                result
            }))
            .expect("seed markdown host");
        (host, session_id, timeline)
    }

    fn seed_chat_with_history(
        cx: &mut TestAppContext,
        timeline: Timeline,
        paged: bool,
    ) -> (
        Entity<WorkspaceStore>,
        Entity<WindowState>,
        String,
        crate::store::tests::LocalHostDriver,
    ) {
        let (host, session_id, timeline) = seeded_chat_host(cx, timeline, paged);
        let mut driver = crate::store::tests::LocalHostDriver::new(&host);
        let workspace_store = cx.new(|cx| {
            WorkspaceStore::new_attached(
                driver.link.clone(),
                crate::store::WorkspaceAttachment::Local,
                None,
                None,
                false,
                cx,
            )
        });
        driver.wait_until(cx, &workspace_store, "workspace baseline", |cx| {
            workspace_store.read_with(cx, |store, _| store.baseline_ready())
        });
        if paged {
            let link = driver.link.clone();
            driver
                .await_with_events(
                    cx,
                    &workspace_store,
                    link.query(tcode_protocol::Query::SessionHistoryPage {
                        session_id: session_id.clone(),
                        before: u64::MAX,
                        limit: 1,
                    }),
                )
                .expect("the seeded history is read");
        }
        workspace_store.update(cx, |store, _| store.select_session(session_id.clone()));
        driver.wait_until(cx, &workspace_store, "selected conversation", |cx| {
            workspace_store.read_with(cx, |store, _| {
                store.baseline_ready() && !store.chat_loading()
            })
        });
        workspace_store.update(cx, |store, cx| {
            store.set_session_replica_for_test(session_id.clone(), timeline, cx);
        });
        driver.wait_until(cx, &workspace_store, "Git status", |cx| {
            workspace_store.read_with(cx, |store, _| store.chat_git_controls().is_some())
        });
        let window_state = cx.new(|_| WindowState::new(false));
        let link = driver.link.clone();
        cx.on_quit(move || {
            let events = link.events();
            smol::block_on(smol::future::race(
                link.pump(),
                smol::future::race(
                    async { link.shutdown().await.expect("stop chat test host") },
                    async {
                        // The UI is gone at test teardown; drain its retired subscriptions.
                        while events.recv().await.is_ok() {}
                        std::future::pending::<()>().await;
                    },
                ),
            ));
            host.stopped
                .recv_blocking()
                .expect("chat test host stopped");
        });
        (workspace_store, window_state, session_id, driver)
    }
}
