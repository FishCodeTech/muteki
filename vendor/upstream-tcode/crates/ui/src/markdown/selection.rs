//! Text selection across one document: a chat transcript, or a standalone
//! Markdown view such as a pull request comment.
//!
//! gpui-base's window text selection serves the whole window, and that reach
//! is what a transcript cannot live with: every left press anywhere in the
//! window clears it and begins a gesture, one gesture spans every pane, and
//! a gesture ends only from a release that reaches the root in the bubble
//! phase, so a drop target or a click handler that stops the release leaves
//! it following the pointer. It also selects by screen geometry, so a table
//! is read row band by row band rather than cell by cell. Here a document
//! owns its selection: only a press on its own area begins one, the release
//! is heard in the capture phase, and the ends are positions in the
//! document's text, which order the way it reads.

use std::{cell::RefCell, ops::RangeInclusive, rc::Rc, time::Duration};

use gpui::{
    AnyElement, AnyWindowHandle, App, AppContext as _, Bounds, ClipboardItem, Context, Entity,
    EntityId, Half as _, Hitbox, HitboxBehavior, InteractiveElement as _, IntoElement,
    LongPressEvent, Modifiers, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent,
    ParentElement as _, Pixels, PlatformInput, Point, RenderOnce, ScrollDelta, ScrollWheelEvent,
    Styled as _, Task, TextLayout, TouchPhase, WeakEntity, Window, canvas, div, point, px,
};
use gpui_base::{GlobalState, SelectionEdge};

use super::{
    nodes::{BlockNode, TextPosition},
    state::MarkdownState,
};
use crate::touch_selection::{EditMenuItem, TouchSelectionLayout, TouchSelectionOverlay};

/// A position in one entry of a document. Entries are numbered in the order
/// the document shows them; a standalone view is entry 0 of its own.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct DocPoint {
    pub(crate) entry: u64,
    pub(crate) position: TextPosition,
}

/// Where a painted run of text starts in its document: the segment it is
/// part of and the byte offset of its first character there.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RunOrigin {
    pub(crate) path: Rc<[u32]>,
    pub(crate) line: u32,
    pub(crate) base: usize,
}

impl RunOrigin {
    pub(crate) fn new(path: Rc<[u32]>) -> Self {
        Self {
            path,
            line: 0,
            base: 0,
        }
    }

    pub(crate) fn line(mut self, line: u32) -> Self {
        self.line = line;
        self
    }

    pub(crate) fn offset(mut self, offset: usize) -> Self {
        self.base += offset;
        self
    }

    fn position(&self, offset: usize) -> TextPosition {
        TextPosition {
            path: self.path.to_vec(),
            line: self.line,
            offset: self.base + offset,
        }
    }
}

/// The numeric path of a render path such as `root-3-1`, the form a
/// [`TextPosition`] orders by.
pub(crate) fn render_path(path: &str) -> Rc<[u32]> {
    path.strip_prefix("root")
        .unwrap_or(path)
        .split('-')
        .filter_map(|part| part.parse().ok())
        .collect()
}

/// The parsed text of the entries a document holds in `range`, in order,
/// for copying text that is not on screen.
pub(crate) type EntryNodes = Rc<dyn Fn(RangeInclusive<u64>, &App) -> Vec<(u64, Rc<BlockNode>)>>;

/// What a double or triple click selects.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Unit {
    Word,
    Line,
}

#[derive(Default)]
struct Touch {
    active: bool,
    menu_open: bool,
    /// A finger is panning the text, and the menu steps aside until it lifts.
    panning: bool,
    dragging: Option<(SelectionEdge, Point<Pixels>)>,
}

/// One selection over the entries of a document.
pub struct SelectionDocument {
    anchor: Option<DocPoint>,
    focus: Option<DocPoint>,
    /// While the pointer is held, the end a drag keeps.
    gesture: Option<DocPoint>,
    entries: EntryNodes,
    frame: Rc<RefCell<Frame>>,
    touch: Touch,
    auto_scroll: Option<Task<()>>,
}

/// What the document painted last frame, which presses and drags hit-test.
#[derive(Default)]
struct Frame {
    window: Option<AnyWindowHandle>,
    viewport: Option<Bounds<Pixels>>,
    participants: Vec<Participant>,
    /// The carets at the selection's two ends, when they were painted.
    edges: (Option<Bounds<Pixels>>, Option<Bounds<Pixels>>),
}

struct Participant {
    entry: u64,
    bounds: Bounds<Pixels>,
    state: WeakEntity<MarkdownState>,
}

impl SelectionDocument {
    pub(crate) fn new(entries: EntryNodes) -> Self {
        Self {
            anchor: None,
            focus: None,
            gesture: None,
            entries,
            frame: Rc::default(),
            touch: Touch::default(),
            auto_scroll: None,
        }
    }

    /// The selection's ends in document order, when it selects anything.
    pub(crate) fn span(&self) -> Option<(&DocPoint, &DocPoint)> {
        let (anchor, focus) = (self.anchor.as_ref()?, self.focus.as_ref()?);
        match anchor.cmp(focus) {
            std::cmp::Ordering::Less => Some((anchor, focus)),
            std::cmp::Ordering::Greater => Some((focus, anchor)),
            std::cmp::Ordering::Equal => None,
        }
    }

    pub fn has_selection(&self) -> bool {
        self.span().is_some()
    }

    /// The selected text as it would be pasted, entries on their own lines.
    pub fn selected_text(&self, cx: &App) -> String {
        let Some((start, end)) = self.span() else {
            return String::new();
        };
        (self.entries)(start.entry..=end.entry, cx)
            .into_iter()
            .map(|(entry, node)| {
                let from = if entry == start.entry {
                    start.position.clone()
                } else {
                    TextPosition::start()
                };
                let to = if entry == end.entry {
                    end.position.clone()
                } else {
                    TextPosition::end()
                };
                node.text_between(&from, &to)
            })
            .filter(|text| !text.is_empty())
            .collect::<Vec<_>>()
            .join("\n")
    }

