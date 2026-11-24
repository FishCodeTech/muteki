#[cfg(not(target_family = "wasm"))]
use std::time::Instant;
use std::{
    cell::RefCell,
    collections::{HashMap, HashSet},
    ops::Range,
    rc::Rc,
    time::Duration,
};
#[cfg(target_family = "wasm")]
use web_time::Instant;

use crate::theme::ActiveTheme as _;
use crate::typography::TextRole as _;
use crate::widgets::button::{Button, ButtonVariants as _};
use crate::widgets::menu::{ContextMenuExt as _, DropdownMenu as _};
use crate::{
    icon::{Icon, IconName},
    sizing::Sizable as _,
    widgets::tabs::{TabBar, TabItem},
};
use gpui::{
    Action, AnyElement, App, AppContext as _, Bounds, ClipboardItem, ContentMask, Context, Entity,
    ExternalPaths, FocusHandle, Focusable, FontFeatures, FontStyle, FontWeight, Hsla, InputHandler,
    InteractiveElement as _, IntoElement, KeyDownEvent, MouseButton, MouseDownEvent,
    MouseMoveEvent, MouseUpEvent, ParentElement as _, Pixels, Point, Render, Role,
    ScrollWheelEvent, Styled as _, Task, TextAlign, TextRun, UTF16Selection, UnderlineStyle,
    Window, canvas, div, fill, font, point, prelude::FluentBuilder as _, px, rgb, size,
};
use gpui_base::{ElementExt as _, h_flex, h_resizable, resizable_panel, v_flex, v_resizable};
use tcode_protocol::terminal::{
    CellFlags, CellWidth, CursorShape, TerminalColor, TerminalMode as Mode,
    mappings::{self, GridPoint, Modifiers as TermModifiers, MouseButton as TermMouseButton},
};

use crate::{
    material,
    store::{
        StoreChange, TopicKind, WorkspaceStore, observe_store_topics,
        terminal::{HyperlinkMatch, SelectionKind, SelectionSide, TerminalModel},
    },
    terminal_key_bar::{
        TerminalKey, TerminalKeyBar, TerminalKeyBarEvent, should_show_terminal_key_bar,
    },
};
use tcode_core::ui::{MAX_TERMINALS_PER_SESSION, TerminalSplitDirection};

pub(crate) const TERMINAL_FONT_SIZE: f32 = 13.;
pub(crate) const TERMINAL_CELL_WIDTH: f32 = 7.83;
pub(crate) const TERMINAL_CELL_HEIGHT: f32 = 17.;
#[cfg(target_os = "macos")]
pub(crate) const TERMINAL_FONT_FAMILY: &str = "Menlo";
#[cfg(target_os = "windows")]
pub(crate) const TERMINAL_FONT_FAMILY: &str = "Consolas";
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
pub(crate) const TERMINAL_FONT_FAMILY: &str = "Lilex";
const PANE_PADDING: f32 = 8.;
const SELECTION_DRAG_THRESHOLD: f32 = 2.;

#[derive(Clone, Copy, Debug, PartialEq)]
struct ScreenPoint {
    x: f32,
    y: f32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SelectionDragAction {
    None,
    ClearAndWait,
    Start {
        kind: SelectionKind,
        point: (usize, usize),
        side: SelectionSide,
    },
    Update {
        point: (usize, usize),
        side: SelectionSide,
    },
    StartSimpleAndUpdate {
        anchor: (usize, usize),
        anchor_side: SelectionSide,
        point: (usize, usize),
        side: SelectionSide,
    },
}

#[derive(Default)]
struct SelectionDrag {
    selecting: Option<u64>,
    pending_simple: Option<(u64, (usize, usize), SelectionSide)>,
    mouse_down: Option<(u64, ScreenPoint)>,
    last_reported_point: HashMap<u64, (usize, usize)>,
}

impl SelectionDrag {
    fn on_down(
        &mut self,
        terminal_id: u64,
        position: ScreenPoint,
        point: (usize, usize),
        side: SelectionSide,
        click_count: usize,
        shift: bool,
    ) -> SelectionDragAction {
        self.mouse_down = Some((terminal_id, position));
        self.selecting = None;
        self.pending_simple = None;
        let kind = match click_count {
            1 => SelectionKind::Simple,
            2 => SelectionKind::Semantic,
            3 => SelectionKind::Lines,
            _ => return SelectionDragAction::None,
        };
        if kind == SelectionKind::Simple && shift {
            return SelectionDragAction::Update { point, side };
        }
        if kind == SelectionKind::Simple {
            self.pending_simple = Some((terminal_id, point, side));
            return SelectionDragAction::ClearAndWait;
        }
        SelectionDragAction::Start { kind, point, side }
    }

    fn on_move(
        &mut self,
        terminal_id: u64,
        position: ScreenPoint,
        point: (usize, usize),
        side: SelectionSide,
        left_pressed: bool,
    ) -> SelectionDragAction {
        if !left_pressed
            || self
                .mouse_down
                .is_none_or(|(mouse_id, _)| mouse_id != terminal_id)
        {
            return SelectionDragAction::None;
        }
        if self.selecting != Some(terminal_id)
            && let Some((_, mouse_down)) = self.mouse_down
        {
            if !selection_drag_started(position.x - mouse_down.x, position.y - mouse_down.y) {
                return SelectionDragAction::None;
            }
            self.selecting = Some(terminal_id);
            if self
                .pending_simple
                .is_some_and(|(pending_id, _, _)| pending_id == terminal_id)
                && let Some((_, anchor, anchor_side)) = self.pending_simple.take()
            {
                return SelectionDragAction::StartSimpleAndUpdate {
                    anchor,
                    anchor_side,
                    point,
                    side,
                };
            }
        }
        self.selecting = Some(terminal_id);
        SelectionDragAction::Update { point, side }
    }

    fn on_up(&mut self, terminal_id: u64) -> bool {
        let was_selecting = self.selecting == Some(terminal_id);
        if was_selecting {
            self.selecting = None;
        }
        if self
            .mouse_down
            .is_some_and(|(mouse_id, _)| mouse_id == terminal_id)
        {
            self.mouse_down = None;
        }
        if self
            .pending_simple
            .is_some_and(|(pending_id, _, _)| pending_id == terminal_id)
        {
            self.pending_simple = None;
        }
        self.last_reported_point.remove(&terminal_id);
        was_selecting
    }