    pub fn clear(&mut self, cx: &mut Context<Self>) {
        if self.anchor.is_none() && !self.touch.active {
            return;
        }
        self.anchor = None;
        self.focus = None;
        self.gesture = None;
        self.touch = Touch::default();
        self.changed(cx);
    }

    /// Select everything entry `entry` holds.
    pub(crate) fn select_entry(&mut self, entry: u64, cx: &mut Context<Self>) {
        let Some((start, end)) = (self.entries)(entry..=entry, cx)
            .into_iter()
            .next()
            .and_then(|(_, node)| node.extent())
        else {
            return;
        };
        self.gesture = None;
        self.set(
            DocPoint {
                entry,
                position: start,
            },
            DocPoint {
                entry,
                position: end,
            },
            cx,
        );
    }

    /// Show the selection's touch handles and its edit menu, as a long press
    /// leaves them.
    pub(crate) fn keep_touch(&mut self) {
        self.touch.active = self.has_selection();
        self.touch.menu_open = true;
    }

    /// The part of the run at `origin`, `len` bytes long, that is selected.
    pub(crate) fn run_range(
        &self,
        entry: u64,
        origin: &RunOrigin,
        len: usize,
    ) -> Option<std::ops::Range<usize>> {
        let (start, end) = self.span()?;
        let run_start = DocPoint {
            entry,
            position: origin.position(0),
        };
        let run_end = DocPoint {
            entry,
            position: origin.position(len),
        };
        if *end <= run_start || *start >= run_end {
            return None;
        }
        let local = |point: &DocPoint, default: usize| {
            if point.entry == entry
                && *point.position.path == *origin.path
                && point.position.line == origin.line
            {
                point.position.offset.saturating_sub(origin.base).min(len)
            } else {
                default
            }
        };
        let range = local(start, 0)..local(end, len);
        (range.start < range.end).then_some(range)
    }

    /// Repaint every view that painted this document's text.
    fn changed(&self, cx: &mut Context<Self>) {
        let views: Vec<EntityId> = self
            .frame
            .borrow()
            .participants
            .iter()
            .map(|participant| participant.state.entity_id())
            .collect();
        let app: &mut App = cx;
        for view in views {
            app.notify(view);
        }
        cx.notify();
    }

    fn set(&mut self, anchor: DocPoint, focus: DocPoint, cx: &mut Context<Self>) {
        if self.anchor.as_ref() == Some(&anchor) && self.focus.as_ref() == Some(&focus) {
            return;
        }
        self.anchor = Some(anchor);
        self.focus = Some(focus);
        self.changed(cx);
    }

    /// The document position a selection end at `window_point` falls on:
    /// in the entry under it, or past the end of the entry above it when it
    /// is between entries.
    fn point_at(&self, window_point: Point<Pixels>, cx: &App) -> Option<DocPoint> {
        let frame = self.frame.borrow();
        let y = window_point.y;
        let on = frame
            .participants
            .iter()
            .find(|p| p.bounds.top() <= y && y < p.bounds.bottom());
        if let Some(participant) = on
            && let Some(state) = participant.state.upgrade()
            && let Some(position) = state.read(cx).position_at(window_point)
        {
            return Some(DocPoint {
                entry: participant.entry,
                position,
            });
        }
        let above = frame
            .participants
            .iter()
            .filter(|p| p.bounds.top() <= y)
            .max_by_key(|p| p.bounds.bottom().as_f32().to_bits());
        if let Some(above) = above {
            return Some(DocPoint {
                entry: above.entry,
                position: TextPosition::end(),
            });
        }
        frame
            .participants
            .iter()
            .min_by_key(|p| p.bounds.top().as_f32().to_bits())
            .map(|below| DocPoint {
                entry: below.entry,
                position: TextPosition::start(),
            })
    }

    /// The character under `window_point`, in the entry it lies on; none
    /// off the text.
    fn char_at(&self, window_point: Point<Pixels>, cx: &App) -> Option<DocPoint> {
        let frame = self.frame.borrow();
        frame.participants.iter().find_map(|participant| {
            let state = participant.state.upgrade()?;
            let position = state.read(cx).char_at(window_point)?;
            Some(DocPoint {
                entry: participant.entry,
                position,
            })
        })
    }

    fn segment_text(&self, point: &DocPoint, cx: &App) -> Option<String> {
        let (_, node) = (self.entries)(point.entry..=point.entry, cx)
            .into_iter()
            .next()?;
        node.segment_text(&point.position.path, point.position.line)
    }

    /// The edges of the `unit` around the character at `point`, the way
    /// gpui-base's multi-click finds them.
    fn unit_at(&self, unit: Unit, point: &DocPoint, cx: &App) -> Option<(DocPoint, DocPoint)> {
        let text = self.segment_text(point, cx)?;
        let range = match unit {
            Unit::Word => word_at(&text, point.position.offset, cx)?,
            Unit::Line => line_range(&text, point.position.offset),
        };
        let at = |offset| DocPoint {
            entry: point.entry,
            position: TextPosition {
                offset,
                ..point.position.clone()
            },
        };
        Some((at(range.start), at(range.end)))
    }

    /// A press: a caret at `point`, or with `extend` the selection from its
    /// anchor to `point`, which a drag then moves.
    fn press(&mut self, point: DocPoint, extend: bool, cx: &mut Context<Self>) {
        self.touch = Touch::default();
        let anchor = match &self.anchor {
            Some(anchor) if extend => anchor.clone(),
            _ => point.clone(),
        };
        self.gesture = Some(anchor.clone());
        self.set(anchor, point, cx);
    }

    /// A double or triple click: the word or line at `point`. Whether there
    /// was one.
    fn select_unit(&mut self, unit: Unit, point: &DocPoint, cx: &mut Context<Self>) -> bool {
        self.touch = Touch::default();
        self.gesture = None;
        let Some((start, end)) = self.unit_at(unit, point, cx) else {
            return false;
        };
        self.set(start, end, cx);
        true
    }

    fn drag(&mut self, point: DocPoint, cx: &mut Context<Self>) {
        if let Some(anchor) = self.gesture.clone() {
            self.set(anchor, point, cx);
        }
    }

    /// End the pointer's gesture. A click that selected nothing keeps its
    /// place, which a shift-click extends from.
    fn release(&mut self) {
        self.gesture = None;
        self.auto_scroll = None;
    }

    /// The view the selection starts in, which takes the focus so the
    /// keyboard's Copy and Select All reach this document.
    fn selection_view(&self) -> Option<Entity<MarkdownState>> {
        let (start, _) = self.span()?;
        self.frame
            .borrow()
            .participants
            .iter()
            .find(|p| p.entry == start.entry)
            .and_then(|p| p.state.upgrade())
    }

    /// The touch handles and menu to draw, when a long press made this
    /// selection.
    pub(crate) fn touch_layout(&self) -> Option<TouchSelectionLayout> {
        if !self.touch.active {
            return None;
        }
        let frame = self.frame.borrow();
        let (start, end) = frame.edges;
        let fallback = start.or(end)?;
        Some(TouchSelectionLayout {
            start: start.unwrap_or(fallback),
            end: end.unwrap_or(fallback),
            start_visible: start.is_some(),
            end_visible: end.is_some(),
            menu_open: self.touch.menu_open && self.touch.dragging.is_none() && !self.touch.panning,
            dragging: self.touch.dragging.map(|(edge, _)| edge),
        })
    }

    /// Record a painted run's caret at a selection end it holds.
    pub(crate) fn register_edge(
        &self,
        entry: u64,
        origin: &RunOrigin,
        range: &std::ops::Range<usize>,
        layout: &TextLayout,
        visible: Bounds<Pixels>,
    ) {
        if !self.touch.active {
            return;
        }
        let Some((start, end)) = self.span() else {
            return;
        };
        // An end clipped out of view gets no handle.
        let caret = |offset| {
            layout
                .position_for_index(offset)
                .map(|origin| Bounds::new(origin, gpui::size(px(0.), layout.line_height())))
                .filter(|caret| visible.contains(&caret.center()))
        };
        let mut frame = self.frame.borrow_mut();
        let point = |offset| DocPoint {
            entry,
            position: origin.position(offset),
        };
        if point(range.start) == *start {
            frame.edges.0 = caret(range.start);
        }
        if point(range.end) == *end {
            frame.edges.1 = caret(range.end);
        }
    }

    pub(crate) fn register_participant(
        &self,
        entry: u64,
        bounds: Bounds<Pixels>,
        state: WeakEntity<MarkdownState>,
    ) {
        let mut frame = self.frame.borrow_mut();
        frame
            .participants
            .retain(|p| p.state.entity_id() != state.entity_id());
        frame.participants.push(Participant {
            entry,
            bounds,
            state,
        });
    }

    fn begin_frame(&self, viewport: Bounds<Pixels>, window: AnyWindowHandle) {
        let mut frame = self.frame.borrow_mut();
        frame.participants.clear();
        frame.edges = (None, None);
        frame.viewport = Some(viewport);
        frame.window = Some(window);
    }

    fn touch_drag(
        &mut self,
        edge: SelectionEdge,
        phase: TouchPhase,
        position: Point<Pixels>,
        cx: &mut Context<Self>,
    ) {
        match phase {
            TouchPhase::Started => {
                let caret = match edge {
                    SelectionEdge::Start => self.frame.borrow().edges.0,
                    SelectionEdge::End => self.frame.borrow().edges.1,
                };
                let offset = caret.map_or(Point::default(), |caret| caret.center() - position);
                self.touch.dragging = Some((edge, offset));
                cx.notify();
            }
            TouchPhase::Moved => {
                let Some((_, offset)) = self.touch.dragging else {
                    return;
                };
                let Some(point) = self.point_at(position + offset, cx) else {
                    return;
                };
                let Some((start, end)) = self.span().map(|(s, e)| (s.clone(), e.clone())) else {
                    return;
                };
                match edge {
                    SelectionEdge::Start if point < end => self.set(end, point, cx),
                    SelectionEdge::End if point > start => self.set(start, point, cx),
                    _ => {}
                }
            }
            TouchPhase::Ended | TouchPhase::Cancelled => {
                self.touch.dragging = None;
                self.touch.menu_open = true;
                self.changed(cx);
            }
        }
    }
}

/// The byte range of the word at `offset` in `text`, as the platform
/// segments words: Chinese and other text written without spaces splits into
/// its words. Where the platform has no segmentation, gpui-base's rule.
fn word_at(text: &str, offset: usize, cx: &App) -> Option<std::ops::Range<usize>> {
    let offset = floor_boundary(text, offset);
    if offset >= text.len() {
        return None;
    }
    let segmenter = cx
        .try_global::<crate::remote::ClientAttachment>()
        .map_or_else(tcode_client::words::system, |attachment| {
            attachment.host().word_segmenter()
        });
    let Some(segmenter) = segmenter else {
        return word_range(text, offset);
    };
    let breaks = segmenter.breaks(text);
    let end = breaks.partition_point(|boundary| *boundary <= offset);
    Some(*breaks.get(end.checked_sub(1)?)?..*breaks.get(end)?)
}