    fn should_report_mouse_move(&mut self, terminal_id: u64, point: (usize, usize)) -> bool {
        if self.last_reported_point.get(&terminal_id) == Some(&point) {
            false
        } else {
            self.last_reported_point.insert(terminal_id, point);
            true
        }
    }
}

pub(crate) const CONTEXT: &str = "Terminal";

#[derive(Action, Clone, PartialEq, Eq, serde::Deserialize)]
#[action(namespace = tcode_terminal, no_json)]
struct TerminalCopy(u64);
#[derive(Action, Clone, PartialEq, Eq, serde::Deserialize)]
#[action(namespace = tcode_terminal, no_json)]
struct TerminalPaste(u64);
#[derive(Action, Clone, PartialEq, Eq, serde::Deserialize)]
#[action(namespace = tcode_terminal, no_json)]
struct TerminalSelectAll(u64);
#[derive(Action, Clone, PartialEq, Eq, serde::Deserialize)]
#[action(namespace = tcode_terminal, no_json)]
struct TerminalClear(u64);
#[derive(Action, Clone, PartialEq, Eq, serde::Deserialize)]
#[action(namespace = tcode_terminal, no_json)]
struct TerminalAddContext(u64);

/// A tab's context-menu action. Restart and the splits act on the active
/// terminal, so the tab is activated first.
#[derive(Action, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
#[action(namespace = tcode_terminal, no_json)]
struct TerminalTabMenu {
    id: u64,
    kind: TerminalTabMenuKind,
}

#[derive(Clone, Copy, PartialEq, Eq, serde::Deserialize)]
enum TerminalTabMenuKind {
    Restart,
    SplitHorizontal,
    SplitVertical,
    Close,
}

/// A drawer action that does not fit the compact toolbar row and lives in its
/// overflow menu, which addresses items by action rather than by callback.
#[derive(Action, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
#[action(namespace = tcode_terminal, no_json)]
enum TerminalOverflow {
    SplitHorizontal,
    SplitVertical,
    Restart,
    Close,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ClipboardShortcut {
    Copy,
    Paste,
}

#[derive(Clone, Copy)]
struct GridGeometry {
    bounds: Bounds<Pixels>,
    cols: usize,
    rows: usize,
    cell_width: f32,
    cell_height: f32,
}

#[derive(Clone)]
struct MarkedText {
    terminal_id: u64,
    text: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct GridTextStyle {
    pub(crate) fg: TerminalColor,
    pub(crate) bg: TerminalColor,
    bold: bool,
    italic: bool,
    underline: bool,
    underline_wavy: bool,
    selected: bool,
    cursor: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct BatchedTextRun {
    pub(crate) row: usize,
    pub(crate) start_col: usize,
    pub(crate) text: String,
    /// The number of non-spacer grid cells, matching Zed's batching model.
    cell_count: usize,
    pub(crate) style: GridTextStyle,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct TerminalPalette {
    pub(crate) foreground: Hsla,
    pub(crate) background: Hsla,
    pub(crate) selection: Hsla,
}

#[derive(Clone, Copy)]
struct BackgroundRect {
    row: usize,
    start_col: usize,
    cell_count: usize,
    color: Hsla,
}

#[derive(Clone, Copy)]
struct CursorPaint {
    row: usize,
    start_col: usize,
    cell_count: usize,
    color: Hsla,
    visible: bool,
    shape: CursorShape,
    focused: bool,
}

#[derive(Clone)]
pub(crate) struct GridPaintData {
    pub(crate) text_runs: Vec<BatchedTextRun>,
    backgrounds: Vec<BackgroundRect>,
    selections: Vec<BackgroundRect>,
    cursor: Option<CursorPaint>,
}

#[derive(Clone, Default)]
struct RowPaintData {
    text_runs: Vec<BatchedTextRun>,
    backgrounds: Vec<BackgroundRect>,
    selections: Vec<BackgroundRect>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct CursorRowKey {
    position: (usize, usize),
    shape: CursorShape,
    blinking: bool,
    marked_text: Option<String>,
    focused: bool,
    blink_phase: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct RowLayoutKey {
    /// The selected column span on this row, if any.
    selection: Option<(usize, usize)>,
    hovered_link: Option<((usize, usize), (usize, usize))>,
    cursor: Option<CursorRowKey>,
}

#[derive(Clone)]
struct CachedRowLayout {
    key: RowLayoutKey,
    paint: RowPaintData,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct GridCacheKey {
    cols: usize,
    screen_lines: usize,
    display_offset: usize,
    palette: TerminalPalette,
}

struct TerminalGridCache {
    key: GridCacheKey,
    rows: Vec<Option<CachedRowLayout>>,
}

pub struct TerminalDrawer {
    workspace_store: Entity<WorkspaceStore>,
    focus_handle: FocusHandle,
    key_bar: Entity<TerminalKeyBar>,
    grid_bounds: Rc<RefCell<HashMap<u64, GridGeometry>>>,
    /// Last-known panel sizes of the active split (from its resize handle),
    /// used to apportion the drawer body between the two panes when resizing
    /// their PTYs. Empty means "no drag yet": assume an even split.
    split_sizes: Rc<RefCell<Vec<f32>>>,
    row_layout_cache: RefCell<HashMap<u64, TerminalGridCache>>,
    cell_width: f32,
    cell_height: f32,
    scroll_remainder: HashMap<u64, f32>,
    selection_drag: SelectionDrag,
    _focus_subscriptions: Vec<gpui::Subscription>,
    marked_text: Option<MarkedText>,
    bell_tabs: HashSet<u64>,
    hovered_link: Option<(u64, HyperlinkMatch)>,
    pressed_link: Option<(u64, String)>,
    last_link_hover: Option<Instant>,
    cursor_phase: bool,
    last_input: Instant,
    terminal_focused: bool,
    current_size: Option<(f32, f32)>,
    _blink_task: Option<Task<()>>,
    _store_subscriptions: Vec<gpui::Subscription>,
}

impl TerminalDrawer {
    pub fn new(
        workspace_store: Entity<WorkspaceStore>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let store_observer = observe_store_topics(
            &workspace_store,
            &[
                TopicKind::ActiveSession,
                TopicKind::SessionStatus,
                TopicKind::Terminal,
            ],
            cx,
        );
        // Bell and OSC 52 arrive with the grid delta rather than on a private
        // event stream, so they are drained where the store change lands.
        let notice_observer = cx.subscribe_in(
            &workspace_store,
            window,
            |this, _, change: &StoreChange, window, cx| {
                if change.topic == TopicKind::Terminal {
                    this.drain_terminal_notices(window, cx);
                }
            },
        );
        let focus_handle = cx.focus_handle().tab_index(0).tab_stop(true);
        let key_bar = cx.new(|_| TerminalKeyBar::new(focus_handle.clone()));
        let focus_store = workspace_store.clone();
        let focus_in = window.on_focus_in(&focus_handle, cx, move |_, cx| {
            focus_store.read(cx).with_terminal_workspace(|workspace| {
                if let Some(entry) = workspace.active()
                    && entry.terminal.mode().contains(Mode::FOCUS_IN_OUT)
                {
                    entry.terminal.write_raw(b"\x1b[I".to_vec());
                }
            });
        });
        let focus_store = workspace_store.clone();
        let focus_out = window.on_focus_out(&focus_handle, cx, move |_, _, cx| {
            focus_store.read(cx).with_terminal_workspace(|workspace| {
                if let Some(entry) = workspace.active()
                    && entry.terminal.mode().contains(Mode::FOCUS_IN_OUT)
                {
                    entry.terminal.write_raw(b"\x1b[O".to_vec());
                }
            });
        });
        let key_bar_subscription = cx.subscribe_in(
            &key_bar,
            window,
            |this, _, event: &TerminalKeyBarEvent, window, cx| {
                this.send_key_bar_key(event.0, cx);
                this.focus_handle.focus(window, cx);
            },
        );
        #[cfg(not(test))]
        let blink_task = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(500))
                    .await;
                if this
                    .update(cx, |this, cx| {
                        this.cursor_phase = !this.cursor_phase;
                        cx.notify();
                    })
                    .is_err()
                {
                    break;
                }
            }
        }));
        // GPUI's deterministic test scheduler rejects the async-io timer's
        // background-thread wakeup during window teardown.
        #[cfg(test)]
        let blink_task = None;
        Self {
            workspace_store,
            focus_handle,
            key_bar,
            grid_bounds: Rc::new(RefCell::new(HashMap::new())),
            split_sizes: Rc::new(RefCell::new(Vec::new())),
            row_layout_cache: RefCell::new(HashMap::new()),
            cell_width: TERMINAL_CELL_WIDTH,
            cell_height: TERMINAL_CELL_HEIGHT,
            scroll_remainder: HashMap::new(),
            selection_drag: SelectionDrag::default(),
            _focus_subscriptions: vec![focus_in, focus_out, key_bar_subscription],
            marked_text: None,
            bell_tabs: HashSet::new(),
            hovered_link: None,
            pressed_link: None,
            last_link_hover: None,
            cursor_phase: true,
            last_input: Instant::now(),
            terminal_focused: false,
            current_size: None,
            _blink_task: blink_task,
            _store_subscriptions: vec![store_observer, notice_observer],
        }
    }

    pub fn is_size(&self, width: f32, height: f32) -> bool {
        self.current_size == Some((width, height))
    }

    pub fn resize(&mut self, width: f32, height: f32, cx: &mut Context<Self>) {
        if self.is_size(width, height) {
            return;
        }
        self.current_size = Some((width, height));
        self.workspace_store
            .update(cx, |store, cx| store.set_terminal_height(height, cx));
    }

    fn with_terminal(&self, cx: &mut Context<Self>, f: impl FnOnce(&crate::store::ClientTerminal)) {
        self.workspace_store
            .read(cx)
            .with_terminal_workspace(|workspace| {
                if let Some(entry) = workspace.active() {
                    f(&entry.terminal);
                }
            });
    }

    fn with_terminal_id(
        &self,
        terminal_id: u64,
        cx: &mut Context<Self>,
        f: impl FnOnce(&crate::store::ClientTerminal),
    ) {
        self.workspace_store
            .read(cx)
            .with_terminal_workspace(|workspace| {
                if let Some(entry) = workspace.terminal(terminal_id) {
                    f(&entry.terminal);
                }
            });
    }

    fn paste_to_terminal(&self, terminal_id: u64, text: &str, cx: &mut Context<Self>) {
        self.with_terminal_id(terminal_id, cx, |terminal| {
            let mode = terminal.mode();
            let text = prepare_terminal_paste(text, mode.contains(Mode::BRACKETED_PASTE));
            terminal.write_input(text.into_bytes());
        });
    }

    fn send_key_bar_key(&mut self, key: TerminalKey, cx: &mut Context<Self>) {
        let terminal = self
            .workspace_store
            .read(cx)
            .with_terminal_workspace(|workspace| {
                workspace.active().map(|entry| entry.terminal.clone())
            })
            .flatten();
        let Some(terminal) = terminal else {
            return;
        };
        let bytes = self.key_bar.update(cx, |bar, _| {
            bar.encode_key(
                key,
                terminal.mode(),
                terminal.keyboard_mode(),
                terminal.modify_other_keys(),
            )
        });
        terminal.write_input(bytes);
        self.note_input(cx);
    }

    fn send_committed_text(&mut self, terminal_id: u64, text: &str, cx: &mut Context<Self>) {
        let terminal = self
            .workspace_store
            .read(cx)
            .with_terminal_workspace(|workspace| {
                workspace
                    .terminal(terminal_id)
                    .map(|entry| entry.terminal.clone())
            })
            .flatten();
        let Some(terminal) = terminal else {
            return;
        };
        let bytes = self.key_bar.update(cx, |bar, cx| {
            let bytes = bar.encode_text(
                text,
                terminal.mode(),
                terminal.keyboard_mode(),
                terminal.modify_other_keys(),
            );
            cx.notify();
            bytes
        });
        if !bytes.is_empty() {
            terminal.write_input(bytes);
        }
    }

    fn on_terminal_copy(
        &mut self,
        action: &TerminalCopy,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(text) = self
            .workspace_store
            .read(cx)
            .with_terminal_workspace(|workspace| {
                workspace
                    .terminal(action.0)
                    .and_then(|entry| entry.terminal.selected_text())
                    .map(|selection| selection.text)
            })
            .flatten()
        {
            cx.write_to_clipboard(ClipboardItem::new_string(text));
        }
    }

    fn on_terminal_paste(
        &mut self,
        action: &TerminalPaste,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
            self.paste_to_terminal(action.0, &text, cx);
        }
    }

    fn on_terminal_select_all(
        &mut self,
        action: &TerminalSelectAll,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.with_terminal_id(action.0, cx, |terminal| terminal.select_all());
    }

    fn on_terminal_clear(
        &mut self,
        action: &TerminalClear,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.with_terminal_id(action.0, cx, |terminal| terminal.clear());
    }

    fn on_terminal_add_context(
        &mut self,
        action: &TerminalAddContext,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.workspace_store
            .update(cx, |store, _cx| store.capture_terminal_selection(action.0));
    }

    fn on_overflow(
        &mut self,
        action: &TerminalOverflow,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.workspace_store.update(cx, |store, cx| match action {
            TerminalOverflow::SplitHorizontal => {
                store.split_terminal(TerminalSplitDirection::Horizontal)
            }
            TerminalOverflow::SplitVertical => {
                store.split_terminal(TerminalSplitDirection::Vertical)
            }
            TerminalOverflow::Restart => store.restart_terminal(),
            TerminalOverflow::Close => store.close_terminal_panel(cx),
        });
    }

    fn on_tab_menu(&mut self, action: &TerminalTabMenu, _: &mut Window, cx: &mut Context<Self>) {
        let TerminalTabMenu { id, kind } = *action;
        self.workspace_store.update(cx, |store, cx| match kind {
            TerminalTabMenuKind::Restart => {
                store.activate_terminal(id);
                store.restart_terminal();
            }
            TerminalTabMenuKind::SplitHorizontal => {
                store.activate_terminal(id);
                store.split_terminal(TerminalSplitDirection::Horizontal);
            }
            TerminalTabMenuKind::SplitVertical => {
                store.activate_terminal(id);
                store.split_terminal(TerminalSplitDirection::Vertical);
            }
            TerminalTabMenuKind::Close => store.close_terminal(id, cx),
        });
    }