/// The byte range of the word at `offset` in `text`, as gpui-base's
/// multi-click reads it: a run of ASCII, Latin or Cyrillic letters, digits
/// and underscores, a run of spaces, or one other character (each CJK
/// character is a word of its own). None past the end of the text.
fn word_range(text: &str, offset: usize) -> Option<std::ops::Range<usize>> {
    #[derive(Clone, Copy, PartialEq)]
    enum Class {
        Word,
        Space,
        Other,
    }
    let class = |c: char| {
        if c == '_'
            || c.is_ascii_alphanumeric()
            || matches!(c, '\u{00C0}'..='\u{024F}' | '\u{0400}'..='\u{04FF}' | '\u{1E00}'..='\u{1EFF}' | '\u{0300}'..='\u{036F}')
        {
            Class::Word
        } else if c.is_whitespace() && !matches!(c, '\n' | '\r') {
            Class::Space
        } else {
            Class::Other
        }
    };
    let offset = floor_boundary(text, offset);
    let at = text[offset..].chars().next()?;
    let kind = class(at);
    let joins = |c: char| kind != Class::Other && class(c) == kind;
    let start = text[..offset]
        .char_indices()
        .rev()
        .take_while(|(_, c)| joins(*c))
        .last()
        .map_or(offset, |(ix, _)| ix);
    let end = text[offset + at.len_utf8()..]
        .char_indices()
        .find(|(_, c)| !joins(*c))
        .map_or(text.len(), |(ix, _)| offset + at.len_utf8() + ix);
    Some(start..end)
}

/// The line of `text` holding `offset`.
fn line_range(text: &str, offset: usize) -> std::ops::Range<usize> {
    let offset = floor_boundary(text, offset);
    let start = text[..offset].rfind('\n').map_or(0, |newline| newline + 1);
    let end = text[offset..]
        .find('\n')
        .map_or(text.len(), |newline| offset + newline);
    start..end
}

fn floor_boundary(text: &str, offset: usize) -> usize {
    let mut offset = offset.min(text.len());
    while !text.is_char_boundary(offset) {
        offset -= 1;
    }
    offset
}

/// The surface a document's selection is made on: a press on it begins a
/// selection, and while one is being made the pointer is followed to the
/// release wherever the window delivers it.
#[derive(IntoElement)]
pub struct SelectionArea {
    document: Entity<SelectionDocument>,
    child: AnyElement,
    base: gpui::Div,
}

impl SelectionArea {
    pub fn new(document: &Entity<SelectionDocument>, child: impl IntoElement) -> Self {
        Self {
            document: document.clone(),
            child: child.into_any_element(),
            base: div(),
        }
    }
}

impl gpui::Styled for SelectionArea {
    fn style(&mut self) -> &mut gpui::StyleRefinement {
        self.base.style()
    }
}

impl RenderOnce for SelectionArea {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let document = self.document;
        let listener = {
            let document = document.clone();
            let prepaint_document = document.clone();
            canvas(
                move |bounds, window, cx| {
                    // Before the content, whose views register what they
                    // paint into this frame, and whose blocking hitboxes (a
                    // copy button, a menu) then keep presses for themselves.
                    // The clip the text scrolls under: a standalone view's own
                    // bounds are one message, not the page that scrolls it.
                    let viewport = window.content_mask().bounds;
                    prepaint_document
                        .read(cx)
                        .begin_frame(viewport, window.window_handle());
                    window.insert_hitbox(bounds, HitboxBehavior::Normal)
                },
                move |_, hitbox, window, _| listen(&document, hitbox, window),
            )
            .absolute()
            .size_full()
        };
        let touch = {
            let source = document.clone();
            let drag = document.clone();
            let copy = document.clone();
            let select_all = document.clone();
            TouchSelectionOverlay::new(
                (
                    "document-touch-selection",
                    document.entity_id().as_u64() as usize,
                ),
                move |_, cx| source.read(cx).touch_layout(),
            )
            .handles(move |edge, phase, position, _, cx| {
                drag.update(cx, |document, cx| {
                    document.touch_drag(edge, phase, position, cx)
                });
            })
            .items([
                EditMenuItem::new(
                    "copy",
                    crate::tr!("edit_menu.copy").into_owned(),
                    move |_, cx| {
                        let text = copy.read(cx).selected_text(cx).trim().to_string();
                        if !text.is_empty() {
                            cx.write_to_clipboard(ClipboardItem::new_string(text));
                        }
                        copy.update(cx, |document, cx| {
                            document.touch.menu_open = false;
                            cx.notify();
                        });
                    },
                ),
                EditMenuItem::new(
                    "select-all",
                    crate::tr!("edit_menu.select_all").into_owned(),
                    move |_, cx| {
                        select_all.update(cx, |document, cx| {
                            if let Some(entry) = document.span().map(|(start, _)| start.entry) {
                                document.select_entry(entry, cx);
                                document.keep_touch();
                            }
                        });
                    },
                ),
            ])
            .into_elements(window, cx)
        };
        self.base
            .relative()
            .on_action({
                let document = document.clone();
                move |_: &gpui_base::input::Copy, _, cx| {
                    let text = document.read(cx).selected_text(cx).trim().to_string();
                    if text.is_empty() {
                        cx.propagate();
                        return;
                    }
                    cx.write_to_clipboard(ClipboardItem::new_string(text));
                }
            })
            .child(listener)
            .child(self.child)
            .children(touch)
    }
}

/// The press, drag, release and long press of one document's selection.
fn listen(document: &Entity<SelectionDocument>, hitbox: Hitbox, window: &mut Window) {
    window.on_mouse_event({
        let document = document.clone();
        // In the capture phase: a scroller stretched past its end keeps the
        // whole stream, the lift included.
        move |event: &ScrollWheelEvent, phase, _, cx| {
            if !phase.capture() || !document.read(cx).touch.active {
                return;
            }
            let panning = matches!(event.touch_phase, TouchPhase::Started | TouchPhase::Moved);
            document.update(cx, |document, cx| {
                if document.touch.panning != panning {
                    document.touch.panning = panning;
                    document.changed(cx);
                }
            });
        }
    });
    window.on_mouse_event({
        let document = document.clone();
        let hitbox = hitbox.clone();
        move |event: &MouseDownEvent, phase, window, cx| {
            if !phase.bubble() || event.button != MouseButton::Left || !hitbox.is_hovered(window) {
                return;
            }
            GlobalState::init(cx);
            // A finger pans; a tap only puts away what a long press selected.
            if GlobalState::is_touch_press(cx) {
                document.update(cx, |document, cx| document.clear(cx));
                return;
            }
            if event.click_count > 1 {
                let unit = if event.click_count == 2 {
                    Unit::Word
                } else {
                    Unit::Line
                };
                if let Some(point) = document.read(cx).char_at(event.position, cx) {
                    document.update(cx, |document, cx| document.select_unit(unit, &point, cx));
                    focus_selection(&document, window, cx);
                }
                return;
            }
            let Some(point) = document.read(cx).point_at(event.position, cx) else {
                document.update(cx, |document, cx| document.clear(cx));
                return;
            };
            document.update(cx, |document, cx| {
                document.press(point, event.modifiers.shift, cx)
            });
        }
    });
    window.on_mouse_event({
        let document = document.clone();
        move |event: &MouseMoveEvent, phase, window, cx| {
            if !phase.capture() || document.read(cx).gesture.is_none() {
                return;
            }
            if event.pressed_button != Some(MouseButton::Left) || cx.has_active_drag() {
                document.update(cx, |document, _| document.release());
                return;
            }
            follow(&document, event.position, window, cx);
        }
    });
    window.on_mouse_event({
        let document = document.clone();
        move |event: &MouseUpEvent, phase, window, cx| {
            if !phase.capture()
                || event.button != MouseButton::Left
                || document.read(cx).gesture.is_none()
            {
                return;
            }
            document.update(cx, |document, _| document.release());
            focus_selection(&document, window, cx);
        }
    });
    window.on_mouse_event({
        let document = document.clone();
        move |event: &LongPressEvent, phase, window, cx| {
            let started = event.phase == TouchPhase::Started;
            // A long press elsewhere selects there instead; heard before a
            // field that takes the press for itself.
            if phase.capture() {
                if started && !hitbox.is_hovered(window) {
                    document.update(cx, |document, cx| document.clear(cx));
                }
                return;
            }
            // On the text, it selects unless what lies over the text took it.
            if started {
                if window.default_prevented() || !hitbox.is_hovered(window) {
                    return;
                }
                let Some(point) = document.read(cx).char_at(event.start_position, cx) else {
                    return;
                };
                // The finger then moves the far end, from the word's start.
                document.update(cx, |document, cx| {
                    if document.select_unit(Unit::Word, &point, cx) {
                        document.gesture = document.span().map(|(start, _)| start.clone());
                    }
                    document.touch.active = document.has_selection();
                });
                if document.read(cx).touch.active {
                    // The window keeps a capture only from a press whose
                    // default was taken.
                    window.capture_long_press(&document);
                    window.prevent_default();
                    cx.stop_propagation();
                    crate::haptics::long_press();
                }
                return;
            }
            if !window.has_long_press_capture(&document) {
                return;
            }
            match event.phase {
                TouchPhase::Moved => {
                    if let Some(point) = document.read(cx).point_at(event.position, cx) {
                        document.update(cx, |document, cx| document.drag(point, cx));
                    }
                }
                _ => {
                    document.update(cx, |document, cx| {
                        document.gesture = None;
                        document.touch.menu_open = true;
                        document.changed(cx);
                    });
                    focus_selection(&document, window, cx);
                }
            }
        }
    });
}

fn focus_selection(document: &Entity<SelectionDocument>, window: &mut Window, cx: &mut App) {
    if let Some(view) = document.read(cx).selection_view() {
        let focus = view.read(cx).focus_handle.clone();
        focus.focus(window, cx);
    }
}

/// Move the selection's moving end to the pointer, and scroll the document
/// while the pointer holds near an edge of its viewport.
fn follow(
    document: &Entity<SelectionDocument>,
    position: Point<Pixels>,
    window: &mut Window,
    cx: &mut App,
) {
    if let Some(point) = document.read(cx).point_at(position, cx) {
        document.update(cx, |document, cx| document.drag(point, cx));
    }
    let viewport = document.read(cx).frame.borrow().viewport;
    let scrolling = viewport
        .and_then(|viewport| gpui_base::AutoScroll::compute_delta(position.y, viewport))
        .is_some();
    let running = document.read(cx).auto_scroll.is_some();
    if scrolling && !running {
        let task = auto_scroll(document.downgrade(), window, cx);
        document.update(cx, |document, _| document.auto_scroll = Some(task));
    }
}