    /// Apply the bell and OSC 52 notices carried by the latest grid deltas.
    fn drain_terminal_notices(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let notices = self
            .workspace_store
            .read(cx)
            .with_terminal_workspace(|workspace| {
                workspace
                    .terminals
                    .iter()
                    .map(|entry| (entry.id, entry.terminal.take_notices()))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        for (terminal_id, (bell, clipboard)) in notices {
            if bell {
                self.bell_tabs.insert(terminal_id);
                window.play_system_bell();
            }
            // GPUI exposes the system clipboard but no primary-selection
            // clipboard. macOS has no primary selection, and on other platforms
            // substituting the system clipboard would be wrong.
            if let Some(clipboard) = clipboard.filter(|clipboard| !clipboard.selection) {
                cx.write_to_clipboard(ClipboardItem::new_string(clipboard.text));
            }
        }
        window.invalidate_character_coordinates();
    }

    fn on_key_down(&mut self, event: &KeyDownEvent, _window: &mut Window, cx: &mut Context<Self>) {
        self.note_input(cx);
        let keystroke = &event.keystroke;
        if let Some(shortcut) = terminal_clipboard_shortcut(
            &keystroke.key,
            keystroke.modifiers,
            cfg!(target_os = "macos"),
        ) {
            match shortcut {
                ClipboardShortcut::Copy => {
                    if let Some(text) = self
                        .workspace_store
                        .read(cx)
                        .with_terminal_workspace(|workspace| {
                            workspace
                                .active()
                                .and_then(|entry| entry.terminal.selected_text())
                                .map(|selection| selection.text)
                        })
                        .flatten()
                    {
                        cx.write_to_clipboard(ClipboardItem::new_string(text));
                    }
                }
                // Left unhandled, the key makes the browser fire its paste
                // event, the only way a page receives the clipboard; it
                // arrives through `TerminalInputHandler::paste`.
                ClipboardShortcut::Paste if cfg!(target_family = "wasm") => return,
                ClipboardShortcut::Paste => {
                    if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text())
                        && let Some(terminal_id) = self
                            .workspace_store
                            .read(cx)
                            .with_terminal_workspace(|workspace| workspace.active_id)
                            .flatten()
                    {
                        self.paste_to_terminal(terminal_id, &text, cx);
                    }
                }
            }
            cx.stop_propagation();
            return;
        }
        let mut handled = false;
        self.with_terminal(cx, |terminal| {
            if let Some(bytes) = terminal_key_bytes(
                keystroke,
                terminal.mode(),
                terminal.keyboard_mode(),
                terminal.modify_other_keys(),
            ) {
                terminal.write_input(bytes);
                handled = true;
            }
        });
        if handled {
            cx.stop_propagation();
        }
    }

    fn note_input(&mut self, cx: &mut Context<Self>) {
        self.last_input = Instant::now();
        self.cursor_phase = true;
        if let Some(id) = self
            .workspace_store
            .read(cx)
            .with_terminal_workspace(|workspace| workspace.active_id)
            .flatten()
        {
            self.bell_tabs.remove(&id);
        }
        cx.notify();
    }

    fn on_scroll(
        &mut self,
        terminal_id: u64,
        event: &ScrollWheelEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let delta = f32::from(event.delta.pixel_delta(px(self.cell_height)).y);
        let remainder = self.scroll_remainder.entry(terminal_id).or_default();
        let total = *remainder + delta;
        let lines = (total / self.cell_height).trunc() as i32;
        *remainder = total - lines as f32 * self.cell_height;
        if lines != 0 {
            self.workspace_store
                .read(cx)
                .with_terminal_workspace(|workspace| {
                    if let Some(entry) = workspace.terminal(terminal_id) {
                        let mode = entry.terminal.mode();
                        let point = self
                            .grid_point_and_side(terminal_id, event.position)
                            .map(|((row, column), _)| GridPoint { row, column })
                            .unwrap_or(GridPoint { row: 0, column: 0 });
                        if mappings::routes_mouse(mode, event.modifiers.shift) {
                            if let Some(bytes) = mappings::scroll_report(
                                point,
                                lines,
                                term_modifiers(event.modifiers),
                                mode,
                            ) {
                                entry.terminal.write_raw(bytes);
                            }
                        } else if mode.contains(Mode::ALT_SCREEN)
                            && mode.contains(Mode::ALTERNATE_SCROLL)
                            && !event.modifiers.shift
                        {
                            entry.terminal.write_raw(mappings::alt_scroll(lines));
                        } else {
                            entry.terminal.scroll(lines);
                        }
                    }
                });
            cx.stop_propagation();
            cx.notify();
        }
    }

    fn render_grid(
        &self,
        terminal_id: u64,
        state: &TerminalModel,
        register_input: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let palette = TerminalPalette {
            foreground: cx.theme().foreground,
            background: cx.theme().background,
            selection: cx.theme().primary.opacity(0.28),
        };
        let marked_text = self
            .marked_text
            .as_ref()
            .filter(|marked| marked.terminal_id == terminal_id)
            .map(|marked| marked.text.clone());
        let hovered_link = self
            .hovered_link
            .as_ref()
            .filter(|(id, _)| *id == terminal_id)
            .map(|(_, link)| link);
        let blink_phase =
            self.cursor_phase || self.last_input.elapsed() < Duration::from_millis(500);
        let paint_data = layout_grid_cached(
            &mut self.row_layout_cache.borrow_mut(),
            terminal_id,
            state,
            palette,
            marked_text.as_deref(),
            hovered_link,
            self.terminal_focused,
            blink_phase,
        );
        let cell_width = self.cell_width;
        let cell_height = self.cell_height;
        let cols = state.cols();
        let rows = state.rows();
        let focus_handle = self.focus_handle.clone();
        let drawer = cx.entity();
        let grid_bounds = self.grid_bounds.clone();

        canvas(
            |_bounds, _window, _cx| (),
            move |bounds, (), window, cx| {
                window.with_content_mask(Some(ContentMask { bounds }), |window| {
                    let scale_factor = window.scale_factor();
                    let origin = snapped_grid_origin(bounds, scale_factor);
                    grid_bounds.borrow_mut().insert(
                        terminal_id,
                        GridGeometry {
                            bounds: Bounds::new(origin, bounds.size),
                            cols,
                            rows,
                            cell_width,
                            cell_height,
                        },
                    );

                    let cursor_bounds = paint_terminal_grid(
                        bounds,
                        window,
                        cx,
                        &paint_data,
                        palette,
                        cell_width,
                        cell_height,
                        marked_text.is_none(),
                    );

                    if let Some(marked_text) = marked_text.as_ref().filter(|text| !text.is_empty())
                        && let Some(cursor_bounds) = cursor_bounds
                    {
                        let ime_run = TextRun {
                            len: marked_text.len(),
                            font: terminal_font(),
                            color: palette.foreground,
                            background_color: None,
                            strikethrough: None,
                            underline: Some(UnderlineStyle {
                                thickness: px(1.),
                                color: Some(palette.foreground),
                                wavy: false,
                            }),
                        };
                        let shaped = window.text_system().shape_line(
                            marked_text.clone().into(),
                            px(TERMINAL_FONT_SIZE),
                            &[ime_run],
                            None,
                        );
                        let covered_cells = (f32::from(shaped.width) / cell_width).ceil().max(1.);
                        let ime_bounds = Bounds::new(
                            cursor_bounds.origin,
                            size(px(covered_cells * cell_width), px(cell_height)),
                        );
                        window.paint_quad(fill(ime_bounds, palette.background));
                        let _ = shaped.paint(
                            cursor_bounds.origin,
                            px(cell_height),
                            TextAlign::Left,
                            None,
                            window,
                            cx,
                        );
                    }

                    if register_input {
                        window.handle_input(
                            &focus_handle,
                            TerminalInputHandler {
                                drawer,
                                terminal_id,
                                cursor_bounds,
                                cell_width: px(cell_width),
                            },
                            cx,
                        );
                    }
                });
            },
        )
        // The pane is rarely an exact multiple of the cell metrics; it centers
        // this canvas, so the sub-cell remainder is split evenly on both axes
        // instead of accumulating at the right/bottom edge.
        .w(px(cols as f32 * cell_width))
        .h(px(rows as f32 * cell_height))
        .into_any_element()
    }

    fn grid_point_and_side(
        &self,
        terminal_id: u64,
        position: gpui::Point<Pixels>,
    ) -> Option<((usize, usize), SelectionSide)> {
        let geometry = *self.grid_bounds.borrow().get(&terminal_id)?;
        Some(grid_point_and_side(
            f32::from(position.x - geometry.bounds.left()),
            f32::from(position.y - geometry.bounds.top()),
            geometry.cols,
            geometry.rows,
            geometry.cell_width,
            geometry.cell_height,
        ))
    }

    fn terminal_mouse_down(
        &mut self,
        terminal_id: u64,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.focus_handle.focus(window, cx);
        window.request_virtual_keyboard();
        let Some((point, side)) = self.grid_point_and_side(terminal_id, event.position) else {
            return;
        };
        self.workspace_store
            .update(cx, |store, _cx| store.activate_terminal(terminal_id));
        let workspace_store = self.workspace_store.clone();
        let mut stop_propagation = false;
        #[cfg(target_os = "linux")]
        let primary_text = (event.button == MouseButton::Middle)
            .then(|| cx.read_from_primary().and_then(|item| item.text()))
            .flatten();
        workspace_store
            .read(cx)
            .with_terminal_workspace(|workspace| {
                if let Some(entry) = workspace.terminal(terminal_id) {
                    let mode = entry.terminal.mode();
                    if mappings::routes_mouse(mode, event.modifiers.shift) {
                        if let Some(button) = term_mouse_button(event.button)
                            && let Some(bytes) = mappings::mouse_button_report(
                                GridPoint {
                                    row: point.0,
                                    column: point.1,
                                },
                                button,
                                term_modifiers(event.modifiers),
                                true,
                                mode,
                            )
                        {
                            entry.terminal.write_raw(bytes);
                        }
                        if event.button == MouseButton::Right {
                            stop_propagation = true;
                        }
                        return;
                    }
                    if event.button == MouseButton::Right {
                        if entry.terminal.selected_text().is_none() {
                            entry
                                .terminal
                                .start_selection(SelectionKind::Semantic, point, side);
                        }
                        return;
                    }
                    #[cfg(target_os = "linux")]
                    if event.button == MouseButton::Middle {
                        if let Some(text) = primary_text {
                            let text =
                                prepare_terminal_paste(&text, mode.contains(Mode::BRACKETED_PASTE));
                            entry.terminal.write_input(text.into_bytes());
                        }
                        stop_propagation = true;
                        return;
                    }
                    if event.button != MouseButton::Left {
                        return;
                    }
                    if terminal_link_modifier(event.modifiers, cfg!(target_os = "macos"))
                        && let Some(link) = entry.terminal.hyperlink_at(point.0, point.1)
                    {
                        self.pressed_link = Some((terminal_id, link.url));
                        return;
                    }
                    let action = self.selection_drag.on_down(
                        terminal_id,
                        ScreenPoint {
                            x: f32::from(event.position.x),
                            y: f32::from(event.position.y),
                        },
                        point,
                        side,
                        event.click_count,
                        event.modifiers.shift,
                    );
                    match action {
                        SelectionDragAction::None => {}
                        SelectionDragAction::ClearAndWait => entry.terminal.clear_selection(),
                        SelectionDragAction::Start { kind, point, side } => {
                            entry.terminal.start_selection(kind, point, side);
                        }
                        SelectionDragAction::Update { point, side } => {
                            entry.terminal.update_selection(point, side);
                        }
                        SelectionDragAction::StartSimpleAndUpdate { .. } => {
                            unreachable!("mouse down cannot start a drag")
                        }
                    }
                }
            });
        if stop_propagation {
            cx.stop_propagation();
        }
        cx.notify();
    }

    fn terminal_mouse_move(
        &mut self,
        terminal_id: u64,
        event: &MouseMoveEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some((point, side)) = self.grid_point_and_side(terminal_id, event.position) else {
            self.hovered_link = None;
            return;
        };
        let workspace_store = self.workspace_store.clone();
        workspace_store
            .read(cx)
            .with_terminal_workspace(|workspace| {
                let Some(entry) = workspace.terminal(terminal_id) else {
                    return;
                };
                let mode = entry.terminal.mode();
                if mappings::routes_mouse(mode, event.modifiers.shift) {
                    if self
                        .selection_drag
                        .should_report_mouse_move(terminal_id, point)
                        && let Some(bytes) = mappings::mouse_move_report(
                            GridPoint {
                                row: point.0,
                                column: point.1,
                            },
                            event.pressed_button.and_then(term_mouse_button),
                            term_modifiers(event.modifiers),
                            mode,
                        )
                    {
                        entry.terminal.write_raw(bytes);
                    }
                    return;
                }
                if terminal_link_modifier(event.modifiers, cfg!(target_os = "macos")) {
                    if self
                        .last_link_hover
                        .is_none_or(|last| last.elapsed() >= Duration::from_millis(16))
                    {
                        self.last_link_hover = Some(Instant::now());
                        self.hovered_link = entry
                            .terminal
                            .hyperlink_at(point.0, point.1)
                            .map(|link| (terminal_id, link));
                    }
                } else {
                    self.hovered_link = None;
                    let action = self.selection_drag.on_move(
                        terminal_id,
                        ScreenPoint {
                            x: f32::from(event.position.x),
                            y: f32::from(event.position.y),
                        },
                        point,
                        side,
                        event.pressed_button == Some(MouseButton::Left),
                    );
                    if !matches!(action, SelectionDragAction::None) {
                        match action {
                            SelectionDragAction::StartSimpleAndUpdate {
                                anchor,
                                anchor_side,
                                point,
                                side,
                            } => {
                                entry.terminal.start_selection(
                                    SelectionKind::Simple,
                                    anchor,
                                    anchor_side,
                                );
                                entry.terminal.update_selection(point, side);
                            }
                            SelectionDragAction::Update { point, side } => {
                                entry.terminal.update_selection(point, side);
                            }
                            SelectionDragAction::None
                            | SelectionDragAction::ClearAndWait
                            | SelectionDragAction::Start { .. } => {
                                unreachable!("unexpected mouse move action")
                            }
                        }
                        if !mode.contains(Mode::ALT_SCREEN)
                            && entry.terminal.history_size() > 0
                            && let Some(lines) = drag_scroll_lines(
                                event.position.y,
                                self.grid_bounds.borrow().get(&terminal_id).copied(),
                                self.cell_height,
                            )
                        {
                            entry.terminal.scroll(lines);
                        }
                    }
                }
            });
        cx.notify();
    }

    fn terminal_mouse_up(
        &mut self,
        terminal_id: u64,
        event: &MouseUpEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let released_url = self
            .grid_point_and_side(terminal_id, event.position)
            .and_then(|(point, _side)| {
                self.workspace_store
                    .read(cx)
                    .with_terminal_workspace(|workspace| {
                        let entry = workspace.terminal(terminal_id)?;
                        let mode = entry.terminal.mode();
                        if mappings::routes_mouse(mode, event.modifiers.shift) {
                            if let Some(button) = term_mouse_button(event.button)
                                && let Some(bytes) = mappings::mouse_button_report(
                                    GridPoint {
                                        row: point.0,
                                        column: point.1,
                                    },
                                    button,
                                    term_modifiers(event.modifiers),
                                    false,
                                    mode,
                                )
                            {
                                entry.terminal.write_raw(bytes);
                            }
                        } else if event.button == MouseButton::Left
                            && terminal_link_modifier(event.modifiers, cfg!(target_os = "macos"))
                        {
                            let released = entry
                                .terminal
                                .hyperlink_at(point.0, point.1)
                                .map(|link| link.url);
                            if let (Some((pressed_id, pressed)), Some(released)) =
                                (self.pressed_link.take(), released)
                                && pressed_id == terminal_id
                                && pressed == released
                            {
                                return Some(released);
                            }
                        }
                        None
                    })
                    .flatten()
            });
        if let Some(released_url) = released_url {
            cx.open_url(&released_url);
        }
        if self.selection_drag.on_up(terminal_id) {
            #[cfg(target_os = "linux")]
            if let Some(text) = self
                .workspace_store
                .read(cx)
                .with_terminal_workspace(|workspace| {
                    workspace
                        .terminal(terminal_id)
                        .and_then(|entry| entry.terminal.selected_text())
                        .map(|selection| selection.text)
                })
                .flatten()
            {
                cx.write_to_primary(ClipboardItem::new_string(text));
            }
        }
        self.pressed_link = None;
        cx.notify();
    }

    fn render_terminal(&self, terminal_id: u64, cx: &mut Context<Self>) -> AnyElement {
        let Some(terminal) = self
            .workspace_store
            .read(cx)
            .with_terminal_workspace(|workspace| {
                workspace
                    .terminal(terminal_id)
                    .map(|entry| entry.terminal.clone())
            })
            .flatten()
        else {
            return div().into_any_element();
        };
        let register_input = self
            .workspace_store
            .read(cx)
            .with_terminal_workspace(|workspace| workspace.active_id == Some(terminal_id))
            .unwrap_or(false);

        let model = terminal.model();
        let label = model.title();
        let (exited, exit_code) = (model.exited(), model.exit_code());
        let has_selection = model.has_selection();
        let mut grid = v_flex().child(self.render_grid(terminal_id, &model, register_input, cx));
        drop(model);
        terminal.model_mut().clear_damage();
        if exited {
            let status = exit_code
                .map(|code| crate::tr!("terminal.exited_code", code = code).into_owned())
                .unwrap_or_else(|| crate::tr!("terminal.exited").into_owned());
            grid = grid.child(
                div()
                    .h(px(self.cell_height))
                    .text_color(cx.theme().muted_foreground)
                    .child(status),
            );
        }

        // The add-to-context button is a pure overlay: it must never affect the
        // grid's geometry. Reserving space for it while a selection exists
        // resized the PTY mid-drag — rows jumped and blank lines appeared.
        let link_hovered = self
            .hovered_link
            .as_ref()
            .is_some_and(|(id, _)| *id == terminal_id);
        // PTY dimensions are deliberately NOT measured from this pane: its
        // percentage height does not resolve against the flex-sized drawer
        // body, so the pane hugs the grid's content height and any row count
        // derived from it is self-referential (frozen at its current value).
        // `render` measures the drawer body — which does track the real
        // height — and resizes every pane's PTY from there.
        div()
            .id(("terminal-grid", terminal_id))
            .relative()
            .size_full()
            .min_h_0()
            .overflow_hidden()
            .p(px(PANE_PADDING))
            .items_center()
            .justify_center()
            .when(link_hovered, |this| this.cursor_pointer())
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, event, window, cx| {
                    this.terminal_mouse_down(terminal_id, event, window, cx)
                }),
            )
            .on_mouse_down(
                MouseButton::Middle,
                cx.listener(move |this, event, window, cx| {
                    this.terminal_mouse_down(terminal_id, event, window, cx)
                }),
            )
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, event, window, cx| {
                    this.terminal_mouse_down(terminal_id, event, window, cx)
                }),
            )
            .on_mouse_move(cx.listener(move |this, event, window, cx| {
                this.terminal_mouse_move(terminal_id, event, window, cx)
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(move |this, event, window, cx| {
                    this.terminal_mouse_up(terminal_id, event, window, cx)
                }),
            )
            .on_mouse_up(
                MouseButton::Middle,
                cx.listener(move |this, event, window, cx| {
                    this.terminal_mouse_up(terminal_id, event, window, cx)
                }),
            )
            .on_mouse_up(
                MouseButton::Right,
                cx.listener(move |this, event, window, cx| {
                    this.terminal_mouse_up(terminal_id, event, window, cx)
                }),
            )
            .on_scroll_wheel(cx.listener(move |this, event, window, cx| {
                this.on_scroll(terminal_id, event, window, cx)
            }))
            .on_drop(cx.listener(move |this, paths: &ExternalPaths, window, cx| {
                if paths.paths().is_empty() {
                    return;
                }
                this.focus_handle.focus(window, cx);
                let quoted = paths
                    .paths()
                    .iter()
                    .map(|path| shell_quote(&path.to_string_lossy()))
                    .collect::<Vec<_>>()
                    .join(" ");
                this.paste_to_terminal(terminal_id, &format!(" {quoted} "), cx);
            }))
            .child(grid)
            .when(has_selection, |this| {
                this.child(
                    Button::new(("terminal-add-context", terminal_id))
                        .absolute()
                        .block_mouse_except_scroll()
                        .right(px(PANE_PADDING))
                        .top(px(PANE_PADDING))
                        .small()
                        .label(crate::tr!("terminal.add_context"))
                        .tooltip(format!("{} · {}", label, crate::tr!("terminal.selection")))
                        // The terminal must keep the selection this action captures.
                        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.workspace_store.update(cx, |store, _cx| {
                                store.capture_terminal_selection(terminal_id)
                            });
                        })),
                )
            })
            .context_menu({
                let workspace_store = self.workspace_store.clone();
                move |menu, _window, cx| {
                    let has_selection = workspace_store
                        .read(cx)
                        .with_terminal_workspace(|workspace| {
                            workspace
                                .terminal(terminal_id)
                                .and_then(|entry| entry.terminal.selected_text())
                                .is_some()
                        })
                        .unwrap_or(false);
                    menu.menu_with_enable(
                        crate::tr!("terminal.copy").into_owned(),
                        Box::new(TerminalCopy(terminal_id)),
                        has_selection,
                    )
                    // A browser hands the clipboard only to its own paste
                    // event, never to a menu the page draws.
                    .when(!cfg!(target_family = "wasm"), |menu| {
                        menu.menu(
                            crate::tr!("terminal.paste").into_owned(),
                            Box::new(TerminalPaste(terminal_id)),
                        )
                    })
                    .menu(
                        crate::tr!("terminal.select_all").into_owned(),
                        Box::new(TerminalSelectAll(terminal_id)),
                    )
                    .menu(
                        crate::tr!("terminal.clear").into_owned(),
                        Box::new(TerminalClear(terminal_id)),
                    )
                    .separator()
                    .menu_with_enable(
                        crate::tr!("terminal.add_context").into_owned(),
                        Box::new(TerminalAddContext(terminal_id)),
                        has_selection,
                    )
                }
            })
            .into_any_element()
    }
}

impl Focusable for TerminalDrawer {
    fn focus_handle(&self, _: &gpui::App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for TerminalDrawer {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.terminal_focused = self.focus_handle.is_focused(window);

        // PTY dimensions and mouse hit-testing use the exact advance and
        // vertical metrics of the same resolved face used by StyledText.
        let shaped_cell = window.text_system().shape_line(
            "MMMMMMMMMM".into(),
            px(TERMINAL_FONT_SIZE),
            &[TextRun {
                len: 10,
                font: terminal_font(),
                color: cx.theme().foreground,
                background_color: None,
                strikethrough: None,
                underline: None,
            }],
            None,
        );
        self.cell_width = f32::from(shaped_cell.width) / 10.;
        self.cell_height = f32::from(shaped_cell.ascent + shaped_cell.descent)
            .ceil()
            .max(TERMINAL_FONT_SIZE + 2.);
        let (tabs, active_id, active_split) = self
            .workspace_store
            .read(cx)
            .with_terminal_workspace(|workspace| {
                (
                    workspace
                        .terminals
                        .iter()
                        .map(|entry| {
                            (
                                entry.id,
                                entry.terminal.label(),
                                entry.terminal.exited(),
                                self.bell_tabs.contains(&entry.id),
                            )
                        })
                        .collect::<Vec<_>>(),
                    workspace.active_id,
                    workspace.active_id.and_then(|id| workspace.split_for(id)),
                )
            })
            .unwrap_or_default();

        if self
            .marked_text
            .as_ref()
            .is_some_and(|marked| Some(marked.terminal_id) != active_id)
        {
            self.marked_text = None;
        }

        let compact = crate::window_seam::window_is_compact(window, cx);
        let mut tab_strip = TabBar::new("terminal-tab-list")
            .underline()
            .hairline(false)
            .compact(compact)
            .label_max_w(px(160.))
            .aria_label(crate::tr!("terminal.tabs"))
            .flex_1()
            .min_w_0()
            .mb(px(-1.));
        for (id, label, _, bell) in &tabs {
            let id = *id;
            let selected = active_id == Some(id);
            let close_id = id;
            tab_strip = tab_strip.item(
                TabItem::new(("terminal-tab", id), label.clone())
                    .selected(selected)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.workspace_store
                            .update(cx, |store, _cx| store.activate_terminal(id));
                    }))
                    .trailing(
                        h_flex()
                            .gap(cx.theme().tokens.spacing.xxs)
                            .when(*bell, |this| {
                                this.child(
                                    div()
                                        .text_token(&cx.theme().tokens.typography.xs)
                                        .text_color(cx.theme().warning)
                                        .child("●"),
                                )
                            })
                            .child(
                                Button::new(("terminal-tab-close", close_id))
                                    .ghost()
                                    .compact()
                                    .xsmall()
                                    .icon(
                                        Icon::new(IconName::Close)
                                            .text_color(cx.theme().muted_foreground),
                                    )
                                    .tooltip(crate::tr!("terminal.close_tab"))
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.workspace_store.update(cx, |store, cx| {
                                            store.close_terminal(close_id, cx)
                                        });
                                    })),
                            ),
                    )
                    .context_menu({
                        let can_split = self
                            .workspace_store
                            .read(cx)
                            .session_status()
                            .is_some_and(|status| status.terminal_split_available);
                        move |menu, _, _| {
                            let item = |kind| Box::new(TerminalTabMenu { id, kind });
                            menu.menu(
                                crate::tr!("terminal.restart").into_owned(),
                                item(TerminalTabMenuKind::Restart),
                            )
                            .menu_with_enable(
                                crate::tr!("terminal.split_horizontal").into_owned(),
                                item(TerminalTabMenuKind::SplitHorizontal),
                                can_split,
                            )
                            .menu_with_enable(
                                crate::tr!("terminal.split_vertical").into_owned(),
                                item(TerminalTabMenuKind::SplitVertical),
                                can_split,
                            )
                            .separator()
                            .menu(
                                crate::tr!("terminal.close_tab").into_owned(),
                                item(TerminalTabMenuKind::Close),
                            )
                        }
                    }),
            );
        }

        let at_limit = self
            .workspace_store
            .read(cx)
            .session_status()
            .is_some_and(|status| status.terminal_limit_reached);
        let can_split = self
            .workspace_store
            .read(cx)
            .session_status()
            .is_some_and(|status| status.terminal_split_available);
        let active_exited = tabs
            .iter()
            .any(|(id, _, exited, _)| Some(*id) == active_id && *exited);
        // A compact page gives the drawer one toolbar row on the page inset:
        // the tab strip, a new-terminal target, and everything else in an
        // overflow menu rather than five dense controls fighting for 393pt.
        let new_tooltip = if at_limit {
            crate::tr!("terminal.max_reached", count = MAX_TERMINALS_PER_SESSION)
        } else {
            crate::tr!("terminal.new")
        };
        let mut header = material::bar_divider_below(
            material::bar("terminal-toolbar", material::Bar::Toolbar, compact, cx),
            cx,
        )
        .child(tab_strip);
        if compact {
            header = header
                .child(
                    material::toolbar_icon_button(
                        "terminal-new",
                        IconName::Plus,
                        new_tooltip,
                        true,
                    )
                    .flex_none()
                    .disabled(at_limit)
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.workspace_store
                            .update(cx, |store, _cx| store.new_terminal());
                    })),
                )
                .child(
                    material::toolbar_icon_button(
                        "terminal-overflow",
                        IconName::Ellipsis,
                        crate::tr!("mobile.more_actions"),
                        true,
                    )
                    .flex_none()
                    .dropdown_menu(move |menu, _, _| {
                        menu.menu_with_enable(
                            crate::tr!("terminal.split_horizontal").into_owned(),
                            Box::new(TerminalOverflow::SplitHorizontal),
                            can_split,
                        )
                        .menu_with_enable(
                            crate::tr!("terminal.split_vertical").into_owned(),
                            Box::new(TerminalOverflow::SplitVertical),
                            can_split,
                        )
                        .menu_with_enable(
                            crate::tr!("terminal.restart").into_owned(),
                            Box::new(TerminalOverflow::Restart),
                            active_exited,
                        )
                        .separator()
                        .menu(
                            crate::tr!("terminal.close").into_owned(),
                            Box::new(TerminalOverflow::Close),
                        )
                    }),
                );
        } else {
            header = header
                .when(active_exited, |this| {
                    this.child(
                        Button::new("terminal-restart")
                            .flex_none()
                            .ghost()
                            .small()
                            .label(crate::tr!("terminal.restart"))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.workspace_store
                                    .update(cx, |store, _cx| store.restart_terminal());
                            })),
                    )
                })
                .child(
                    Button::new("terminal-split-horizontal")
                        .flex_none()
                        .ghost()
                        .small()
                        .compact()
                        .label("↔")
                        .disabled(!can_split)
                        .tooltip(crate::tr!("terminal.split_horizontal"))
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.workspace_store.update(cx, |store, _cx| {
                                store.split_terminal(TerminalSplitDirection::Horizontal)
                            });
                        })),
                )
                .child(
                    Button::new("terminal-split-vertical")
                        .flex_none()
                        .ghost()
                        .small()
                        .compact()
                        .label("↕")
                        .disabled(!can_split)
                        .tooltip(crate::tr!("terminal.split_vertical"))
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.workspace_store.update(cx, |store, _cx| {
                                store.split_terminal(TerminalSplitDirection::Vertical)
                            });
                        })),
                )
                .child(
                    Button::new("terminal-new")
                        .flex_none()
                        .ghost()
                        .small()
                        .compact()
                        .label("+")
                        .disabled(at_limit)
                        .tooltip(new_tooltip)
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.workspace_store
                                .update(cx, |store, _cx| store.new_terminal());
                        })),
                )
                .child(
                    material::toolbar_icon_button(
                        "terminal-close-drawer",
                        IconName::Close,
                        crate::tr!("terminal.close"),
                        false,
                    )
                    .flex_none()
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.workspace_store
                            .update(cx, |store, cx| store.close_terminal_panel(cx));
                    })),
                );
        }

        if active_split.is_none() {
            self.split_sizes.borrow_mut().clear();
        }
        let body: AnyElement = match (active_id, active_split) {
            (_, Some(split)) => {
                let split_sizes = self.split_sizes.clone();
                let on_resize = move |state: &gpui::Entity<gpui_base::ResizableState>,
                                      _: &mut Window,
                                      cx: &mut App| {
                    *split_sizes.borrow_mut() = state
                        .read(cx)
                        .sizes()
                        .iter()
                        .map(|size| f32::from(*size))
                        .collect();
                };
                match split.direction {
                    TerminalSplitDirection::Horizontal => {
                        let first = resizable_panel()
                            .pr(px(2.))
                            .child(self.render_terminal(split.first, cx));
                        let second = resizable_panel()
                            .pl(px(2.))
                            .child(self.render_terminal(split.second, cx));
                        h_resizable(("terminal-split-h", split.first))
                            .on_resize(on_resize)
                            .child(first)
                            .child(second)
                            .into_any_element()
                    }
                    TerminalSplitDirection::Vertical => {
                        let first = resizable_panel()
                            .pb(px(2.))
                            .child(self.render_terminal(split.first, cx));
                        let second = resizable_panel()
                            .pt(px(2.))
                            .child(self.render_terminal(split.second, cx));
                        v_resizable(("terminal-split-v", split.first))
                            .on_resize(on_resize)
                            .child(first)
                            .child(second)
                            .into_any_element()
                    }
                }
            }
            (Some(id), None) => self.render_terminal(id, cx),
            _ => div()
                .p(cx.theme().tokens.spacing.md)
                .child(crate::tr!("terminal.starting"))
                .into_any_element(),
        };

        v_flex()
            .size_full()
            .min_h_0()
            .font_family(TERMINAL_FONT_FAMILY)
            .text_size(px(TERMINAL_FONT_SIZE))
            .on_action(cx.listener(Self::on_terminal_copy))
            .on_action(cx.listener(Self::on_terminal_paste))
            .on_action(cx.listener(Self::on_terminal_select_all))
            .on_action(cx.listener(Self::on_terminal_clear))
            .on_action(cx.listener(Self::on_terminal_add_context))
            .on_action(cx.listener(Self::on_overflow))
            .on_action(cx.listener(Self::on_tab_menu))
            .child(header)
            .child(
                crate::material::accessible_clickable(
                    div(),
                    "terminal-content",
                    Role::Terminal,
                    crate::tr!("terminal.content"),
                    cx,
                )
                .key_context(CONTEXT)
                .track_focus(&self.focus_handle)
                .on_key_down(cx.listener(Self::on_key_down))
                // The accessibility focus ring is painted as a shadow behind this
                // element. Keep the terminal surface opaque so the shadow cannot
                // show through the otherwise transparent grid as a solid blue fill.
                .bg(cx.theme().popover)
                .flex_1()
                .min_h_0()
                // Resize every pane's PTY from this element's bounds. This is
                // the innermost element whose laid-out height reliably tracks
                // the drawer; the panes themselves hug their grid content
                // (percentage heights fail to resolve below this point), which
                // previously froze the row count at its initial value.
                .on_prepaint({
                    let workspace_store = self.workspace_store.clone();
                    let (cell_width, cell_height) = (self.cell_width, self.cell_height);
                    let split_sizes = self.split_sizes.clone();
                    move |bounds, window, cx| {
                        let width = f32::from(bounds.size.width);
                        let height = f32::from(bounds.size.height);
                        let scale_factor = window.scale_factor();
                        let resize =
                            |cx: &App, terminal_id: u64, pane_width: f32, pane_height: f32| {
                                let cols = ((pane_width - 2. * PANE_PADDING) / cell_width)
                                    .floor()
                                    .max(2.) as usize;
                                let rows = ((pane_height - 2. * PANE_PADDING) / cell_height)
                                    .floor()
                                    .max(2.) as usize;
                                let cell_width_px =
                                    (cell_width * scale_factor).round().max(1.) as u32;
                                let cell_height_px =
                                    (cell_height * scale_factor).round().max(1.) as u32;
                                workspace_store
                                    .read(cx)
                                    .with_terminal_workspace(|workspace| {
                                        if let Some(entry) = workspace.terminal(terminal_id) {
                                            entry.terminal.resize_with_cell_size(
                                                cols,
                                                rows,
                                                cell_width_px,
                                                cell_height_px,
                                            );
                                        }
                                    });
                            };
                        match active_split {
                            None => {
                                if let Some(id) = active_id {
                                    resize(cx, id, width, height);
                                }
                            }
                            Some(split) => {
                                // Panel sizes from the resize handle; before any
                                // drag the group splits the axis evenly (1px
                                // handle). Each pane also carries a 2px gutter
                                // (`pr`/`pl`/`pb`/`pt`) toward the handle.
                                let sizes = split_sizes.borrow();
                                let axis = match split.direction {
                                    TerminalSplitDirection::Horizontal => width,
                                    TerminalSplitDirection::Vertical => height,
                                };
                                let (first, second) = match sizes.as_slice() {
                                    [first, second] => (*first, *second),
                                    _ => ((axis - 1.) / 2., (axis - 1.) / 2.),
                                };
                                match split.direction {
                                    TerminalSplitDirection::Horizontal => {
                                        resize(cx, split.first, first - 2., height);
                                        resize(cx, split.second, second - 2., height);
                                    }
                                    TerminalSplitDirection::Vertical => {
                                        resize(cx, split.first, width, first - 2.);
                                        resize(cx, split.second, width, second - 2.);
                                    }
                                }
                            }
                        }
                    }
                })
                .child(body),
            )
            .when(
                should_show_terminal_key_bar(self.terminal_focused, cx),
                |drawer| drawer.child(self.key_bar.clone()),
            )
    }
}