fn auto_scroll(document: WeakEntity<SelectionDocument>, _: &Window, cx: &mut App) -> Task<()> {
    cx.spawn(async move |cx| {
        loop {
            cx.background_executor()
                .timer(Duration::from_millis(16))
                .await;
            let frame = document
                .read_with(cx, |document, _| {
                    let frame = document.frame.borrow();
                    Some((frame.window?, frame.viewport?, document.gesture.is_some()))
                })
                .ok()
                .flatten();
            let Some((handle, viewport, true)) = frame else {
                break;
            };
            let document = document.clone();
            let stopped = cx
                .update_window(handle, |_, window, cx| {
                    let pointer = window.mouse_position();
                    let Some(delta) = gpui_base::AutoScroll::compute_delta(pointer.y, viewport)
                    else {
                        return true;
                    };
                    window.dispatch_event(
                        PlatformInput::ScrollWheel(ScrollWheelEvent {
                            position: clamp_into(pointer, viewport),
                            delta: ScrollDelta::Pixels(point(px(0.), -delta)),
                            modifiers: Modifiers::default(),
                            ..Default::default()
                        }),
                        cx,
                    );
                    if let Some(document) = document.upgrade()
                        && let Some(point) = document.read(cx).point_at(pointer, cx)
                    {
                        document.update(cx, |document, cx| document.drag(point, cx));
                    }
                    false
                })
                .unwrap_or(true);
            if stopped {
                let _ = document.update(cx, |document, _| document.auto_scroll = None);
                break;
            }
        }
    })
}

fn clamp_into(pointer: Point<Pixels>, bounds: Bounds<Pixels>) -> Point<Pixels> {
    point(
        pointer
            .x
            .max(bounds.left() + px(1.))
            .min(bounds.right() - px(1.)),
        pointer
            .y
            .max(bounds.top() + px(1.))
            .min(bounds.bottom() - px(1.)),
    )
}

/// The character boundary in a run nearest to `window_point`, on the row at
/// its height.
///
/// `TextLayout::index_for_position` returns the start of the character under
/// the point; a point on a character's right half ends after it.
pub(crate) fn nearest_boundary(
    text: &str,
    layout: &TextLayout,
    window_point: Point<Pixels>,
) -> usize {
    let offset = match layout.index_for_position(window_point) {
        Ok(offset) => offset,
        Err(nearest) => return nearest.min(text.len()),
    };
    let Some(next_offset) = text
        .get(offset..)
        .and_then(|rest| rest.chars().next())
        .map(|character| offset + character.len_utf8())
    else {
        return offset.min(text.len());
    };
    let Some(position) = layout.position_for_index(offset) else {
        return offset;
    };
    let width = layout
        .position_for_index(next_offset)
        .filter(|next| next.y == position.y)
        .map_or_else(|| layout.line_height().half(), |next| next.x - position.x);
    if window_point.x >= position.x + width.half() {
        next_offset
    } else {
        offset
    }
}

/// The document a standalone selectable view selects in on its own.
pub(crate) fn own_document(
    state: WeakEntity<MarkdownState>,
    cx: &mut App,
) -> Entity<SelectionDocument> {
    cx.new(|_| {
        SelectionDocument::new(Rc::new(move |range, cx| {
            if !range.contains(&0) {
                return Vec::new();
            }
            state
                .upgrade()
                .map(|state| vec![(0, state.read(cx).parsed_root())])
                .unwrap_or_default()
        }))
    })
}

/// Which document a view's text belongs to, and as which entry.
#[derive(Clone)]
pub(crate) struct Membership {
    pub(crate) document: Entity<SelectionDocument>,
    pub(crate) entry: u64,
}

#[cfg(test)]
mod tests {
    use std::cell::{OnceCell, RefCell};

    use gpui::{Context, Render, TestAppContext, VisualTestContext, size};

    use super::*;
    use crate::{
        markdown::{MarkdownView, state::MarkdownState},
        widgets::input::{Textarea, TextareaState},
    };

    #[test]
    fn a_double_click_takes_a_run_of_letters_or_spaces_or_one_other_character() {
        let text = "let foo_bar = baz(1);";
        let word = |text: &'static str, offset| word_range(text, offset).map(|r| &text[r]);
        assert_eq!(word(text, 5), Some("foo_bar"));
        assert_eq!(word(text, 3), Some(" "));
        assert_eq!(word(text, 17), Some("("));
        assert_eq!(word(text, 19), Some(")"));
        assert_eq!(word(text, text.len()), None);
        assert_eq!(word("选中文字", 3), Some("中"));
    }

    fn draw(cx: &mut VisualTestContext) {
        cx.run_until_parked();
        cx.update(|window, cx| _ = window.draw(cx));
    }

    fn tap(cx: &mut VisualTestContext, selector: &'static str) {
        let bounds = cx
            .debug_bounds(selector)
            .unwrap_or_else(|| panic!("{selector} is shown"));
        cx.simulate_click(bounds.center(), Modifiers::default());
        draw(cx);
    }

    fn long_press(cx: &mut VisualTestContext, position: Point<Pixels>) {
        for phase in [TouchPhase::Started, TouchPhase::Ended] {
            cx.simulate_event(LongPressEvent {
                phase,
                start_position: position,
                position,
            });
            draw(cx);
        }
    }

    fn document_of(markdown: &Entity<MarkdownState>, cx: &App) -> Entity<SelectionDocument> {
        markdown
            .read(cx)
            .selection
            .as_ref()
            .unwrap()
            .document
            .clone()
    }

    fn menu_open(markdown: &Entity<MarkdownState>, cx: &mut VisualTestContext) -> Option<bool> {
        cx.update(|_, cx| {
            document_of(markdown, cx)
                .read(cx)
                .touch_layout()
                .map(|layout| layout.menu_open)
        })
    }

    fn selected(markdown: &Entity<MarkdownState>, cx: &mut VisualTestContext) -> String {
        cx.update(|_, cx| document_of(markdown, cx).read(cx).selected_text(cx))
    }

    struct Body {
        markdown: Entity<MarkdownState>,
        wheel_deltas: Rc<RefCell<Vec<Pixels>>>,
    }