fn snapped_grid_origin(bounds: Bounds<Pixels>, scale_factor: f32) -> Point<Pixels> {
    let snap_down = |value: Pixels| px((f32::from(value) * scale_factor).floor() / scale_factor);
    point(snap_down(bounds.origin.x), snap_down(bounds.origin.y))
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn paint_terminal_grid(
    bounds: Bounds<Pixels>,
    window: &mut Window,
    cx: &mut App,
    paint_data: &GridPaintData,
    palette: TerminalPalette,
    cell_width: f32,
    cell_height: f32,
    show_cursor: bool,
) -> Option<Bounds<Pixels>> {
    let scale_factor = window.scale_factor();
    let snap_down = |value: Pixels| px((f32::from(value) * scale_factor).floor() / scale_factor);
    let snap_up = |value: Pixels| px((f32::from(value) * scale_factor).ceil() / scale_factor);
    let origin = snapped_grid_origin(bounds, scale_factor);

    for background in paint_data.backgrounds.iter().chain(&paint_data.selections) {
        let left = origin.x + px(background.start_col as f32 * cell_width);
        let right =
            origin.x + px((background.start_col + background.cell_count) as f32 * cell_width);
        let left = snap_down(left);
        let background_bounds = Bounds::new(
            point(left, origin.y + px(background.row as f32 * cell_height)),
            size(snap_up(right) - left, px(cell_height)),
        );
        window.paint_quad(fill(background_bounds, background.color));
    }

    let cursor_bounds = paint_data.cursor.map(|cursor| {
        Bounds::new(
            point(
                origin.x + px(cursor.start_col as f32 * cell_width),
                origin.y + px(cursor.row as f32 * cell_height),
            ),
            size(px(cursor.cell_count as f32 * cell_width), px(cell_height)),
        )
    });
    if show_cursor
        && let Some(cursor) = paint_data.cursor.filter(|cursor| cursor.visible)
        && let Some(cursor_bounds) = cursor_bounds
    {
        match (cursor.focused, cursor.shape) {
            (false, _) => {
                let t = px(1.);
                window.paint_quad(fill(
                    Bounds::new(cursor_bounds.origin, size(cursor_bounds.size.width, t)),
                    cursor.color,
                ));
                window.paint_quad(fill(
                    Bounds::new(
                        point(cursor_bounds.left(), cursor_bounds.bottom() - t),
                        size(cursor_bounds.size.width, t),
                    ),
                    cursor.color,
                ));
                window.paint_quad(fill(
                    Bounds::new(cursor_bounds.origin, size(t, cursor_bounds.size.height)),
                    cursor.color,
                ));
                window.paint_quad(fill(
                    Bounds::new(
                        point(cursor_bounds.right() - t, cursor_bounds.top()),
                        size(t, cursor_bounds.size.height),
                    ),
                    cursor.color,
                ));
            }
            (_, CursorShape::Beam) => window.paint_quad(fill(
                Bounds::new(
                    cursor_bounds.origin,
                    size(px(2.), cursor_bounds.size.height),
                ),
                cursor.color,
            )),
            (_, CursorShape::Underline) => window.paint_quad(fill(
                Bounds::new(
                    point(cursor_bounds.left(), cursor_bounds.bottom() - px(2.)),
                    size(cursor_bounds.size.width, px(2.)),
                ),
                cursor.color,
            )),
            (_, CursorShape::Block) => window.paint_quad(fill(cursor_bounds, cursor.color)),
            (_, CursorShape::Hidden) => {}
        }
    }

    for run in &paint_data.text_runs {
        let mut run_font = terminal_font();
        run_font.weight = if run.style.bold {
            FontWeight::BOLD
        } else {
            FontWeight::NORMAL
        };
        run_font.style = if run.style.italic {
            FontStyle::Italic
        } else {
            FontStyle::Normal
        };
        let foreground = if run.style.cursor {
            terminal_color(run.style.bg, palette)
        } else {
            terminal_color(run.style.fg, palette)
        };
        let text_run = TextRun {
            len: run.text.len(),
            font: run_font,
            color: foreground,
            background_color: None,
            strikethrough: None,
            underline: run.style.underline.then_some(UnderlineStyle {
                thickness: px(1.),
                color: Some(foreground),
                wavy: run.style.underline_wavy,
            }),
        };
        let shaped = window.text_system().shape_line(
            run.text.clone().into(),
            px(TERMINAL_FONT_SIZE),
            &[text_run],
            Some(px(cell_width)),
        );
        let position = point(
            origin.x + px(run.start_col as f32 * cell_width),
            origin.y + px(run.row as f32 * cell_height),
        );
        let _ = shaped.paint(position, px(cell_height), TextAlign::Left, None, window, cx);
    }

    cursor_bounds
}

/// Lay out a whole grid with no row cache. The live drawer keeps a cache per
/// terminal; this is for one-shot renders of stored command output.
pub(crate) fn layout_grid(
    state: &TerminalModel,
    palette: TerminalPalette,
    composing: bool,
    hovered_link: Option<&HyperlinkMatch>,
    focused: bool,
    blink_phase: bool,
) -> GridPaintData {
    layout_grid_cached(
        &mut HashMap::new(),
        0,
        state,
        palette,
        composing.then_some(""),
        hovered_link,
        focused,
        blink_phase,
    )
}

#[allow(clippy::too_many_arguments)]
fn layout_grid_cached(
    caches: &mut HashMap<u64, TerminalGridCache>,
    terminal_id: u64,
    state: &TerminalModel,
    palette: TerminalPalette,
    marked_text: Option<&str>,
    hovered_link: Option<&HyperlinkMatch>,
    focused: bool,
    blink_phase: bool,
) -> GridPaintData {
    let composing = marked_text.is_some();
    let cursor = layout_cursor(state, palette);
    let cursor_cell = cursor.map(|cursor| (cursor.row, cursor.start_col));
    let grid_key = GridCacheKey {
        cols: state.cols(),
        screen_lines: state.rows(),
        display_offset: state.display_offset(),
        palette,
    };
    let cache = caches
        .entry(terminal_id)
        .or_insert_with(|| TerminalGridCache {
            key: grid_key,
            rows: vec![None; state.rows()],
        });
    if cache.key != grid_key || state.fully_damaged() {
        cache.key = grid_key;
        cache.rows = vec![None; state.rows()];
    }

    let mut paint = RowPaintData::default();

    for row in 0..state.rows() {
        let row_key = row_layout_key(
            state,
            row,
            cursor_cell,
            marked_text,
            hovered_link,
            focused,
            blink_phase,
        );
        let clean = !state.row_damaged(row);
        let cached = cache.rows[row]
            .as_ref()
            .filter(|cached| clean && cached.key == row_key)
            .map(|cached| cached.paint.clone());
        let row_paint = cached.unwrap_or_else(|| {
            let paint = layout_row(
                state,
                row,
                palette,
                composing,
                hovered_link,
                focused,
                blink_phase,
                cursor_cell,
            );
            cache.rows[row] = Some(CachedRowLayout {
                key: row_key,
                paint: paint.clone(),
            });
            paint
        });
        paint.text_runs.extend(row_paint.text_runs);
        paint.backgrounds.extend(row_paint.backgrounds);
        paint.selections.extend(row_paint.selections);
    }

    GridPaintData {
        text_runs: paint.text_runs,
        backgrounds: paint.backgrounds,
        selections: paint.selections,
        cursor: cursor.map(|mut cursor| {
            cursor.focused = focused;
            cursor.visible &= !composing && (!state.cursor_blinking() || blink_phase);
            cursor
        }),
    }
}

fn row_layout_key(
    state: &TerminalModel,
    row: usize,
    cursor_cell: Option<(usize, usize)>,
    marked_text: Option<&str>,
    hovered_link: Option<&HyperlinkMatch>,
    focused: bool,
    blink_phase: bool,
) -> RowLayoutKey {
    let mut selected = (0..state.cols()).filter(|col| state.is_selected(row, *col));
    let selection = selected
        .next()
        .map(|first| (first, selected.next_back().unwrap_or(first)));
    let hovered_link = hovered_link
        .filter(|link| row >= link.start.0 && row <= link.end.0)
        .map(|link| (link.start, link.end));
    let cursor = cursor_cell
        .filter(|(cursor_row, _)| *cursor_row == row)
        .map(|position| CursorRowKey {
            position,
            shape: state.cursor_shape(),
            blinking: state.cursor_blinking(),
            marked_text: marked_text.map(str::to_owned),
            focused,
            blink_phase,
        });
    RowLayoutKey {
        selection,
        hovered_link,
        cursor,
    }
}

#[allow(clippy::too_many_arguments)]
fn layout_row(
    state: &TerminalModel,
    row: usize,
    palette: TerminalPalette,
    composing: bool,
    hovered_link: Option<&HyperlinkMatch>,
    focused: bool,
    blink_phase: bool,
    cursor_cell: Option<(usize, usize)>,
) -> RowPaintData {
    let mut paint = RowPaintData::default();
    let mut previous_cell_had_extras = false;
    for col in 0..state.cols() {
        let Some(cell) = state.cell(row, col) else {
            break;
        };
        let style = state.style(cell);
        let flags = style.flags();
        let selected = state.is_selected(row, col);
        let (fg, bg) = cell_colors(style.fg, style.bg, flags);

        let background = if bg == TerminalColor::Background {
            None
        } else {
            Some(terminal_color(bg, palette))
        };
        if let Some(color) = background {
            push_background(&mut paint.backgrounds, row, col, color);
        }
        if selected {
            push_background(&mut paint.selections, row, col, palette.selection);
        }

        // A wide spacer still participates in backgrounds and hit-testing,
        // but never contributes a glyph to the shaped text.
        if cell.width == CellWidth::Spacer {
            continue;
        }

        // Alacritty stores emoji variation/modifier codepoints as extras;
        // its following placeholder space is not an independently painted
        // character. This mirrors Zed's terminal layout workaround.
        let blank = cell.text.is_empty();
        if blank && previous_cell_had_extras {
            previous_cell_had_extras = false;
            continue;
        }
        previous_cell_had_extras = cell.text.chars().nth(1).is_some();

        let underline = flags.intersects(CellFlags::ALL_UNDERLINES);
        if blank && !underline {
            continue;
        }

        let cursor_visible = !composing
            && !selected
            && cursor_cell == Some((row, col))
            && focused
            && state.cursor_shape() == CursorShape::Block
            && (!state.cursor_blinking() || blink_phase);
        let hyperlink_hovered =
            hovered_link.is_some_and(|link| (row, col) >= link.start && (row, col) <= link.end);
        let text = if blank {
            " ".to_string()
        } else {
            cell.text.clone()
        };
        let style = GridTextStyle {
            fg,
            bg,
            bold: flags.contains(CellFlags::BOLD),
            italic: flags.contains(CellFlags::ITALIC),
            underline: underline || hyperlink_hovered,
            underline_wavy: flags.contains(CellFlags::UNDERCURL),
            selected,
            cursor: cursor_visible,
        };

        if let Some(current) = paint.text_runs.last_mut()
            && current.row == row
            && current.start_col + current.cell_count == col
            && current.style == style
        {
            current.text.push_str(&text);
            current.cell_count += 1;
        } else {
            paint.text_runs.push(BatchedTextRun {
                row,
                start_col: col,
                text,
                cell_count: 1,
                style,
            });
        }
    }
    paint
}

fn push_background(backgrounds: &mut Vec<BackgroundRect>, row: usize, col: usize, color: Hsla) {
    if let Some(previous) = backgrounds.last_mut()
        && previous.row == row
        && previous.start_col + previous.cell_count == col
        && previous.color == color
    {
        previous.cell_count += 1;
    } else {
        backgrounds.push(BackgroundRect {
            row,
            start_col: col,
            cell_count: 1,
            color,
        });
    }
}

fn cell_colors(
    foreground: TerminalColor,
    background: TerminalColor,
    flags: CellFlags,
) -> (TerminalColor, TerminalColor) {
    let (mut fg, mut bg) = (foreground, background);
    if flags.contains(CellFlags::INVERSE) {
        std::mem::swap(&mut fg, &mut bg);
    }
    (fg, bg)
}

fn layout_cursor(state: &TerminalModel, palette: TerminalPalette) -> Option<CursorPaint> {
    let (row, col) = state.cursor()?;
    let cell = state.cell(row, col)?;
    let (start_col, cell_count, color_col) = if cell.width == CellWidth::Spacer && col > 0 {
        (col - 1, 2, col - 1)
    } else if cell.width == CellWidth::Wide {
        (col, 2, col)
    } else {
        (col, 1, col)
    };
    let style = state.style(state.cell(row, color_col)?);
    let (fg, _) = cell_colors(style.fg, style.bg, style.flags());
    Some(CursorPaint {
        row,
        start_col,
        cell_count,
        color: terminal_color(fg, palette).opacity(0.72),
        visible: !state.is_selected(row, color_col),
        shape: state.cursor_shape(),
        focused: true,
    })
}

struct TerminalInputHandler {
    drawer: Entity<TerminalDrawer>,
    terminal_id: u64,
    cursor_bounds: Option<Bounds<Pixels>>,
    cell_width: Pixels,
}

impl InputHandler for TerminalInputHandler {
    fn paste(&mut self, item: ClipboardItem, _window: &mut Window, cx: &mut App) {
        if let Some(text) = item.text() {
            let terminal_id = self.terminal_id;
            self.drawer.update(cx, |drawer, cx| {
                drawer.paste_to_terminal(terminal_id, &text, cx)
            });
        }
    }

    fn selected_text_range(
        &mut self,
        _ignore_disabled_input: bool,
        _window: &mut Window,
        _cx: &mut App,
    ) -> Option<UTF16Selection> {
        Some(UTF16Selection {
            range: 0..0,
            reversed: false,
        })
    }

    fn marked_text_range(&mut self, _window: &mut Window, cx: &mut App) -> Option<Range<usize>> {
        self.drawer
            .read(cx)
            .marked_text
            .as_ref()
            .filter(|marked| marked.terminal_id == self.terminal_id)
            .map(|marked| 0..marked.text.encode_utf16().count())
    }

    fn text_for_range(
        &mut self,
        _range_utf16: Range<usize>,
        _adjusted_range: &mut Option<Range<usize>>,
        _window: &mut Window,
        _cx: &mut App,
    ) -> Option<String> {
        None
    }

    fn replace_text_in_range(
        &mut self,
        _replacement_range: Option<Range<usize>>,
        text: &str,
        window: &mut Window,
        cx: &mut App,
    ) {
        let terminal_id = self.terminal_id;
        let text = text.to_string();
        self.drawer.update(cx, |drawer, cx| {
            drawer.marked_text = None;
            drawer.last_input = Instant::now();
            drawer.cursor_phase = true;
            drawer.bell_tabs.remove(&terminal_id);
            if !text.is_empty() {
                drawer.send_committed_text(terminal_id, &text, cx);
            }
            cx.notify();
        });
        window.invalidate_character_coordinates();
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        _range_utf16: Option<Range<usize>>,
        new_text: &str,
        _new_selected_range: Option<Range<usize>>,
        window: &mut Window,
        cx: &mut App,
    ) {
        let terminal_id = self.terminal_id;
        let marked_text = (!new_text.is_empty()).then(|| MarkedText {
            terminal_id,
            text: new_text.to_string(),
        });
        self.drawer.update(cx, |drawer, cx| {
            drawer.marked_text = marked_text;
            cx.notify();
        });
        window.invalidate_character_coordinates();
    }

    fn unmark_text(&mut self, window: &mut Window, cx: &mut App) {
        let terminal_id = self.terminal_id;
        self.drawer.update(cx, |drawer, cx| {
            if drawer
                .marked_text
                .as_ref()
                .is_some_and(|marked| marked.terminal_id == terminal_id)
            {
                drawer.marked_text = None;
                cx.notify();
            }
        });
        window.invalidate_character_coordinates();
    }

    fn bounds_for_range(
        &mut self,
        range_utf16: Range<usize>,
        _window: &mut Window,
        _cx: &mut App,
    ) -> Option<Bounds<Pixels>> {
        let mut bounds = self.cursor_bounds?;
        bounds.origin.x += self.cell_width * range_utf16.start as f32;
        Some(bounds)
    }

    fn character_index_for_point(
        &mut self,
        _point: Point<Pixels>,
        _window: &mut Window,
        _cx: &mut App,
    ) -> Option<usize> {
        None
    }

    fn apple_press_and_hold_enabled(&mut self) -> bool {
        false
    }
}

fn terminal_clipboard_shortcut(
    key: &str,
    modifiers: gpui::Modifiers,
    use_platform_modifier: bool,
) -> Option<ClipboardShortcut> {
    let expected_modifiers = if use_platform_modifier {
        modifiers.platform
            && !modifiers.control
            && !modifiers.alt
            && !modifiers.shift
            && !modifiers.function
    } else {
        modifiers.control
            && modifiers.shift
            && !modifiers.platform
            && !modifiers.alt
            && !modifiers.function
    };
    if !expected_modifiers {
        return None;
    }
    if key.eq_ignore_ascii_case("c") {
        Some(ClipboardShortcut::Copy)
    } else if key.eq_ignore_ascii_case("v") {
        Some(ClipboardShortcut::Paste)
    } else {
        None
    }
}

fn terminal_link_modifier(modifiers: gpui::Modifiers, use_platform_modifier: bool) -> bool {
    if use_platform_modifier {
        modifiers.platform
    } else {
        modifiers.control
    }
}

fn prepare_terminal_paste(text: &str, bracketed_paste: bool) -> String {
    if bracketed_paste {
        format!("\x1b[200~{}\x1b[201~", text.replace('\x1b', ""))
    } else {
        text.replace("\r\n", "\r").replace('\n', "\r")
    }
}

fn terminal_key_bytes(
    keystroke: &gpui::Keystroke,
    mode: Mode,
    keyboard_mode: tcode_protocol::terminal::KeyboardModes,
    modify_other_keys: Option<u8>,
) -> Option<Vec<u8>> {
    mappings::key_bytes(
        &keystroke.key,
        term_modifiers(keystroke.modifiers),
        mode,
        keyboard_mode,
        modify_other_keys,
        true,
    )
}

fn term_modifiers(modifiers: gpui::Modifiers) -> TermModifiers {
    TermModifiers {
        shift: modifiers.shift,
        alt: modifiers.alt,
        control: modifiers.control,
        platform: modifiers.platform,
    }
}

fn term_mouse_button(button: MouseButton) -> Option<TermMouseButton> {
    match button {
        MouseButton::Left => Some(TermMouseButton::Left),
        MouseButton::Middle => Some(TermMouseButton::Middle),
        MouseButton::Right => Some(TermMouseButton::Right),
        _ => None,
    }
}

fn shell_quote(path: &str) -> String {
    format!("'{}'", path.replace('\'', "'\\''"))
}

fn grid_point_and_side(
    x: f32,
    y: f32,
    cols: usize,
    rows: usize,
    cell_width: f32,
    cell_height: f32,
) -> ((usize, usize), SelectionSide) {
    let last_column = cols.saturating_sub(1);
    let mut column = (x / cell_width) as usize;
    let cell_x = x.max(0.) % cell_width;
    let mut side = if cell_x > cell_width / 2. {
        SelectionSide::Right
    } else {
        SelectionSide::Left
    };
    if column > last_column {
        column = last_column;
        side = SelectionSide::Right;
    }

    let bottommost_row = rows.saturating_sub(1) as i32;
    let mut row = (y / cell_height) as i32;
    if row > bottommost_row {
        row = bottommost_row;
        side = SelectionSide::Right;
    } else if y < 0. {
        side = SelectionSide::Left;
    }

    ((row.max(0) as usize, column.min(last_column)), side)
}

fn selection_drag_started(dx: f32, dy: f32) -> bool {
    dx.hypot(dy) > SELECTION_DRAG_THRESHOLD
}

fn drag_scroll_lines(y: Pixels, geometry: Option<GridGeometry>, cell_height: f32) -> Option<i32> {
    let geometry = geometry?;
    let top = geometry.bounds.top();
    let bottom = top + px(geometry.rows as f32 * geometry.cell_height);
    let pixels = if y < top {
        f32::from(top - y)
    } else if y > bottom {
        -f32::from(y - bottom)
    } else {
        return None;
    };
    let lines = (pixels.abs().powf(1.1) / cell_height).ceil() as i32;
    Some(lines.clamp(1, 3) * pixels.signum() as i32)
}

pub(crate) fn terminal_font() -> gpui::Font {
    let mut terminal_font = font(TERMINAL_FONT_FAMILY);
    terminal_font.features = FontFeatures::disable_ligatures();
    #[cfg(target_family = "wasm")]
    {
        terminal_font.fallbacks = Some(gpui::FontFallbacks::from_fonts(vec![
            "Tcode Terminal Symbols".into(),
        ]));
    }
    terminal_font
}

pub(crate) fn terminal_color(color: TerminalColor, palette: TerminalPalette) -> Hsla {
    match color {
        TerminalColor::Foreground | TerminalColor::Cursor => palette.foreground,
        TerminalColor::Background => palette.background,
        TerminalColor::Rgb { r, g, b } => {
            rgb((u32::from(r) << 16) | (u32::from(g) << 8) | u32::from(b)).into()
        }
        TerminalColor::Indexed(index) => {
            const ANSI: [u32; 16] = [
                0x1f2329, 0xe45649, 0x50a14f, 0xc18401, 0x4078f2, 0xa626a4, 0x0184bc, 0xabb2bf,
                0x5c6370, 0xff616e, 0x7bc275, 0xe5c07b, 0x61afef, 0xc678dd, 0x56b6c2, 0xffffff,
            ];
            if index < 16 {
                return rgb(ANSI[index as usize]).into();
            }
            if index < 232 {
                let n = index - 16;
                let component = |v: u8| if v == 0 { 0 } else { 55 + 40 * u32::from(v) };
                let r = component(n / 36);
                let g = component((n % 36) / 6);
                let b = component(n % 6);
                return rgb((r << 16) | (g << 8) | b).into();
            }
            let gray = 8 + 10 * u32::from(index - 232);
            rgb((gray << 16) | (gray << 8) | gray).into()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use tcode_protocol::terminal::{TerminalCell, TerminalFrame, TerminalRow, TerminalStyle};

    fn cell(ch: char, width: CellWidth) -> TerminalCell {
        TerminalCell {
            text: if ch == ' ' { String::new() } else { ch.into() },
            width,
            ..TerminalCell::default()
        }
    }

    fn model(cells: Vec<TerminalCell>) -> TerminalModel {
        model_with_styles(cells, vec![TerminalStyle::default()])
    }

    fn model_with_styles(cells: Vec<TerminalCell>, styles: Vec<TerminalStyle>) -> TerminalModel {
        let cols = cells.len();
        let mut model = TerminalModel::new(String::new());
        model.apply_frame(TerminalFrame {
            cols: cols as u16,
            rows: 1,
            styles,
            visible: vec![TerminalRow {
                cells,
                wrapped: false,
            }],
            ..TerminalFrame::default()
        });
        model
    }

    #[test]
    fn grid_point_maps_cell_halves_and_clamps_past_the_last_column() {
        for (x, col, side) in [
            (2., 0, SelectionSide::Left),
            (8., 0, SelectionSide::Right),
            (35., 2, SelectionSide::Right),
        ] {
            assert_eq!(grid_point_and_side(x, 5., 3, 2, 10., 20.), ((0, col), side));
        }
    }

    #[test]
    fn selection_drag_honors_click_kind_threshold_and_terminal_ownership() {
        let mut drag = SelectionDrag::default();
        assert_eq!(
            drag.on_down(
                7,
                ScreenPoint { x: 10., y: 10. },
                (0, 1),
                SelectionSide::Left,
                1,
                false
            ),
            SelectionDragAction::ClearAndWait
        );
        for (terminal, x, pressed) in [(7, 12., true), (8, 20., true), (7, 20., false)] {
            assert_eq!(
                drag.on_move(
                    terminal,
                    ScreenPoint { x, y: 10. },
                    (0, 2),
                    SelectionSide::Right,
                    pressed
                ),
                SelectionDragAction::None
            );
        }
        assert_eq!(
            drag.on_move(
                7,
                ScreenPoint { x: 12.1, y: 10. },
                (0, 2),
                SelectionSide::Right,
                true
            ),
            SelectionDragAction::StartSimpleAndUpdate {
                anchor: (0, 1),
                anchor_side: SelectionSide::Left,
                point: (0, 2),
                side: SelectionSide::Right
            }
        );
        assert_eq!(
            drag.on_move(
                7,
                ScreenPoint { x: 20., y: 40. },
                (3, 2),
                SelectionSide::Right,
                true
            ),
            SelectionDragAction::Update {
                point: (3, 2),
                side: SelectionSide::Right
            }
        );
        assert!(!drag.on_up(8));
        assert!(drag.on_up(7));
        assert_eq!(
            drag.on_move(
                7,
                ScreenPoint { x: 30., y: 40. },
                (3, 3),
                SelectionSide::Right,
                true
            ),
            SelectionDragAction::None
        );
        for (clicks, kind) in [(2, SelectionKind::Semantic), (3, SelectionKind::Lines)] {
            assert_eq!(
                drag.on_down(
                    7,
                    ScreenPoint { x: 0., y: 0. },
                    (2, 3),
                    SelectionSide::Left,
                    clicks,
                    false
                ),
                SelectionDragAction::Start {
                    kind,
                    point: (2, 3),
                    side: SelectionSide::Left
                }
            );
            drag.on_up(7);
        }
        assert_eq!(
            drag.on_down(
                7,
                ScreenPoint { x: 0., y: 0. },
                (2, 3),
                SelectionSide::Left,
                1,
                true
            ),
            SelectionDragAction::Update {
                point: (2, 3),
                side: SelectionSide::Left
            }
        );
    }

    #[test]
    fn shell_quotes_paths() {
        assert_eq!(shell_quote("/tmp/a"), "'/tmp/a'");
        assert_eq!(shell_quote("/tmp/a b"), "'/tmp/a b'");
        assert_eq!(shell_quote("a'b"), "'a'\\''b'");
    }

    #[test]
    fn clipboard_shortcuts_are_platform_specific() {
        let command = gpui::Modifiers {
            platform: true,
            ..Default::default()
        };
        let control_shift = gpui::Modifiers {
            control: true,
            shift: true,
            ..Default::default()
        };

        assert_eq!(
            terminal_clipboard_shortcut("c", command, true),
            Some(ClipboardShortcut::Copy)
        );
        assert_eq!(
            terminal_clipboard_shortcut("v", command, true),
            Some(ClipboardShortcut::Paste)
        );
        assert_eq!(terminal_clipboard_shortcut("c", control_shift, true), None);
        assert_eq!(terminal_clipboard_shortcut("c", command, false), None);
        assert_eq!(
            terminal_clipboard_shortcut("C", control_shift, false),
            Some(ClipboardShortcut::Copy)
        );
        assert_eq!(
            terminal_clipboard_shortcut("V", control_shift, false),
            Some(ClipboardShortcut::Paste)
        );
        assert_eq!(
            terminal_clipboard_shortcut(
                "c",
                gpui::Modifiers {
                    control: true,
                    ..Default::default()
                },
                false,
            ),
            None
        );
    }

    #[test]
    fn hyperlink_modifier_matches_the_platform_shortcut_convention() {
        let command = gpui::Modifiers {
            platform: true,
            ..Default::default()
        };
        let control = gpui::Modifiers {
            control: true,
            ..Default::default()
        };

        assert!(terminal_link_modifier(command, true));
        assert!(!terminal_link_modifier(control, true));
        assert!(terminal_link_modifier(control, false));
        assert!(!terminal_link_modifier(command, false));
    }

    #[test]
    fn terminal_paste_preparation_is_shared_by_clipboard_paths() {
        assert_eq!(prepare_terminal_paste("a\r\nb\nc", false), "a\rb\rc");
        assert_eq!(
            prepare_terminal_paste("a\x1bb", true),
            "\x1b[200~ab\x1b[201~"
        );
    }

    #[test]
    fn grid_paint_preserves_column_boundaries_and_style() {
        let mut undercurl = cell('x', CellWidth::Narrow);
        undercurl.style = 1;
        let palette = TerminalPalette {
            foreground: rgb(0xffffff).into(),
            background: rgb(0x000000).into(),
            selection: rgb(0x336699).into(),
        };
        for (state, expected, wavy) in [
            (
                model(vec![
                    cell('a', CellWidth::Narrow),
                    cell('中', CellWidth::Wide),
                    cell(' ', CellWidth::Spacer),
                    cell('b', CellWidth::Narrow),
                    cell('文', CellWidth::Wide),
                    cell(' ', CellWidth::Spacer),
                    cell('c', CellWidth::Narrow),
                ]),
                vec![(0, "a中", 2), (3, "b文", 2), (6, "c", 1)],
                false,
            ),
            (
                model(vec![cell('x', CellWidth::Narrow)]),
                vec![(0, "x", 1)],
                false,
            ),
            (
                model_with_styles(
                    vec![undercurl],
                    vec![
                        TerminalStyle::default(),
                        TerminalStyle {
                            flags: CellFlags::UNDERCURL.bits(),
                            ..TerminalStyle::default()
                        },
                    ],
                ),
                vec![(0, "x", 1)],
                true,
            ),
        ] {
            let paint = layout_grid(&state, palette, false, None, true, true);
            let boundaries = paint
                .text_runs
                .iter()
                .map(|run| (run.start_col, run.text.as_str(), run.cell_count))
                .collect::<Vec<_>>();
            assert_eq!(boundaries, expected);
            assert!(paint.selections.is_empty());
            assert!(paint.backgrounds.is_empty());
            for run in &paint.text_runs {
                assert_eq!(run.style.underline, wavy);
                assert_eq!(run.style.underline_wavy, wavy);
            }
        }
    }

    /// Clean deltas preserve painted text; replacement deltas update it.
    #[test]
    fn cached_grid_paint_tracks_clean_and_replaced_rows() {
        let palette = TerminalPalette {
            foreground: rgb(0xffffff).into(),
            background: rgb(0x000000).into(),
            selection: rgb(0x336699).into(),
        };
        let mut state = model(vec![cell('a', CellWidth::Narrow)]);
        let mut caches = HashMap::new();
        let first = layout_grid_cached(&mut caches, 7, &state, palette, None, None, true, true);
        assert_eq!(first.text_runs[0].text, "a");

        // A delta that changes nothing on this row preserves its painted text.
        state.clear_damage();
        state.apply_delta(&idle_delta(1, 1));
        let cached = layout_grid_cached(&mut caches, 7, &state, palette, None, None, true, true);
        assert_eq!(cached.text_runs[0].text, "a");

        state.clear_damage();
        let mut delta = idle_delta(1, 1);
        delta.styles = vec![TerminalStyle::default()];
        delta.rows_replaced = vec![tcode_protocol::terminal::TerminalRowUpdate {
            index: 0,
            row: TerminalRow {
                cells: vec![cell('b', CellWidth::Narrow)],
                wrapped: false,
            },
        }];
        state.apply_delta(&delta);
        let rebuilt = layout_grid_cached(&mut caches, 7, &state, palette, None, None, true, true);
        assert_eq!(rebuilt.text_runs[0].text, "b");
    }

    fn idle_delta(cols: u16, rows: u16) -> tcode_protocol::terminal::TerminalDelta {
        tcode_protocol::terminal::TerminalDelta {
            cols,
            rows,
            ..Default::default()
        }
    }

    #[test]
    fn native_key_routing_leaves_text_to_ime_and_encodes_terminal_controls() {
        for (key, expected) in [
            ("a", None),
            ("ctrl-space", Some(vec![0])),
            ("ctrl-c", Some(vec![3])),
            ("backspace", Some(vec![0x7f])),
        ] {
            assert_eq!(
                terminal_key_bytes(
                    &gpui::Keystroke::parse(key).unwrap(),
                    Mode::empty(),
                    tcode_protocol::terminal::KeyboardModes::NO_MODE,
                    None
                ),
                expected,
                "{key}"
            );
        }
    }
}