    impl Render for Body {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            let wheel_deltas = self.wheel_deltas.clone();
            div()
                .size_full()
                .child(MarkdownView::new(&self.markdown).selectable(true))
                .on_scroll_wheel(move |event, window, _| {
                    wheel_deltas
                        .borrow_mut()
                        .push(event.delta.pixel_delta(window.line_height()).y);
                })
        }
    }

    fn open_body<'a>(
        text: &str,
        cx: &'a mut TestAppContext,
    ) -> (
        Entity<Body>,
        Entity<MarkdownState>,
        &'a mut VisualTestContext,
    ) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        cx.update(crate::theme::init);
        cx.update(crate::markdown::init);
        let text = text.to_string();
        let body = Rc::new(OnceCell::new());
        let (_, cx) = cx.add_window_view({
            let body = body.clone();
            move |window, cx| {
                let markdown = cx.new(|cx| MarkdownState::new(&text, cx));
                let view = cx.new(|_| Body {
                    markdown,
                    wheel_deltas: Rc::default(),
                });
                body.set(view.clone()).ok().unwrap();
                gpui_base::Root::new(view, window, cx)
            }
        });
        let body = body.get().unwrap().clone();
        let markdown = body.read_with(cx, |body, _| body.markdown.clone());
        (body, markdown, cx)
    }

    #[gpui::test]
    fn a_drag_scrolls_at_the_viewport_edge_not_the_message_edge(cx: &mut TestAppContext) {
        let (body, _, cx) = open_body("selectable text", cx);
        draw(cx);
        let bottom = cx.update(|window, _| window.viewport_size().height);
        let deltas = |cx: &mut VisualTestContext| {
            body.read_with(cx, |body, _| body.wheel_deltas.borrow().clone())
        };

        cx.simulate_mouse_down(
            point(px(5.), px(10.)),
            MouseButton::Left,
            Modifiers::default(),
        );
        draw(cx);
        // Mid-window, well past the one-line message's bottom edge.
        cx.simulate_mouse_move(
            point(px(5.), bottom / 2.),
            Some(MouseButton::Left),
            Modifiers::default(),
        );
        draw(cx);
        cx.executor().advance_clock(Duration::from_millis(48));
        cx.run_until_parked();
        assert_eq!(deltas(cx), Vec::<Pixels>::new());

        // Inside the viewport's bottom edge, the document scrolls down.
        cx.simulate_mouse_move(
            point(px(5.), bottom - px(8.)),
            Some(MouseButton::Left),
            Modifiers::default(),
        );
        draw(cx);
        cx.executor().advance_clock(Duration::from_millis(48));
        cx.run_until_parked();
        let scrolled = deltas(cx);
        assert!(
            !scrolled.is_empty() && scrolled.iter().all(|delta| *delta < px(0.)),
            "expected downward auto-scroll, got {scrolled:?}"
        );

        // The release stops it.
        cx.simulate_mouse_up(
            point(px(5.), bottom - px(8.)),
            MouseButton::Left,
            Modifiers::default(),
        );
        draw(cx);
        let settled = deltas(cx).len();
        cx.executor().advance_clock(Duration::from_millis(64));
        cx.run_until_parked();
        assert_eq!(deltas(cx).len(), settled);
    }

    #[gpui::test]
    fn text_wrapping_after_an_inline_image_paints_on_its_own_rows(cx: &mut TestAppContext) {
        // excalidraw#8530's body as GitHub stores it: one paragraph broken by
        // CRLFs, too long a link for a phone row.
        let (_, markdown, cx) = open_body(
            "**Technical parts**\r\n![glyphs](https://example.invalid/glyphs.png)\r\n\
             https://link.excalidraw.com/readonly/MbbnWPSWXgadXdtmzgeO",
            cx,
        );
        cx.simulate_resize(size(px(393.), px(852.)));
        draw(cx);
        let runs = markdown.read_with(cx, |state, _| {
            state
                .runs
                .borrow()
                .iter()
                .map(|run| run.bounds)
                .collect::<Vec<_>>()
        });
        assert!(runs.len() > 2, "the link must wrap: {runs:?}");
        for (ix, run) in runs.iter().enumerate() {
            for other in &runs[ix + 1..] {
                let overlap = run.intersect(other);
                assert!(
                    overlap.size.width <= px(0.5) || overlap.size.height <= px(0.5),
                    "{run:?} and {other:?} paint over each other in {runs:?}"
                );
            }
        }
    }

    #[gpui::test]
    fn long_press_on_a_message_offers_copy_and_select_all_past_the_viewport(
        cx: &mut TestAppContext,
    ) {
        let paragraphs = (0..40)
            .map(|ix| format!("Paragraph {ix} of the message."))
            .collect::<Vec<_>>();
        let text = paragraphs.join("\n\n")
            + "\n\n```text\nfirst line\nsecond line\n```\n\n| name | value |\n| --- | --- |\n| alpha | beta |";
        let expected =
            paragraphs.join("\n") + "\nfirst line\nsecond line\nname\tvalue\nalpha\tbeta";
        let (_, markdown, cx) = open_body(&text, cx);
        // A window far shorter than the message: most of its blocks are
        // outside the viewport, and the virtualized list never paints them.
        cx.simulate_resize(size(px(320.), px(120.)));
        draw(cx);
        assert!(cx.debug_bounds("markdown-block-0").is_some());
        assert!(cx.debug_bounds("markdown-block-39").is_none());

        long_press(cx, point(px(12.), px(8.)));
        assert_eq!(selected(&markdown, cx), "Paragraph");
        // Read-only text offers Copy and Select All, nothing that edits.
        assert!(cx.debug_bounds("edit-menu-copy").is_some());
        assert!(cx.debug_bounds("edit-menu-select-all").is_some());
        assert!(cx.debug_bounds("edit-menu-cut").is_none());
        assert!(cx.debug_bounds("edit-menu-paste").is_none());
        let menu = cx.debug_bounds("edit-menu").unwrap();
        let window_bounds = Bounds::new(point(px(0.), px(0.)), size(px(320.), px(120.)));
        assert!(window_bounds.contains(&menu.origin));
        assert!(window_bounds.contains(&menu.bottom_right()));

        // Select All takes the whole message, the blocks never laid out
        // included, and keeps the menu.
        tap(cx, "edit-menu-select-all");
        assert_eq!(selected(&markdown, cx), expected);
        assert!(cx.debug_bounds("edit-menu-copy").is_some());

        // Copy closes the menu; the selection stays.
        tap(cx, "edit-menu-copy");
        assert_eq!(
            cx.update(|_, cx| cx.read_from_clipboard().and_then(|item| item.text())),
            Some(expected)
        );
        assert!(cx.debug_bounds("edit-menu").is_none());
        assert_eq!(menu_open(&markdown, cx), Some(false));

        // A tap on the text puts the selection away, and the menu with it.
        cx.simulate_click(point(px(200.), px(8.)), Modifiers::default());
        draw(cx);
        assert_eq!(menu_open(&markdown, cx), None);
        assert!(cx.debug_bounds("edit-menu").is_none());
    }

    #[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
    #[gpui::test]
    fn a_double_click_on_chinese_takes_the_word_the_platform_segments(cx: &mut TestAppContext) {
        let (_, markdown, cx) = open_body("我们修复问题", cx);
        draw(cx);
        // The right half of "修", the first character of "修复".
        let position = markdown.read_with(cx, |state, _| {
            let runs = state.runs.borrow();
            let run = &runs[0];
            let start = run.layout.position_for_index(6).unwrap();
            let end = run.layout.position_for_index(9).unwrap();
            point(start.x + (end.x - start.x) * 0.75, start.y + px(4.))
        });
        for click_count in [1, 2] {
            cx.simulate_event(MouseDownEvent {
                button: MouseButton::Left,
                position,
                modifiers: Modifiers::default(),
                click_count,
                first_mouse: false,
            });
            cx.simulate_event(MouseUpEvent {
                button: MouseButton::Left,
                position,
                modifiers: Modifiers::default(),
                click_count,
            });
        }
        draw(cx);
        assert_eq!(selected(&markdown, cx), "修复");
    }

    /// A message above a composer.
    struct Page {
        markdown: Entity<MarkdownState>,
        textarea: Entity<TextareaState>,
    }

    impl Render for Page {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div()
                .size_full()
                .child(
                    div()
                        .h(px(60.))
                        .child(MarkdownView::new(&self.markdown).selectable(true)),
                )
                .child(div().h(px(100.)))
                .child(div().h(px(60.)).child(Textarea::new(&self.textarea)))
        }
    }

    #[gpui::test]
    fn one_touch_selection_at_a_time_and_the_menu_steps_aside_for_a_pan(cx: &mut TestAppContext) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        cx.update(crate::theme::init);
        cx.update(crate::markdown::init);
        let page = Rc::new(OnceCell::new());
        let (_, cx) = cx.add_window_view({
            let page = page.clone();
            move |window, cx| {
                let textarea =
                    cx.new(|cx| TextareaState::new(window, cx).default_value("draft text"));
                let markdown = cx.new(|cx| MarkdownState::new("quick select value", cx));
                let view = cx.new(|_| Page { markdown, textarea });
                page.set(view.clone()).ok().unwrap();
                gpui_base::Root::new(view, window, cx)
            }
        });
        let (markdown, textarea) = page
            .get()
            .unwrap()
            .read_with(cx, |page, _| (page.markdown.clone(), page.textarea.clone()));
        cx.simulate_resize(size(px(320.), px(400.)));
        draw(cx);

        long_press(cx, point(px(12.), px(8.)));
        assert_eq!(menu_open(&markdown, cx), Some(true));

        // A finger scrolling the page moves the menu out of the way until it
        // lifts; the selection and its handles stay.
        let pan = point(px(200.), px(100.));
        for (phase, open) in [
            (TouchPhase::Started, false),
            (TouchPhase::Moved, false),
            (TouchPhase::Ended, true),
        ] {
            cx.simulate_event(ScrollWheelEvent {
                position: pan,
                delta: ScrollDelta::Pixels(point(px(0.), px(-20.))),
                modifiers: Modifiers::default(),
                touch_phase: phase,
            });
            draw(cx);
            assert_eq!(menu_open(&markdown, cx), Some(open), "{phase:?}");
        }
        assert!(cx.debug_bounds("edit-menu-copy").is_some());
        assert!(cx.debug_bounds("edit-menu-cut").is_none());

        // A long press in the composer takes the selection over: the message
        // loses its selection and the field's menu shows.
        let draft = textarea.read_with(cx, |state, _| state.range_to_bounds(&(0..1)).unwrap());
        long_press(cx, draft.center());
        assert_eq!(menu_open(&markdown, cx), None);
        assert_eq!(
            textarea.read_with(cx, |state, _| state.selected_text().to_string()),
            "draft"
        );
        assert!(cx.debug_bounds("edit-menu-cut").is_some());

        // And back: the message's long press focuses the message, and the
        // field's controls go with its focus.
        long_press(cx, point(px(12.), px(8.)));
        assert_eq!(menu_open(&markdown, cx), Some(true));
        assert!(cx.debug_bounds("edit-menu-copy").is_some());
        assert!(cx.debug_bounds("edit-menu-cut").is_none());
    }
}
