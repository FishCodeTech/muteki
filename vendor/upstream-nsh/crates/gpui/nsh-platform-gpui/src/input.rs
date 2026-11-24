//! Pointer, wheel and keyboard input of a surface: hit testing over the
//! views, touches for the JS responder system, presses, hover / pressed
//! states, cursors, native pan / pinch / rotation values, routing wheel,
//! touchpad and touchscreen scrolling to the scroll views, and keys.
//!
//! The surface element ([`crate::events`]) turns GPUI's events into the
//! events here (root coordinates, points; times in seconds of the
//! surface's clock), and so does the test agent's input (`crate::synth`): real
//! and simulated input share every handler.
//!
//! Hit testing walks the views ([`pick`]), as the Linux backend does:
//! points go into children through their frames, the parent's scroll
//! offset and sticky shift ([`ViewState::child_origin`]) and their
//! transforms (inverted with [`Mat4::unproject`], so 3D transforms hit
//! where they are drawn). Hidden and concealed views, `pointerEvents`,
//! `hitSlop` and `overflow: hidden` are honored; outside a view's bounds,
//! only the area its descendants cover (`MountItem::Overflow`: a dropdown
//! under its field) is looked into, as on iOS and Android. The items a
//! view hosts ([`ViewState::natives`]: editors, headers, dialogs) are drawn
//! above its children and take the pointer first.
//!
//! Items, text fields and selectable text get the pointer events
//! themselves ([`Item`]): a press on them goes to them until it ends.
//! Presses, pans and scroll drags never start on hosted items and text
//! fields (they handle their own clicks, focus and selection drags), as
//! native widgets keep theirs on Linux; mouse drags over selectable text
//! select it rather than pan.
//!
//! Presses follow React Native and the other backends: the innermost view
//! listening to press events gets them (or the pressable text span under
//! the pointer; how it looks while pressed is the app's `activeStyle`,
//! applied by the core from the reported states); inside scroll views a touch reports `onPressDown` only after ~130 ms, as it may start a
//! scroll. A move beyond 6 pt starts the innermost pan or scroll view that
//! takes that direction (only touches and pens drag scroll views: the
//! mouse scrolls with the wheel, the touchpad, the keyboard and the
//! indicator, and its drags go to pans, responders and text selection);
//! beyond 10 pt the press is over.
//!
//! A view with a `panActivation` around the first pointer gates it
//! ([`Gate`], `PanActivation::decide` on every move): the scroll views
//! around the view wait until its pan fails, and never take the drag once
//! it activates; the activation goes to JS with the move's touches
//! (`TouchEvent::activated`), where the view becomes the responder.
//!
//! A scroll stays on the scroll view it began in (a touchpad gesture until
//! it ends, mouse wheel notches while they come within 200 ms), as in
//! browsers: inner views passing under the pointer do not take it over, and
//! its view reaching an edge does not hand the rest to an outer one.
//! Touchpad gestures end by GPUI's phases where the platform has them
//! (macOS, Windows' DirectManipulation; [`touchpad`]): fingers resting stop
//! a glide, lifting them glides on with the gesture's velocity (and the
//! system's momentum that follows is swallowed), as the macOS and Linux
//! backends do. Without phases (Wayland; X11 reports touchpads as wheel
//! lines) a precise scroll ends once its deltas pause. Touchpad pinches
//! (GPUI's `PinchEvent`, [`touchpad_pinch`]) and Ctrl + wheel pinch the
//! view under the pointer.
//!
//! The decisions over plain data (what a pointer going down may start, a
//! press's events, hover changes, hit areas, pinches, wheel latching) are
//! [`nsh_core::input`]'s, shared by the desktop backends.

use std::{
    cell::{Cell, RefCell},
    rc::Rc,
};

use nsh_core::{
    Command, EventKind, EventPayload, GestureValues, NodeId, PayloadValue, PointerEvents, ROOT_ID,
    TouchEvent, TouchPhase, TouchPoint, ViewKind,
    geometry::Points,
    input::{
        self, Candidate, Gate, Grab, GrabOptions, HitArea, Link, PAN_SLOP, PRESS_SLOP, Pinch,
        SCROLL_PRESS_DELAY, TOUCHPAD_IDLE, VelocityTracker, ZOOM_IDLE, ZOOM_NOTCH, has_press,
        laid_out, latch_holds, touchpad_wait, transforms,
    },
    tailwind::state::{HOVERED, PRESSED},
};
use rustc_hash::{FxHashMap, FxHashSet};

use crate::{
    geom::Mat4,
    surface::Surface,
    view::{ItemPointer, Native, NshView, ViewState},
};

type Press = input::Press<NshView>;

/// A pinch / rotation in progress.
#[derive(Clone)]
struct Transform {
    view: NshView,
    pinch: bool,
    rotation: bool,
    scale: f32,
    degrees: f32,
}

/// Two touchscreen fingers transforming a view.
struct TwoFinger {
    keys: [Key; 2],
    pinch: Pinch,
    transform: Transform,
}

use crate::keys::{VK_ESCAPE, VK_MENU, VK_TAB};

/// What a pointer did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PointerKind {
    /// A button (or a finger, a pen) went down.
    Down,
    /// It moved (with or without buttons).
    Move,
    /// The button went up.
    Up,
    /// The gesture was taken away (capture lost).
    Cancel,
    /// The mouse left the window.
    Leave,
}

/// Modifier keys.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Modifiers {
    /// Shift.
    pub(crate) shift: bool,
    /// Ctrl.
    pub(crate) ctrl: bool,
    /// Alt.
    pub(crate) alt: bool,
    /// The Windows key.
    pub(crate) meta: bool,
}

impl Modifiers {
    /// As the core's modifiers (`meta` is its `cmd`).
    fn core(self) -> nsh_core::keymap::Modifiers {
        nsh_core::keymap::Modifiers {
            ctrl: self.ctrl,
            alt: self.alt,
            shift: self.shift,
            cmd: self.meta,
        }
    }
}

/// A mouse, touch or pen event.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct PointerEvent {
    /// What happened.
    pub(crate) kind: PointerKind,
    /// Where (points, root coordinates).
    pub(crate) x: f32,
    /// Where.
    pub(crate) y: f32,
    /// The button: 0 left, 1 right, 2 middle, 3 back (X1), 4 forward (X2).
    pub(crate) button: u8,
    /// Which pointer: 1 is the mouse, touch and pen contacts have their own.
    pub(crate) pointer: u32,
    /// A finger.
    pub(crate) touch: bool,
    /// A pen.
    pub(crate) pen: bool,
    /// When (seconds, the surface's clock).
    pub(crate) time: f64,
    /// Modifiers held.
    pub(crate) modifiers: Modifiers,
    /// Consecutive clicks at the same place (presses).
    pub(crate) clicks: u32,
    /// The mouse button held during a move or as the mouse leaves,
    /// numbered as `button` (GPUI's `MouseMoveEvent::pressed_button`).
    pub(crate) pressed_button: Option<u8>,
}

/// A wheel or touchpad scroll.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct WheelEvent {
    /// Where the pointer is (points, root coordinates).
    pub(crate) x: f32,
    /// Where.
    pub(crate) y: f32,
    /// How far to scroll the content (points; positive toward the end:
    /// right, down).
    pub(crate) dx: f32,
    /// How far, vertically.
    pub(crate) dy: f32,
    /// From a precision touchpad or a high-resolution wheel (not whole
    /// notches).
    pub(crate) precise: bool,
    /// Modifiers held (Ctrl + wheel zooms, Shift + wheel scrolls
    /// sideways).
    pub(crate) modifiers: Modifiers,
    /// When (seconds, the surface's clock).
    pub(crate) time: f64,
}

/// A key going down or up.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct KeyEvent {
    /// The virtual key code (`VK_*`).
    pub(crate) vk: u16,
    /// Down (or up).
    pub(crate) down: bool,
    /// An auto-repeat of a key held down.
    pub(crate) repeat: bool,
    /// Modifiers held.
    pub(crate) modifiers: Modifiers,
}

/// The mouse cursor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum Cursor {
    /// The arrow.
    #[default]
    Arrow,
    /// A hand (`cursor: pointer`: links).
    Hand,
    /// The I-beam (text).
    Text,
    /// A crosshair.
    Crosshair,
    /// Four arrows.
    Move,
    /// Not allowed.
    NotAllowed,
    /// Resize left and right.
    ResizeEw,
    /// Resize up and down.
    ResizeNs,
    /// Busy.
    Wait,
    /// Working in the background.
    Progress,
    /// Help.
    Help,
    /// No cursor (`cursor: none`).
    Hidden,
}

/// Which device a pointer is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Key {
    /// The mouse (or touchpad clicks).
    Mouse,
    /// A finger or a pen, by its pointer id; behaves like a finger.
    Touch(u32),
}

/// The pointer input state of a surface.
#[derive(Default)]
pub(crate) struct Input {
    /// The pointers that are down.
    pointers: RefCell<Vec<Pointer>>,
    /// Two fingers pinching / rotating on the touchscreen.
    two: RefCell<Option<TwoFinger>>,
    /// A pinch by Ctrl + wheel, with the generation of its end timer.
    zoom: RefCell<Option<(Transform, u64)>>,
    /// A touchpad pinch ([`touchpad_pinch`]), with its absolute scale.
    pinch: RefCell<Option<(Transform, f32)>>,
    /// A precise (touchpad) scroll.
    wheel: RefCell<Option<Wheel>>,
    /// A timer watches the precise scroll for its pause.
    wheel_watch: Cell<bool>,
    /// When the system's momentum after a touchpad gesture last came (ms):
    /// it is swallowed, as the gesture's view glides by itself.
    momentum: Cell<Option<f64>>,
    /// The cursor the surface was last drawn with: GPUI takes cursors from
    /// frames, so a move that changes it draws again.
    pub(crate) cursor: Cell<Cursor>,
    /// The scroll view mouse wheel notches go to while they keep coming,
    /// whether they scroll horizontally, and when the last one came (ms).
    latch: RefCell<Option<(NshView, bool, f64)>>,
    /// Views that track `HOVERED`.
    hover_tracked: RefCell<FxHashSet<NodeId>>,
    /// Views listening to mouse motion; other apps skip the extra hit test.
    mouse_tracked: RefCell<FxHashSet<NodeId>>,
    /// Views that are hovered now.
    hovered: RefCell<Vec<NshView>>,
    /// The hosted item under the mouse (it gets moves and a leave).
    hover_item: RefCell<Option<Item>>,
    /// Native gesture values by view.
    gestures: RefCell<FxHashMap<NodeId, (NshView, GestureValues)>>,
    /// The JS responder blocks native scrolling and pans.
    js_block: Cell<bool>,
    /// Where the mouse last was (root coordinates): keyboard scrolling
    /// goes to the scroll view under it.
    last: Cell<Option<(f32, f32)>>,
    /// The modifier keys and the mouse button last seen: the mouse events
    /// of views a batch moved under a still mouse carry them.
    modifiers: Cell<Modifiers>,
    /// The mouse button held.
    pub(crate) button: Cell<Option<nsh_core::MouseButton>>,
    /// A key with Alt was taken: Alt's release must not open the window
    /// menu.
    alt_combo: Cell<bool>,
    /// Next touch identifier.
    next_id: Cell<u32>,
    /// Next press generation (invalidates callbacks of earlier ones).
    next_gen: Cell<u64>,
}

impl Input {
    /// A touchpad pinch is going on ([`touchpad_pinch`]).
    pub(crate) fn pinching(&self) -> bool {
        self.pinch.borrow().is_some()
    }
}

/// What a pointer is doing.
#[derive(Debug, Clone, PartialEq)]
enum Mode {
    /// Not past the pan slop yet.
    Idle,
    /// Past it, with nothing to drag.
    Free,
    /// Dragging a pan view.
    Pan(NshView),
    /// Scrolling a scroll view, with where the pointer was at the last
    /// move (the scroll module takes deltas).
    Scroll(NshView, (f32, f32)),
    /// Dragging a scroll view's indicator as a scroll bar's thumb (the
    /// mouse), from where the drag began.
    Thumb(NshView, (f32, f32)),
    /// One of two fingers pinching / rotating.
    TwoFinger,
}

/// Something that takes pointer events itself.
#[derive(Clone)]
enum Item {
    /// An item `view` hosts.
    Hosted(NshView, Native),
    /// A text field.
    Input(NshView),
    /// Selectable text (mouse selection).
    Text(NshView),
    /// A selectable view (mouse selection across its texts).
    Area(NshView),
}

impl Item {
    fn view(&self) -> &NshView {
        match self {
            Item::Hosted(view, _) | Item::Input(view) | Item::Text(view) | Item::Area(view) => view,
        }
    }

    fn same(&self, other: &Item) -> bool {
        match (self, other) {
            (Item::Hosted(_, a), Item::Hosted(_, b)) => Rc::ptr_eq(&a.item, &b.item),
            (Item::Input(a), Item::Input(b))
            | (Item::Text(a), Item::Text(b))
            | (Item::Area(a), Item::Area(b)) => a == b,
            _ => false,
        }
    }

    /// Sends a pointer event at `(x, y)` (root coordinates) in the item's
    /// coordinates; true when it took it.
    fn send(
        &self,
        surface: &Rc<Surface>,
        kind: PointerKind,
        (x, y): (f32, f32),
        clicks: u32,
        modifiers: Modifiers,
    ) -> bool {
        let (lx, ly) = local(self.view(), x, y);
        let mut event = ItemPointer {
            kind,
            x: lx,
            y: ly,
            clicks,
            modifiers,
        };
        match self {
            Item::Hosted(_, native) => {
                event.x -= native.rect.origin.x.0;
                event.y -= native.rect.origin.y.0;
                native.item.pointer(&event)
            }
            Item::Input(view) => crate::text_input::pointer(surface, view, &event),
            Item::Text(view) => crate::text::pointer(surface, view, &event),
            // Areas follow the pointer in root coordinates: autoscrolling
            // moves the area under it.
            Item::Area(_) => crate::selectable_view::pointer(surface, kind, x, y),
        }
    }
}

/// A pointer that is down.
struct Pointer {
    key: Key,
    /// Touch identifier for JS.
    id: u32,
    /// Behaves like a finger (delayed press-in, two-finger gestures).
    touch: bool,
    x: f32,
    y: f32,
    start: (f32, f32),
    /// When it last moved (ms).
    time: f64,
    velocity: VelocityTracker,
    /// The innermost view under it when it went down.
    target: NshView,
    /// The views around it that listen to touches, target outwards.
    path: Vec<NodeId>,
    /// Its touches are reported to JS.
    reporting: bool,
    press: Option<Press>,
    /// The item that took it, which gets its moves and its release.
    item: Option<Item>,
    /// Modifiers when it went down (for its item's events).
    modifiers: Modifiers,
    /// What a drag may start, innermost first.
    candidates: Vec<(NshView, Candidate)>,
    /// The undecided `panActivation` around it.
    gate: Option<Gate>,
    /// The view a second finger would pinch / rotate.
    pinch: Option<NshView>,
    /// Views that track `PRESSED` and are pressed by it.
    pressed: Vec<NshView>,
    mode: Mode,
    /// It moved (a JS responder granted now took it from a native press).
    moved: bool,
}

/// A precise (touchpad) scroll.
struct Wheel {
    view: NshView,
    /// When its last delta came (ms).
    last: f64,
    /// Accumulated deltas, for the velocity.
    total: (f32, f32),
    velocity: VelocityTracker,
    /// Began with the touchpad's phases ([`touchpad`]): it ends when they
    /// say so, not when its deltas pause.
    phased: bool,
    /// Began with fingers resting (a phase with no delta) and has not
    /// scrolled yet.
    held: bool,
}

impl Wheel {
    fn new(view: NshView, time: f64, phased: bool) -> Wheel {
        let mut velocity = VelocityTracker::default();
        velocity.add(time, nsh_core::geometry::point(Points(0.0), Points(0.0)));
        Wheel {
            view,
            last: time,
            total: (0.0, 0.0),
            velocity,
            phased,
            held: false,
        }
    }
}

// --- Timers -------------------------------------------------------------------------

/// Runs `f` on this (main) thread after `ms` milliseconds.
fn after_ms(ms: u64, f: impl FnOnce() + 'static) {
    crate::rt::after_ms(ms, f);
}

// --- The views' hooks ---------------------------------------------------------------

/// Forgets every gesture (the app stopped).
pub(crate) fn reset(surface: &Surface) {
    let input = &surface.input;
    input.pointers.borrow_mut().clear();
    input.two.borrow_mut().take();
    input.zoom.borrow_mut().take();
    input.pinch.borrow_mut().take();
    input.wheel.borrow_mut().take();
    input.momentum.set(None);
    input.latch.borrow_mut().take();
    input.hover_tracked.borrow_mut().clear();
    input.mouse_tracked.borrow_mut().clear();
    input.hovered.borrow_mut().clear();
    input.hover_item.borrow_mut().take();
    input.gestures.borrow_mut().clear();
    input.js_block.set(false);
}

/// `TrackStates`.
pub(crate) fn track_states(surface: &Rc<Surface>, view: &NshView, states: u16) {
    {
        let mut s = view.state_mut();
        s.tracked = states;
        // The core keeps the states still tracked and forgets the rest
        // (`Core::update_tracking`): so do we, or a change it was not told
        // about (the pointer leaving a view whose hover was tracked
        // throughout) would never reach it.
        s.states &= states;
    }
    update_hover_tracking(surface, view);
}

/// Whether hovering `view` matters: for its `hover:` styles (the
/// `HOVERED` state) or its `onMouseEnter` / `onMouseLeave` / `onMouseMove`.
fn hoverable(view: &NshView) -> bool {
    let s = view.state();
    input::hoverable(s.tracked, s.events)
}

fn update_hover_tracking(surface: &Surface, view: &NshView) {
    let id = view.state().id;
    if view.state().events.contains(EventKind::MouseMove) {
        surface.input.mouse_tracked.borrow_mut().insert(id);
    } else {
        surface.input.mouse_tracked.borrow_mut().remove(&id);
    }
    let mut tracked = surface.input.hover_tracked.borrow_mut();
    if hoverable(view) {
        tracked.insert(id);
    } else {
        tracked.remove(&id);
        // Not hovered as far as anyone knows: picked again if hovering it
        // matters again under the pointer.
        surface.input.hovered.borrow_mut().retain(|v| v != view);
    }
}

/// Sets (or clears) interaction state `bit` of `view` (pressed, hovered,
/// focused), reporting it when tracked and changed.
pub(crate) fn set_state(surface: &Surface, view: &NshView, bit: u16, on: bool) {
    let (id, next) = {
        let mut s = view.state_mut();
        let Some(next) = input::next_states(s.tracked, s.states, bit, on) else {
            return;
        };
        s.states = next;
        (s.id, next)
    };
    surface.control(|c| c.set_states(id, next));
}

/// `Gestures`.
pub(crate) fn gestures(surface: &Rc<Surface>, view: &NshView, values: GestureValues) {
    let id = view.state().id;
    let mut gestures = surface.input.gestures.borrow_mut();
    if values.is_empty() {
        gestures.remove(&id);
    } else {
        // Entries of views that were removed would keep them alive.
        gestures.retain(|_, (v, _)| v.attached());
        gestures.insert(id, (view.clone(), values));
    }
}

/// The view's events changed.
pub(crate) fn events_changed(surface: &Rc<Surface>, view: &NshView) {
    // Press listeners and the cursor are read when a pointer goes down or
    // moves; hover handlers have the view hovered (from the next pick).
    update_hover_tracking(surface, view);
}

/// The view's props changed.
pub(crate) fn props_changed(_surface: &Rc<Surface>, _view: &NshView) {
    // `pointerEvents`, `hitSlop` and `delayLongPress` are
    // read from the state when a pointer goes down.
}

/// The view's style changed.
pub(crate) fn style_changed(_view: &NshView) {
    // The cursor is read when the window asks for it.
}

/// `SetJsResponder` / `ClearJsResponder`. Runs while the mounter applies a
/// batch: it must not write animations (that borrows the mounter).
pub(crate) fn command(surface: &Rc<Surface>, _id: NodeId, command: &Command) {
    match command {
        Command::SetJsResponder { block } => {
            surface.input.js_block.set(*block);
            let mut pointers = std::mem::take(&mut *surface.input.pointers.borrow_mut());
            for p in &mut pointers {
                // Granted on a move: the JS gesture takes over from the native
                // press (a grant when the finger lands leaves inner presses be,
                // as they would have won the negotiation in React Native).
                if p.moved {
                    cancel_press(surface, p);
                    release_pressed(surface, p);
                }
                if *block && let Mode::Pan(view) = &p.mode {
                    let view = view.clone();
                    pan_end(surface, &view, p, false);
                    p.mode = Mode::Free;
                }
            }
            let mut current = surface.input.pointers.borrow_mut();
            pointers.append(&mut current);
            *current = pointers;
        }
        Command::ClearJsResponder => surface.input.js_block.set(false),
        _ => {}
    }
}

// --- Cursors ------------------------------------------------------------------------

/// The cursor of a CSS `cursor` value, `None` to inherit the parent's.
fn css_cursor(cursor: nsh_core::Cursor) -> Option<Cursor> {
    use nsh_core::Cursor as C;
    Some(match cursor {
        C::Auto => return None,
        C::Default => Cursor::Arrow,
        C::Pointer => Cursor::Hand,
        C::Text => Cursor::Text,
        C::Wait => Cursor::Wait,
        C::Progress => Cursor::Progress,
        C::Help => Cursor::Help,
        C::Crosshair => Cursor::Crosshair,
        C::Move => Cursor::Move,
        // Windows has no grab cursors: the hand offers to take, the four
        // arrows show it is being moved.
        C::Grab => Cursor::Hand,
        C::Grabbing => Cursor::Move,
        C::NotAllowed => Cursor::NotAllowed,
        // Nor zoom cursors.
        C::ZoomIn | C::ZoomOut => Cursor::Crosshair,
        C::ColResize => Cursor::ResizeEw,
        C::RowResize => Cursor::ResizeNs,
        C::None => Cursor::Hidden,
    })
}

/// A view's own cursor: its style's, the I-beam over text fields and
/// selectable text (a press target keeps the hand), or `None`.
fn view_cursor(s: &ViewState) -> Option<Cursor> {
    css_cursor(s.style.cursor).or_else(|| {
        (s.kind == ViewKind::Input || crate::text::selectable(s)).then_some(Cursor::Text)
    })
}

/// The cursor over `(x, y)` (root coordinates): the hosted item's there,
/// else the innermost view's that has one.
pub(crate) fn cursor(surface: &Rc<Surface>, x: f32, y: f32) -> Cursor {
    let Some(hit) = pick(&surface.root, x, y, true) else {
        return Cursor::Arrow;
    };
    if let Some(native) = &hit.item {
        let (lx, ly) = local(&hit.view, x, y);
        if let Some(c) = native
            .item
            .cursor(lx - native.rect.origin.x.0, ly - native.rect.origin.y.0)
        {
            return c;
        }
    }
    let in_area = crate::selectable_view::area_of(&hit.view).is_some();
    let mut view = Some(hit.view);
    while let Some(v) = view {
        let s = v.state();
        if let Some(c) = view_cursor(&s) {
            return c;
        }
        if in_area && crate::selectable_view::selects(&s) {
            return Cursor::Text;
        }
        drop(s);
        view = v.parent_view();
    }
    Cursor::Arrow
}

/// Pressing `(x, y)` (root coordinates) drags the window: the nearest view
/// that sets `windowDrag` says so, unless a text field, a hosted item
/// (header buttons, editors) or a view with press or touch handlers is
/// nearer.
pub(crate) fn window_drag(surface: &Surface, x: f32, y: f32) -> bool {
    let Some(hit) = pick(&surface.root, x, y, true) else {
        return false;
    };
    if hit.item.is_some() {
        return false;
    }
    let mut view = Some(hit.view);
    while let Some(v) = view {
        {
            let s = v.state();
            if let Some(drag) = s.props.window_drag {
                return drag;
            }
            if s.kind == ViewKind::Input
                || has_press(s.events)
                || s.events.contains(EventKind::Touch)
            {
                return false;
            }
        }
        view = v.parent_view();
    }
    false
}

/// Where pressing drags the window, as rectangles in root coordinates:
/// what [`window_drag`] answers point by point, for platforms that
/// hit-test drag areas themselves (Windows, `crate::chrome`). The views
/// that set `windowDrag`, less what is nearer and takes presses (text
/// fields, hosted items, views with press or touch handlers or with
/// `windowDrag: false`). Transforms are not followed.
pub(crate) fn window_drag_rects(surface: &Surface) -> Vec<nsh_core::Bounds<nsh_core::Points>> {
    let mut drag = Vec::new();
    let mut block = Vec::new();
    collect_drag(&surface.root, (0.0, 0.0), false, &mut drag, &mut block);
    for b in &block {
        drag = drag
            .into_iter()
            .flat_map(|r| crate::geom::subtract(r, *b))
            .collect();
    }
    drag
}

fn collect_drag(
    view: &NshView,
    origin: (f32, f32),
    mut inside: bool,
    drag: &mut Vec<nsh_core::Bounds<nsh_core::Points>>,
    block: &mut Vec<nsh_core::Bounds<nsh_core::Points>>,
) {
    let s = view.state();
    if s.style.hidden || s.concealed || s.props.pointer_events == PointerEvents::None {
        return;
    }
    let r = crate::geom::rect(
        origin.0,
        origin.1,
        s.frame.size.width.0,
        s.frame.size.height.0,
    );
    match s.props.window_drag {
        Some(true) => {
            drag.push(r);
            inside = true;
        }
        Some(false) => {
            if inside {
                block.push(r);
            }
            inside = false;
        }
        None => {
            if inside
                && (s.kind == ViewKind::Input
                    || has_press(s.events)
                    || s.events.contains(EventKind::Touch))
            {
                block.push(r);
                inside = false;
            }
        }
    }
    if inside {
        for n in &s.natives {
            let n = n.rect;
            block.push(crate::geom::rect(
                origin.0 + n.origin.x.0,
                origin.1 + n.origin.y.0,
                n.size.width.0,
                n.size.height.0,
            ));
        }
    }
    view.with_children(|children| {
        for child in children {
            let o = {
                let c = child.state();
                s.child_origin(c.frame, c.shift)
            };
            collect_drag(child, (origin.0 + o.0, origin.1 + o.1), inside, drag, block);
        }
    });
}

/// A secondary click at `(x, y)` (root coordinates): the nearest view
/// around the one under it that listens to `onContextMenu` hears it,
/// unless a text field or selectable text is nearer: that shows its edit
/// menu.
fn context_menu(surface: &Rc<Surface>, x: f32, y: f32, modifiers: Modifiers) {
    let Some(hit) = pick(&surface.root, x, y, true) else {
        return;
    };
    let area = crate::selectable_view::area_of(&hit.view);
    let mut view = Some(hit.view);
    while let Some(v) = view {
        let (id, listens, input, selectable) = {
            let s = v.state();
            (
                s.id,
                s.events.contains(EventKind::ContextMenu),
                s.kind == ViewKind::Input,
                crate::text::selectable(&s),
            )
        };
        if listens {
            let (location_x, location_y) = local(&v, x, y);
            let event = nsh_core::ContextMenuEvent {
                position: nsh_core::geometry::point(Points(location_x), Points(location_y)),
                page_position: nsh_core::geometry::point(Points(x), Points(y)),
                modifiers: modifiers.core(),
            };
            surface.event(id, EventKind::ContextMenu, EventPayload::ContextMenu(event));
            return;
        }
        if input {
            crate::text_input::context_menu(surface, &v);
            return;
        }
        // Texts in a selectable view select with it.
        match &area {
            Some(area) if *area == v => {
                crate::selectable_view::context_menu(surface, area);
                return;
            }
            None if selectable => {
                crate::text::context_menu(surface, &v);
                return;
            }
            _ => {}
        }
        view = v.parent_view();
    }
}

// --- Events from the window ---------------------------------------------------------

/// A pointer event from the window.
pub(crate) fn pointer(surface: &Rc<Surface>, event: PointerEvent) {
    let key = if event.touch || event.pen {
        Key::Touch(event.pointer)
    } else {
        Key::Mouse
    };
    let (x, y) = (event.x, event.y);
    let time = event.time * 1000.0;
    let mouse = key == Key::Mouse;
    if mouse {
        surface.input.modifiers.set(event.modifiers);
        surface.input.button.set(match event.kind {
            PointerKind::Down => mouse_button(event.button),
            PointerKind::Move | PointerKind::Leave => event.pressed_button.and_then(mouse_button),
            PointerKind::Up | PointerKind::Cancel => None,
        });
    }
    match event.kind {
        PointerKind::Down => {
            // The secondary button (GPUI's macOS window already turns a
            // Control-click into one).
            if mouse && event.button == 1 {
                context_menu(surface, x, y, event.modifiers);
                return;
            }
            // Any press elsewhere clears a text selection.
            crate::text::window_pressed(surface, x, y);
            crate::selectable_view::window_pressed(surface, x, y);
            if mouse {
                surface.input.last.set(Some((x, y)));
                match event.button {
                    0 => {}
                    // The mouse's back button goes back, as in browsers.
                    3 => {
                        crate::navigation::go_back(surface);
                        return;
                    }
                    _ => return,
                }
            }
            down(surface, key, x, y, time, event.clicks, event.modifiers);
        }
        PointerKind::Move => {
            if mouse {
                surface.input.last.set(Some((x, y)));
                let button = surface.input.button.get();
                let hit = hover(surface, x, y, event.modifiers, button);
                mouse_moved(surface, hit.as_ref(), (x, y), event.modifiers, button);
            }
            motion(surface, key, x, y, time);
        }
        PointerKind::Up => {
            if mouse && event.button != 0 {
                return;
            }
            up(surface, key, Some((x, y)), time, false);
        }
        // Where a cancel happens means nothing: the pointer stays where it
        // last was.
        PointerKind::Cancel => up(surface, key, None, time, true),
        // The views it was over hear it leave from where it last was.
        PointerKind::Leave => {
            let at = surface.input.last.take().unwrap_or((x, y));
            let button = surface.input.button.get();
            set_hovered(surface, Vec::new(), at, event.modifiers, button);
            set_hover_item(surface, None, (x, y), event.modifiers);
        }
    }
}

/// A wheel event from the window.
pub(crate) fn wheel(surface: &Rc<Surface>, event: WheelEvent) {
    let (x, y) = (event.x, event.y);
    surface.input.last.set(Some((x, y)));
    surface.input.modifiers.set(event.modifiers);
    let (mut dx, mut dy) = (event.dx, event.dy);
    // Mice have no pinch: Ctrl + wheel pinches, as it zooms on desktops
    // (precision touchpads pinch this way too).
    if event.modifiers.ctrl && dy != 0.0 && wheel_zoom(surface, x, y, dy) {
        return;
    }
    if !event.precise && dx == 0.0 && event.modifiers.shift {
        (dx, dy) = (dy, dx);
    }
    // A multiline field under the pointer scrolls its own lines first, as
    // GTK's and AppKit's text views do.
    if dy.abs() > dx.abs()
        && let Some(hit) = pick(&surface.root, x, y, true)
        && let Some(field) = chain(&hit.view)
            .into_iter()
            .find(|v| v.kind() == ViewKind::Input)
        && crate::text_input::wheel(&field, dy)
    {
        return;
    }
    let kind = if event.precise {
        WheelInput::Touchpad { phased: false }
    } else {
        WheelInput::Step
    };
    wheel_input(surface, x, y, dx, dy, event.time * 1000.0, kind);
}

/// A key from the window; true when handled. Only key downs (and their
/// repeats) go to the modules; ups are not acted on.
pub(crate) fn key(surface: &Rc<Surface>, event: KeyEvent) -> bool {
    if event.down && event.vk == VK_ESCAPE {
        crate::text_input::disarm(surface);
    }
    if event.down && crate::text_input::hold_key(surface, &event) {
        return true;
    }
    key_now(surface, event)
}

/// A key from the window, or one that waited behind typed text for the
/// next field ([`crate::text_input`]). True when handled.
pub(crate) fn key_now(surface: &Rc<Surface>, event: KeyEvent) -> bool {
    let input = &surface.input;
    if !event.down {
        crate::keymap::released(surface, &event);
        // `DefWindowProc` opens the window menu when Alt is released after
        // it saw no other key: swallow the release once Alt + key was ours.
        return event.vk == VK_MENU && input.alt_combo.replace(false);
    }
    // The window's shortcuts (full screen, the dev host's reload) work
    // wherever the focus is.
    let shortcut = surface.shortcuts.borrow().iter().any(|f| f(&event));
    let editing = crate::text_input::focused(surface).is_some();
    let handled = shortcut
        || crate::system::key(surface, &event)
        || crate::keymap::key(surface, &event, editing)
        || crate::chrome::shortcut(surface, &event)
        || crate::text_input::key(surface, &event)
        || crate::text::key(surface, &event)
        || crate::selectable_view::key(surface, &event)
        || crate::navigation::key_pressed(surface, &event)
        || {
            let (x, y) = input.last.get().unwrap_or((-1.0, -1.0));
            crate::scroll::key(surface, &event, x, y)
        }
        || tab(surface, &event);
    if handled && event.modifiers.alt {
        input.alt_combo.set(true);
    }
    handled
}

/// A key from the window while one of the surface's native components has
/// the keyboard focus; true when handled. GPUI gives the component the key
/// first (its key bindings, then its key listeners), so this sees only
/// keys the component let through: the window's shortcuts, then the app's
/// bindings as while a text field is edited (a binding whose `when` is
/// `auto` leaves typing keys to the component), then the menu's shortcuts.
/// Every other key stays the component's.
pub(crate) fn component_key(surface: &Rc<Surface>, event: KeyEvent) -> bool {
    if !event.down {
        crate::keymap::released(surface, &event);
        return event.vk == VK_MENU && surface.input.alt_combo.replace(false);
    }
    if event.vk == VK_ESCAPE {
        crate::text_input::disarm(surface);
    }
    let handled = surface.shortcuts.borrow().iter().any(|f| f(&event))
        || crate::keymap::key(surface, &event, true)
        || crate::chrome::shortcut(surface, &event);
    if handled && event.modifiers.alt {
        surface.input.alt_combo.set(true);
    }
    handled
}

/// Tab and Shift + Tab move the keyboard focus between the text fields (and
/// the native components that take the keyboard), in tree order (GTK's focus chain does this on Linux), within the newest
/// modal: the views under it never get the focus. The field reached has
/// its text selected, as in Windows' dialogs. Past the last field (the
/// first with Shift) the key is not taken: the window's focus moves on to
/// the embedding app's next tab stop, or wraps around when there is none
/// ([`crate::app`], [`tab_to_edge`]).
fn tab(surface: &Rc<Surface>, event: &KeyEvent) -> bool {
    let m = event.modifiers;
    if event.vk != VK_TAB || m.ctrl || m.alt || m.meta {
        return false;
    }
    let mut stops = Vec::new();
    collect_stops(
        surface,
        &crate::navigation::focus_scope(surface),
        &mut stops,
    );
    if stops.is_empty() {
        return false;
    }
    let focused = crate::text_input::focused(surface)
        .or_else(|| crate::native::focused_component(surface).and_then(|id| surface.view(id)));
    let current = focused.and_then(|f| stops.iter().position(|v| *v == f));
    // Past the ends the window's focus moves on ([`tab_to_edge`]).
    let Some(next) = input::tab_stop(current, stops.len(), m.shift, false) else {
        return false;
    };
    focus_stop(surface, &stops[next]);
    true
}

/// Focuses the first Tab stop (the last with `shift`), reached with Tab
/// from outside them: from the embedding app's widgets, or past the last
/// stop when the window has no other tab stop (they wrap around, as in the
/// other backends' windows). True when there is one.
pub(crate) fn tab_to_edge(surface: &Rc<Surface>, shift: bool) -> bool {
    let mut stops = Vec::new();
    collect_stops(
        surface,
        &crate::navigation::focus_scope(surface),
        &mut stops,
    );
    let next = if shift { stops.last() } else { stops.first() };
    match next {
        Some(view) => {
            focus_stop(surface, view);
            true
        }
        None => false,
    }
}

/// Moves the keyboard focus to `view`: a text field, or a native component
/// (focused by [`crate::native::apply_focus`], which has the window).
fn focus_stop(surface: &Rc<Surface>, view: &NshView) {
    if view.kind() == ViewKind::Native {
        crate::native::tab_to(surface, view.id());
    } else {
        crate::text_input::tab_to(view);
    }
}

/// The Tab stops shown under `view`, in tree order: enabled text fields and
/// the native components that take the keyboard.
fn collect_stops(surface: &Surface, view: &NshView, out: &mut Vec<NshView>) {
    {
        let s = view.state();
        if s.style.hidden || s.concealed || s.props.pointer_events == PointerEvents::None {
            return;
        }
        let stop = match s.kind {
            ViewKind::Input => !s.props.accessibility.state.disabled,
            ViewKind::Native => crate::native::focusable(surface, s.id),
            _ => false,
        };
        if stop {
            out.push(view.clone());
        }
    }
    view.with_children(|children| {
        for child in children {
            collect_stops(surface, child, out);
        }
    });
}

// --- Hit testing --------------------------------------------------------------------

/// What is under a point.
struct Hit {
    /// The innermost view.
    view: NshView,
    /// The item it hosts that the point is on.
    item: Option<Native>,
}

/// The innermost view at `(x, y)` (root coordinates); hosted items are
/// looked at only with `items`.
fn pick(root: &NshView, x: f32, y: f32, items: bool) -> Option<Hit> {
    pick_in(root, x, y, items)
}

fn pick_in(view: &NshView, x: f32, y: f32, items: bool) -> Option<Hit> {
    let s = view.state();
    // Views animating out after their removal are gone for the pointer.
    if s.style.hidden
        || s.concealed
        || s.props.pointer_events == PointerEvents::None
        || s.live.as_ref().is_some_and(|l| l.dying())
    {
        return None;
    }
    let pe = s.props.pointer_events;
    let clip = s.clips();
    let HitArea {
        inside,
        in_slop,
        children,
        ..
    } = input::hit_area(
        nsh_core::geometry::point(Points(x), Points(y)),
        s.frame.size,
        s.props.hit_slop,
        s.overflow,
        clip,
        laid_out(s.kind, s.id),
    );
    // Hosted items are drawn above the children, clipped with them.
    if items && !s.natives.is_empty() && (!clip || inside) {
        let natives = s.natives.clone();
        drop(s);
        for native in natives.iter().rev() {
            let r = native.rect;
            if native.item.hit(
                x - r.origin.x.0,
                y - r.origin.y.0,
                (r.size.width.0, r.size.height.0),
            ) {
                return Some(Hit {
                    view: view.clone(),
                    item: Some(native.clone()),
                });
            }
        }
    } else {
        drop(s);
    }
    let own = || {
        (pe != PointerEvents::BoxNone && in_slop).then(|| Hit {
            view: view.clone(),
            item: None,
        })
    };
    if !children || pe == PointerEvents::BoxOnly {
        return own();
    }
    let found = view.with_children(|kids| {
        let s = view.state();
        kids.iter().rev().find_map(|child| {
            let (cx, cy) = {
                let c = child.state();
                let (ox, oy) = s.child_origin(c.frame, c.shift);
                match c.transform() {
                    None => (x - ox, y - oy),
                    Some(t) => {
                        let local = Mat4::mul(&t, &Mat4::new().translate(ox, oy));
                        local.unproject(x, y)?
                    }
                }
            };
            pick_in(child, cx, cy, items)
        })
    });
    found.or_else(own)
}

/// `view` and its ancestors, innermost first.
fn chain(view: &NshView) -> Vec<NshView> {
    let mut out = vec![view.clone()];
    let mut current = view.parent_view();
    while let Some(v) = current {
        current = v.parent_view();
        out.push(v);
    }
    out
}

/// The selectable view a mouse press on `view` selects in: the area around
/// it, unless `view` is a text that is `selectable={false}` or a view
/// between them pans or handles touches itself.
fn area_press(surface: &Surface, view: &NshView) -> Option<NshView> {
    let area = crate::selectable_view::area_of(view)?;
    if !crate::selectable_view::takes(view) {
        return None;
    }
    let mut current = Some(view.clone());
    while let Some(v) = current {
        let s = v.state();
        let values = gesture_values(surface, &v, s.id);
        if values.pan_x.is_some()
            || values.pan_y.is_some()
            || [EventKind::Touch, EventKind::PanStart, EventKind::PanEnd]
                .into_iter()
                .any(|k| s.events.contains(k))
        {
            return None;
        }
        drop(s);
        if v == area {
            break;
        }
        current = v.parent_view();
    }
    Some(area)
}

/// A point in root coordinates in `view`'s.
fn local(view: &NshView, x: f32, y: f32) -> (f32, f32) {
    view.to_root()
        .and_then(|m| m.unproject(x, y))
        .unwrap_or((x, y))
}

fn gesture_values(surface: &Surface, view: &NshView, id: NodeId) -> GestureValues {
    surface
        .input
        .gestures
        .borrow()
        .get(&id)
        .filter(|(v, _)| v == view)
        .map(|(_, values)| *values)
        .unwrap_or_default()
}

// --- Pointers -----------------------------------------------------------------------

fn take(surface: &Surface, key: Key) -> Option<Pointer> {
    let mut pointers = surface.input.pointers.borrow_mut();
    let i = pointers.iter().position(|p| p.key == key)?;
    Some(pointers.swap_remove(i))
}

fn put(surface: &Surface, p: Pointer) {
    surface.input.pointers.borrow_mut().push(p);
}

fn down(
    surface: &Rc<Surface>,
    key: Key,
    x: f32,
    y: f32,
    time: f64,
    clicks: u32,
    modifiers: Modifiers,
) {
    // A release we never saw (the window lost the capture).
    if let Some(stale) = take(surface, key) {
        finish(surface, stale, true);
    }
    let Some(mut hit) = pick(&surface.root, x, y, true) else {
        return;
    };
    let touch = key != Key::Mouse;
    let mut item = None;
    if let Some(native) = hit.item.take() {
        let hosted = Item::Hosted(hit.view.clone(), native);
        if hosted.send(surface, PointerKind::Down, (x, y), clicks, modifiers) {
            item = Some(hosted);
        } else {
            // The item declined: the views below it get the press.
            let Some(below) = pick(&surface.root, x, y, false) else {
                return;
            };
            hit = below;
        }
    }
    // The mouse drags an indicator as a scroll bar: its thumb follows the
    // pointer, where a drag on the content moves the content with it.
    let thumb = (!touch && item.is_none())
        .then(|| {
            chain(&hit.view).into_iter().find(|v| {
                let (lx, ly) = local(v, x, y);
                crate::scroll::grabs_thumb(v, lx, ly)
            })
        })
        .flatten();
    if let Some(view) = thumb {
        let input = &surface.input;
        if input.pointers.borrow().is_empty() {
            input.js_block.set(false);
        }
        let id = input.next_id.get();
        input.next_id.set(id.wrapping_add(1));
        let mut p = Pointer {
            key,
            id,
            touch,
            x,
            y,
            start: (x, y),
            time,
            velocity: VelocityTracker::default(),
            target: view.clone(),
            path: Vec::new(),
            reporting: false,
            press: None,
            item: None,
            modifiers,
            candidates: Vec::new(),
            gate: None,
            pinch: None,
            pressed: Vec::new(),
            mode: Mode::Thumb(view.clone(), (x, y)),
            moved: false,
        };
        p.velocity
            .add(time, nsh_core::geometry::point(Points(x), Points(y)));
        crate::scroll::thumb_begin(surface, &view);
        put(surface, p);
        return;
    }
    if item.is_none()
        && !touch
        && hit.view.kind() != ViewKind::Input
        && let Some(area) = area_press(surface, &hit.view)
    {
        // Mouse drags in a selectable view select across its texts; its
        // texts never select alone.
        crate::selectable_view::down(surface, &area, &hit.view, (x, y), clicks, modifiers.shift);
        item = Some(Item::Area(area));
    }
    if item.is_none() {
        let candidate = if hit.view.kind() == ViewKind::Input {
            Some(Item::Input(hit.view.clone()))
        } else if !touch && crate::text::selectable(&hit.view.state()) {
            // Mouse drags over selectable text select it rather than
            // pan.
            Some(Item::Text(hit.view.clone()))
        } else {
            None
        };
        if let Some(c) = candidate {
            c.send(surface, PointerKind::Down, (x, y), clicks, modifiers);
            item = Some(c);
        }
    }
    let native = matches!(item, Some(Item::Hosted(..) | Item::Input(_)));
    let selects = matches!(item, Some(Item::Text(_) | Item::Area(_)));
    let input = &surface.input;
    if input.pointers.borrow().is_empty() {
        input.js_block.set(false);
    }
    let views = chain(&hit.view);
    // The gate follows the first pointer only, as on the other platforms.
    let first = input.pointers.borrow().is_empty();
    let options = GrabOptions::new(touch, native, selects, first);
    let mut grab = Grab::default();
    for v in &views {
        let s = v.state();
        let link = Link::new(
            s.id,
            s.events,
            s.tracked,
            gesture_values(surface, v, s.id),
            s.scroll.as_ref().map(|scroll| scroll.horizontal),
            s.props.pan_activation,
        );
        grab.add(v, &link, options);
    }
    let Grab {
        path,
        candidates,
        pinch,
        pressed,
        in_scroll,
        gate,
        ..
    } = grab;
    let press = if native {
        None
    } else {
        find_press(surface, &hit.view, &views, x, y, modifiers, clicks)
    };
    let id = input.next_id.get();
    input.next_id.set(id.wrapping_add(1));
    let mut p = Pointer {
        key,
        id,
        touch,
        x,
        y,
        start: (x, y),
        time,
        velocity: VelocityTracker::default(),
        target: hit.view,
        reporting: !path.is_empty(),
        path,
        press,
        item,
        modifiers,
        candidates,
        gate,
        pinch,
        pressed,
        mode: Mode::Idle,
        moved: false,
    };
    p.velocity
        .add(time, nsh_core::geometry::point(Points(x), Points(y)));
    if p.reporting {
        report(surface, TouchPhase::Start, &p, None);
    }
    for v in &p.pressed {
        set_state(surface, v, PRESSED, true);
    }
    if touch && start_two_finger(surface, &mut p) {
        put(surface, p);
        return;
    }
    if let Some(press) = &mut p.press {
        let generation = press.generation;
        if touch && in_scroll {
            after(
                surface,
                key,
                generation,
                SCROLL_PRESS_DELAY,
                |surface, p| {
                    let at = (p.x, p.y);
                    if let Some(press) = &mut p.press {
                        press_in(surface, press, at);
                    }
                },
            );
        } else {
            press_in(surface, press, (x, y));
        }
        if press.events.contains(EventKind::LongPress) {
            let delay = press.long_delay.max(0.0) as u64;
            after(surface, key, generation, delay, |surface, p| {
                let at = (p.x, p.y);
                let Some(press) = &mut p.press else { return };
                press_in(surface, press, at);
                press.long = true;
                press_event(surface, press, EventKind::LongPress, at);
            });
        }
    }
    put(surface, p);
}

fn motion(surface: &Rc<Surface>, key: Key, x: f32, y: f32, time: f64) {
    let Some(mut p) = take(surface, key) else {
        return;
    };
    if p.x == x && p.y == y {
        put(surface, p);
        return;
    }
    p.x = x;
    p.y = y;
    p.time = time;
    p.velocity
        .add(time, nsh_core::geometry::point(Points(x), Points(y)));
    if let Some(item) = &p.item {
        item.send(surface, PointerKind::Move, (x, y), 1, p.modifiers);
    }
    let moved = std::mem::replace(&mut p.moved, true);
    let (tx, ty) = (x - p.start.0, y - p.start.1);
    let activated = Gate::step(&mut p.gate, &mut p.candidates, Points(tx), Points(ty));
    // The view's JS responder takes the drag over: nothing native starts
    // meanwhile.
    if activated.is_some() && p.mode == Mode::Idle {
        p.mode = Mode::Free;
    }
    if p.reporting {
        report(surface, TouchPhase::Move, &p, activated);
    }
    if p.mode == Mode::TwoFinger {
        put(surface, p);
        two_finger_moved(surface);
        return;
    }
    let distance = tx.hypot(ty);
    // JS hears of the first move only now: give its responder a chance to
    // take the gesture (and block native drags) before one starts.
    let js_pending = p.reporting && !moved;
    if p.mode == Mode::Idle && distance > PAN_SLOP.0 && !js_pending {
        // While the gate decides, the candidates around it wait (the
        // pointer stays idle and they are asked again on the next move).
        if p.gate.is_none() {
            p.mode = Mode::Free;
        }
        let started = if surface.input.js_block.get() {
            None
        } else {
            input::startable(&p.candidates, p.gate.as_ref(), Points(tx), Points(ty)).cloned()
        };
        match started {
            Some((view, Candidate::Pan { .. })) => {
                p.gate = None;
                pan_start(surface, &view, &p);
                p.mode = Mode::Pan(view);
            }
            Some((view, Candidate::Scroll { .. })) => {
                // The scroll view takes the touches over from JS.
                if p.reporting {
                    report(surface, TouchPhase::Cancel, &p, None);
                    p.reporting = false;
                }
                p.gate = None;
                crate::scroll::drag_begin(surface, &view);
                // The drag scrolls from where it began: the slop is not lost.
                crate::scroll::drag_move(surface, &view, tx, ty);
                p.mode = Mode::Scroll(view, (x, y));
                cancel_press(surface, &mut p);
                release_pressed(surface, &mut p);
                put(surface, p);
                return;
            }
            None => {}
        }
    }
    if matches!(p.mode, Mode::Pan(_) | Mode::Scroll(..)) || distance > PRESS_SLOP.0 {
        cancel_press(surface, &mut p);
        release_pressed(surface, &mut p);
    }
    match &mut p.mode {
        Mode::Pan(view) => {
            let view = view.clone();
            pan_move(surface, &view, tx, ty);
        }
        Mode::Scroll(view, last) => {
            let (dx, dy) = (x - last.0, y - last.1);
            *last = (x, y);
            let view = view.clone();
            crate::scroll::drag_move(surface, &view, dx, dy);
        }
        Mode::Thumb(view, origin) => {
            let view = view.clone();
            crate::scroll::thumb_move(surface, &view, x - origin.0, y - origin.1);
        }
        _ => {}
    }
    put(surface, p);
}

/// The pointer lifted at `at` (or was cancelled where it last was).
fn up(surface: &Rc<Surface>, key: Key, at: Option<(f32, f32)>, time: f64, cancelled: bool) {
    let Some(mut p) = take(surface, key) else {
        return;
    };
    if let Some((x, y)) = at {
        // The release's own movement still scrolls or pans.
        if let Mode::Scroll(view, last) = &mut p.mode
            && (x, y) != *last
        {
            let (dx, dy) = (x - last.0, y - last.1);
            *last = (x, y);
            let view = view.clone();
            crate::scroll::drag_move(surface, &view, dx, dy);
        }
        if let Mode::Thumb(view, origin) = &p.mode {
            let view = view.clone();
            crate::scroll::thumb_move(surface, &view, x - origin.0, y - origin.1);
        }
        p.x = x;
        p.y = y;
    }
    p.time = time;
    p.velocity
        .add(time, nsh_core::geometry::point(Points(p.x), Points(p.y)));
    finish(surface, p, cancelled);
}

/// The pointer lifted (or was cancelled): ends what it did.
fn finish(surface: &Rc<Surface>, mut p: Pointer, cancelled: bool) {
    if let Some(item) = p.item.take() {
        let kind = if cancelled {
            PointerKind::Cancel
        } else {
            PointerKind::Up
        };
        item.send(surface, kind, (p.x, p.y), 1, p.modifiers);
    }
    if p.reporting {
        let phase = if cancelled {
            TouchPhase::Cancel
        } else {
            TouchPhase::End
        };
        report(surface, phase, &p, None);
    }
    release_pressed(surface, &mut p);
    let mode = std::mem::replace(&mut p.mode, Mode::Free);
    match mode {
        Mode::TwoFinger => end_two_finger(surface),
        Mode::Pan(view) => {
            pan_move(surface, &view, p.x - p.start.0, p.y - p.start.1);
            pan_end(surface, &view, &p, true);
        }
        Mode::Scroll(view, _) => {
            let (vx, vy) = p.velocity.velocity();
            crate::scroll::drag_end(surface, &view, vx, vy);
        }
        Mode::Thumb(view, _) => crate::scroll::thumb_end(surface, &view),
        Mode::Idle | Mode::Free => {
            if let Some(mut press) = p.press.take() {
                let at = (p.x, p.y);
                if cancelled {
                    press_out(surface, &mut press, false, at);
                } else {
                    // A quick tap inside a scroll view: the press in now.
                    press_in(surface, &mut press, at);
                    let inside = released_inside(&press.view, p.x, p.y);
                    press_out(surface, &mut press, inside, at);
                }
            }
        }
    }
}

/// Sends the touches of the reporting pointers, `changed` being the one
/// that `phase` is about (it is not in the list while it is handled), and
/// the view whose pan activation fired with it.
fn report(surface: &Surface, phase: TouchPhase, changed: &Pointer, activated: Option<NodeId>) {
    let points = {
        let pointers = surface.input.pointers.borrow();
        pointers
            .iter()
            .chain(std::iter::once(changed))
            .filter(|p| p.reporting)
            .map(|p| {
                let is_changed = std::ptr::eq(p, changed);
                let (lx, ly) = local(&p.target, p.x, p.y);
                TouchPoint {
                    id: p.id,
                    target: p.target.id(),
                    // JS keeps a finger's path from its start.
                    path: if phase == TouchPhase::Start && is_changed {
                        p.path.clone()
                    } else {
                        Vec::new()
                    },
                    page: nsh_core::geometry::point(Points(p.x), Points(p.y)),
                    location: nsh_core::geometry::point(Points(lx), Points(ly)),
                    changed: is_changed,
                }
            })
            .collect()
    };
    let event = TouchEvent {
        phase,
        timestamp: changed.time,
        points,
        activated,
    };
    surface.event(
        ROOT_ID,
        EventKind::Touch,
        EventPayload::Touch(Box::new(event)),
    );
}

/// Runs `f` on pointer `key` after `ms` unless its press `generation` ended.
fn after(
    surface: &Rc<Surface>,
    key: Key,
    generation: u64,
    ms: u64,
    f: impl FnOnce(&Rc<Surface>, &mut Pointer) + 'static,
) {
    let weak = Rc::downgrade(surface);
    after_ms(ms, move || {
        let Some(surface) = weak.upgrade() else {
            return;
        };
        let Some(mut p) = take(&surface, key) else {
            return;
        };
        if p.press
            .as_ref()
            .is_some_and(|press| press.generation == generation)
        {
            f(&surface, &mut p);
        }
        put(&surface, p);
    });
}

fn release_pressed(surface: &Surface, p: &mut Pointer) {
    for v in p.pressed.drain(..) {
        set_state(surface, &v, PRESSED, false);
    }
}

// --- Presses ------------------------------------------------------------------------

/// The press a pointer down at `(x, y)` on `target` starts: the pressable
/// text span under it, else the innermost enabled view listening to
/// presses.
fn find_press(
    surface: &Surface,
    target: &NshView,
    views: &[NshView],
    x: f32,
    y: f32,
    modifiers: Modifiers,
    clicks: u32,
) -> Option<Press> {
    let generation = surface.input.next_gen.get();
    surface.input.next_gen.set(generation + 1);
    {
        let s = target.state();
        if let Some(text) = &s.text
            && !text.text.links.is_empty()
        {
            drop(s);
            let (lx, ly) = local(target, x, y);
            let s = target.state();
            let text = s.text.as_ref()?;
            let link = crate::text::char_at(&s, lx, ly).and_then(|i| text.text.link_at(i));
            if let Some(link) = link
                && has_press(link.events)
            {
                return Some(
                    Press::new(
                        target.clone(),
                        link.node,
                        link.events,
                        s.props.long_press_delay,
                        generation,
                    )
                    .clicked(modifiers.core(), clicks),
                );
            }
        }
    }
    views.iter().find_map(|v| {
        let s = v.state();
        (has_press(s.events) && !s.props.accessibility.state.disabled).then(|| {
            Press::new(
                v.clone(),
                s.id,
                s.events,
                s.props.long_press_delay,
                generation,
            )
            .clicked(modifiers.core(), clicks)
        })
    })
}

fn press_in(surface: &Rc<Surface>, press: &mut Press, at: (f32, f32)) {
    if let Some(kind) = press.press_in() {
        press_event(surface, press, kind, at);
    }
}

/// Ends a press: `onPressUp`, then `onPress` if `fire` (released inside)
/// and it was no long press. `at` is the pointer in root coordinates.
fn press_out(surface: &Rc<Surface>, press: &mut Press, fire: bool, at: (f32, f32)) {
    for kind in press.press_out(fire) {
        press_event(surface, press, kind, at);
    }
}

/// Sends `kind` of `press` with the pointer at `(x, y)` (root coordinates).
fn press_event(surface: &Surface, press: &Press, kind: EventKind, (x, y): (f32, f32)) {
    log::debug!(target: "nsh::input", "{} on {}", kind.prop_name(), press.node);
    let (location_x, location_y) = local(&press.view, x, y);
    let event = nsh_core::PressEvent {
        position: nsh_core::geometry::point(Points(location_x), Points(location_y)),
        page_position: nsh_core::geometry::point(Points(x), Points(y)),
        modifiers: press.modifiers,
        clicks: press.clicks,
    };
    surface.event(press.node, kind, EventPayload::Press(event));
}

fn cancel_press(surface: &Rc<Surface>, p: &mut Pointer) {
    if let Some(mut press) = p.press.take() {
        press_out(surface, &mut press, false, (p.x, p.y));
    }
}

/// Whether a release at `(x, y)` (root coordinates) is on `view`, its hit
/// slop and a small tolerance around them.
fn released_inside(view: &NshView, x: f32, y: f32) -> bool {
    let (lx, ly) = local(view, x, y);
    let s = view.state();
    input::released_inside(
        nsh_core::geometry::point(Points(lx), Points(ly)),
        s.frame.size,
        s.props.hit_slop,
    )
}

// --- Pans ---------------------------------------------------------------------------

fn pan_payload(surface: &Surface, id: NodeId, t: (f32, f32), v: (f32, f32)) -> EventPayload {
    let (value_x, value_y) = surface.anim.borrow().pan_values(id);
    EventPayload::Pan {
        translation: nsh_core::geometry::point(Points(t.0), Points(t.1)),
        velocity_x: v.0,
        velocity_y: v.1,
        value_x: value_x.map(Points),
        value_y: value_y.map(Points),
    }
}

fn pan_start(surface: &Rc<Surface>, view: &NshView, p: &Pointer) {
    let (id, events) = {
        let s = view.state();
        (s.id, s.events)
    };
    surface.anim.borrow_mut().pan_began(id);
    if events.contains(EventKind::PanStart) {
        let payload = pan_payload(surface, id, (0.0, 0.0), p.velocity.velocity());
        surface.event(id, EventKind::PanStart, payload);
    }
}

fn pan_move(surface: &Rc<Surface>, view: &NshView, tx: f32, ty: f32) {
    let id = view.state().id;
    surface.anim.borrow_mut().pan_moved(id, tx, ty);
    surface.write_animations();
    surface.ensure_ticking();
}

/// Reports the end of a pan (`write`: the values may be written now; not
/// while a batch is applied).
fn pan_end(surface: &Rc<Surface>, view: &NshView, p: &Pointer, write: bool) {
    let (id, events) = {
        let s = view.state();
        (s.id, s.events)
    };
    if write {
        surface.write_animations();
    }
    if events.contains(EventKind::PanEnd) {
        let t = (p.x - p.start.0, p.y - p.start.1);
        let payload = pan_payload(surface, id, t, p.velocity.velocity());
        surface.event(id, EventKind::PanEnd, payload);
    }
}

// --- Pinch and rotation -------------------------------------------------------------

fn transform_payload(key: &'static str, amount: f32) -> EventPayload {
    EventPayload::Map(vec![
        (key, PayloadValue::Number(amount as f64)),
        ("velocity", PayloadValue::Number(0.0)),
    ])
}

fn transform_view(surface: &Surface, view: &NshView) -> Transform {
    let (id, events) = {
        let s = view.state();
        (s.id, s.events)
    };
    let (pinch, rotation) = transforms(&gesture_values(surface, view, id), events);
    Transform {
        view: view.clone(),
        pinch,
        rotation,
        scale: 1.0,
        degrees: 0.0,
    }
}

fn transform_begin(surface: &Surface, t: &Transform) {
    let (id, events) = {
        let s = t.view.state();
        (s.id, s.events)
    };
    {
        let mut anim = surface.anim.borrow_mut();
        if t.pinch {
            anim.transform_began(id, false);
        }
        if t.rotation {
            anim.transform_began(id, true);
        }
    }
    if t.pinch && events.contains(EventKind::PinchStart) {
        surface.event(id, EventKind::PinchStart, transform_payload("scale", 1.0));
    }
    if t.rotation && events.contains(EventKind::RotationStart) {
        let payload = transform_payload("rotation", 0.0);
        surface.event(id, EventKind::RotationStart, payload);
    }
}

fn transform_move(surface: &Rc<Surface>, t: &Transform) {
    let id = t.view.state().id;
    {
        let mut anim = surface.anim.borrow_mut();
        if t.pinch {
            anim.transform_moved(id, false, t.scale);
        }
        if t.rotation {
            anim.transform_moved(id, true, t.degrees);
        }
    }
    surface.write_animations();
    surface.ensure_ticking();
}

fn transform_end(surface: &Surface, t: &Transform) {
    let (id, events) = {
        let s = t.view.state();
        (s.id, s.events)
    };
    if t.pinch && events.contains(EventKind::PinchEnd) {
        surface.event(id, EventKind::PinchEnd, transform_payload("scale", t.scale));
    }
    if t.rotation && events.contains(EventKind::RotationEnd) {
        let payload = transform_payload("rotation", t.degrees);
        surface.event(id, EventKind::RotationEnd, payload);
    }
}

/// A second finger went down (`p`, not in the list yet): pinches /
/// rotates the view either finger would, ending what the first one did.
fn start_two_finger(surface: &Rc<Surface>, p: &mut Pointer) -> bool {
    let other = {
        let pointers = surface.input.pointers.borrow();
        let mut touches = pointers.iter().filter(|o| o.touch);
        match (touches.next(), touches.next()) {
            (Some(o), None) => o.key,
            _ => return false,
        }
    };
    let Some(mut o) = take(surface, other) else {
        return false;
    };
    let Some(view) = p.pinch.clone().or_else(|| o.pinch.clone()) else {
        put(surface, o);
        return false;
    };
    for q in [&mut *p, &mut o] {
        cancel_press(surface, q);
        release_pressed(surface, q);
    }
    match std::mem::replace(&mut o.mode, Mode::TwoFinger) {
        Mode::Pan(pan) => pan_end(surface, &pan, &o, true),
        Mode::Scroll(scroll, _) => crate::scroll::drag_end(surface, &scroll, 0.0, 0.0),
        _ => {}
    }
    p.mode = Mode::TwoFinger;
    let transform = transform_view(surface, &view);
    transform_begin(surface, &transform);
    *surface.input.two.borrow_mut() = Some(TwoFinger {
        keys: [o.key, p.key],
        pinch: Pinch::new(
            nsh_core::geometry::point(Points(o.x), Points(o.y)),
            nsh_core::geometry::point(Points(p.x), Points(p.y)),
        ),
        transform,
    });
    put(surface, o);
    true
}

fn two_finger_moved(surface: &Rc<Surface>) {
    let transform = {
        let mut two = surface.input.two.borrow_mut();
        let Some(t) = two.as_mut() else { return };
        let positions = {
            let pointers = surface.input.pointers.borrow();
            let at = |key: Key| {
                let p = pointers.iter().find(|p| p.key == key)?;
                Some(nsh_core::geometry::point(Points(p.x), Points(p.y)))
            };
            at(t.keys[0]).zip(at(t.keys[1]))
        };
        let Some((a, b)) = positions else { return };
        let (scale, rotation) = t.pinch.at(a, b);
        t.transform.scale = scale;
        t.transform.degrees = rotation.0.to_degrees();
        t.transform.clone()
    };
    transform_move(surface, &transform);
}

/// A finger of the pair lifted: the gesture ends, the other finger does
/// nothing more until it lifts.
fn end_two_finger(surface: &Rc<Surface>) {
    let Some(t) = surface.input.two.borrow_mut().take() else {
        return;
    };
    for p in surface.input.pointers.borrow_mut().iter_mut() {
        if t.keys.contains(&p.key) {
            p.mode = Mode::Free;
        }
    }
    transform_end(surface, &t.transform);
}

/// A Ctrl + wheel step of `dy` points (negative: zooms in) at `(x, y)`:
/// pinches the view under it that pinches, ending once the steps stop.
/// Returns whether a view took it.
fn wheel_zoom(surface: &Rc<Surface>, x: f32, y: f32, dy: f32) -> bool {
    let input = &surface.input;
    let generation = input.next_gen.get();
    input.next_gen.set(generation + 1);
    let current = input.zoom.borrow_mut().take();
    let mut t = match current {
        Some((t, _)) => t,
        None => {
            let Some(hit) = pick(&surface.root, x, y, true) else {
                return false;
            };
            let view = chain(&hit.view).into_iter().find(|v| {
                let s = v.state();
                transforms(&gesture_values(surface, v, s.id), s.events).0
            });
            let Some(view) = view else { return false };
            let mut t = transform_view(surface, &view);
            t.rotation = false;
            transform_begin(surface, &t);
            t
        }
    };
    t.scale *= zoom_factor(dy);
    transform_move(surface, &t);
    *input.zoom.borrow_mut() = Some((t, generation));
    let weak = Rc::downgrade(surface);
    after_ms(ZOOM_IDLE, move || {
        let Some(surface) = weak.upgrade() else {
            return;
        };
        let ended = {
            let mut zoom = surface.input.zoom.borrow_mut();
            match zoom.as_ref() {
                Some((_, g)) if *g == generation => zoom.take(),
                _ => None,
            }
        };
        if let Some((t, _)) = ended {
            transform_end(&surface, &t);
        }
    });
    true
}

/// The scale a Ctrl + wheel scroll of `dy` points pinches by: one notch is
/// one [`input::ZOOM_STEP`], scrolling up zooms in.
fn zoom_factor(dy: f32) -> f32 {
    input::zoom_factor(dy / ZOOM_NOTCH.0)
}

// --- Hover --------------------------------------------------------------------------

/// The mouse moved to `(x, y)` with `button` held: updates `HOVERED`
/// along the views under it and the hosted item under it; returns what is
/// under it.
fn hover(
    surface: &Rc<Surface>,
    x: f32,
    y: f32,
    modifiers: Modifiers,
    button: Option<nsh_core::MouseButton>,
) -> Option<Hit> {
    let input = &surface.input;
    let tracking = !input.hover_tracked.borrow().is_empty() || !input.hovered.borrow().is_empty();
    let pressed = input.pointers.borrow().iter().any(|p| p.key == Key::Mouse);
    let hit = pick(&surface.root, x, y, true);
    // The scroll views under the mouse show their indicators.
    if let Some(hit) = &hit {
        for v in chain(&hit.view)
            .iter()
            .filter(|v| v.kind() == ViewKind::Scroll)
        {
            crate::scroll::hovered(surface, v);
        }
    }
    if tracking {
        let views = match &hit {
            Some(hit) => chain(&hit.view).into_iter().filter(hoverable).collect(),
            None => Vec::new(),
        };
        set_hovered(surface, views, (x, y), modifiers, button);
    }
    // While the mouse is down, its moves go to what it pressed.
    if !pressed {
        let item = hit
            .as_ref()
            .and_then(|h| h.item.clone().map(|n| Item::Hosted(h.view.clone(), n)));
        set_hover_item(surface, item, (x, y), modifiers);
    }
    hit
}

/// `onMouseMove` with the mouse at `at` (root coordinates) over `hit`,
/// `modifiers` and `button` held: the views under it and the views they
/// are inside that listen hear it, inside out, each with the position in
/// it. Real motion and modifier changes only: views a batch moves under a
/// still mouse are entered ([`rehover`]), not moved over.
fn mouse_moved(
    surface: &Surface,
    hit: Option<&Hit>,
    at: (f32, f32),
    modifiers: Modifiers,
    button: Option<nsh_core::MouseButton>,
) {
    if surface.input.mouse_tracked.borrow().is_empty() {
        return;
    }
    let Some(hit) = hit else {
        return;
    };
    for view in chain(&hit.view) {
        mouse_event(surface, &view, EventKind::MouseMove, at, modifiers, button);
    }
}

/// `kind` (`onMouseEnter`, `onMouseLeave`, `onMouseMove`) of `view`, if it
/// listens, with the mouse at `(x, y)` (root coordinates).
fn mouse_event(
    surface: &Surface,
    view: &NshView,
    kind: EventKind,
    (x, y): (f32, f32),
    modifiers: Modifiers,
    button: Option<nsh_core::MouseButton>,
) {
    let id = {
        let s = view.state();
        if !s.events.contains(kind) {
            return;
        }
        s.id
    };
    let (location_x, location_y) = local(view, x, y);
    let event = nsh_core::MouseEvent::new(
        nsh_core::geometry::point(Points(location_x), Points(location_y)),
        nsh_core::geometry::point(Points(x), Points(y)),
        modifiers.core(),
        button,
    );
    surface.event(id, kind, EventPayload::Mouse(event));
}

/// A mouse button numbered as [`PointerEvent::button`] numbers them.
fn mouse_button(button: u8) -> Option<nsh_core::MouseButton> {
    use nsh_core::MouseButton as B;
    Some(match button {
        0 => B::Left,
        1 => B::Right,
        2 => B::Middle,
        3 => B::Back,
        4 => B::Forward,
        _ => return None,
    })
}

/// The views under the mouse are `views` (innermost first), the mouse at
/// `at` (root coordinates): those it left hear so from the inside out,
/// then those it came over from the outside in, as with the DOM's
/// `mouseleave` / `mouseenter`.
fn set_hovered(
    surface: &Surface,
    views: Vec<NshView>,
    at: (f32, f32),
    modifiers: Modifiers,
    button: Option<nsh_core::MouseButton>,
) {
    let old = std::mem::replace(&mut *surface.input.hovered.borrow_mut(), views.clone());
    let (left, entered) = input::hover_changes(&old, &views);
    for v in left {
        set_state(surface, v, HOVERED, false);
        mouse_event(surface, v, EventKind::MouseLeave, at, modifiers, button);
    }
    for v in entered {
        set_state(surface, v, HOVERED, true);
        mouse_event(surface, v, EventKind::MouseEnter, at, modifiers, button);
    }
}

/// The keyboard's modifiers changed (`modifiers` held now): while the
/// mouse rests over the surface with no button down, the views under it
/// that listen hear `onMouseMove` again where it is, with the new
/// modifiers (an editor underlines a definition while Mod is held), as
/// they would on the next move.
pub(crate) fn modifiers_changed(surface: &Surface, modifiers: Modifiers) {
    let input = &surface.input;
    input.modifiers.set(modifiers);
    if input.button.get().is_some() || input.pointers.borrow().iter().any(|p| p.key == Key::Mouse) {
        return;
    }
    if let Some((x, y)) = input.last.get() {
        let hit = pick(&surface.root, x, y, true);
        mouse_moved(surface, hit.as_ref(), (x, y), modifiers, None);
    }
}

/// Picks the hovered views again under a mouse that did not move: after a
/// batch or a scroll moved views under it (or away), as on macOS.
pub(crate) fn rehover(surface: &Surface) {
    let input = &surface.input;
    if input.hover_tracked.borrow().is_empty() && input.hovered.borrow().is_empty() {
        return;
    }
    let Some((x, y)) = input.last.get() else {
        return;
    };
    let views = match pick(&surface.root, x, y, true) {
        Some(hit) => chain(&hit.view).into_iter().filter(hoverable).collect(),
        None => Vec::new(),
    };
    set_hovered(
        surface,
        views,
        (x, y),
        input.modifiers.get(),
        input.button.get(),
    );
}

/// The hosted item under the mouse is `item`: the one it left hears so,
/// the one under it gets the move.
fn set_hover_item(surface: &Rc<Surface>, item: Option<Item>, at: (f32, f32), modifiers: Modifiers) {
    let old = std::mem::replace(&mut *surface.input.hover_item.borrow_mut(), item.clone());
    if let Some(old) = old
        && !item.as_ref().is_some_and(|i| i.same(&old))
    {
        old.send(surface, PointerKind::Leave, at, 0, modifiers);
    }
    if let Some(item) = item {
        item.send(surface, PointerKind::Move, at, 0, modifiers);
    }
}

// --- Wheel and touchpad scrolling ---------------------------------------------------

/// How a scroll event scrolls.
#[derive(Debug, Clone, Copy, PartialEq)]
enum WheelInput {
    /// A mouse wheel notch (points).
    Step,
    /// A precise (touchpad) scroll moved by points; `phased` when its
    /// gesture has phases (it ends by them rather than a pause).
    Touchpad { phased: bool },
    /// The precise scroll ended: by its phase (`fling`: it glides on with
    /// its velocity) or a pause.
    Stop { fling: bool },
}

/// The innermost scroll view at `(x, y)` that can move by `(dx, dy)`
/// (nested views fall through to outer ones at their edges). Scroll views
/// along the wheel's axis take it: a vertical wheel over a carousel
/// scrolls the page around it, as in browsers; horizontal views take
/// vertical wheels only where nothing around scrolls vertically.
fn scroll_target(surface: &Surface, x: f32, y: f32, dx: f32, dy: f32) -> Option<NshView> {
    let hit = pick(&surface.root, x, y, true)?;
    input::scroll_target(
        chain(&hit.view),
        Points(dx),
        Points(dy),
        |v| v.state().scroll.as_ref().map(|s| s.horizontal),
        |v| v.kind() == ViewKind::Scroll && crate::scroll::can_scroll(v, dx, dy),
    )
}

/// Whether `view` is still shown in its surface.
fn shown(view: &NshView) -> bool {
    if !view.attached() {
        return false;
    }
    let mut current = Some(view.clone());
    while let Some(v) = current {
        {
            let s = v.state();
            if s.style.hidden || s.concealed {
                return false;
            }
        }
        current = v.parent_view();
    }
    true
}

/// A scroll event at `(x, y)` (`time` in ms); returns whether a scroll
/// view took it.
fn wheel_input(
    surface: &Rc<Surface>,
    x: f32,
    y: f32,
    dx: f32,
    dy: f32,
    time: f64,
    kind: WheelInput,
) -> bool {
    let input = &surface.input;
    match kind {
        WheelInput::Step => {
            if dx == 0.0 && dy == 0.0 {
                return false;
            }
            // One scroll stays on its view, as in browsers: an inner view
            // passing under the pointer does not take it over, and the view
            // reaching its edge does not hand the rest to an outer one.
            let horizontal = dx.abs() > dy.abs();
            let latched = match &*input.latch.borrow() {
                Some((view, h, last))
                    if latch_holds(*h, *last, horizontal, time) && shown(view) =>
                {
                    Some(view.clone())
                }
                _ => None,
            };
            let Some(view) = latched.or_else(|| scroll_target(surface, x, y, dx, dy)) else {
                input.latch.borrow_mut().take();
                return false;
            };
            *input.latch.borrow_mut() = Some((view.clone(), horizontal, time));
            // Without a begin, the scroll module takes it as a mouse wheel:
            // it settles (and snaps) by itself once the notches stop.
            crate::scroll::wheel(surface, &view, dx, dy);
            true
        }
        WheelInput::Touchpad { phased } => {
            if dx == 0.0 && dy == 0.0 {
                return input.wheel.borrow().is_some();
            }
            if input.wheel.borrow().is_none() && swallow_momentum(input, time) {
                return true;
            }
            // After a hold the scroll goes where it points: the held view
            // keeps it only if it scrolls that way.
            let held = match &*input.wheel.borrow() {
                Some(w) if w.held => Some(w.view.clone()),
                _ => None,
            };
            if let Some(held) = held {
                if scroll_target(surface, x, y, dx, dy).as_ref() == Some(&held) {
                    *input.wheel.borrow_mut() = Some(Wheel::new(held, time, phased));
                } else {
                    input.wheel.borrow_mut().take();
                    crate::scroll::wheel_end(surface, &held, 0.0, 0.0);
                }
            }
            if input.wheel.borrow().is_none() {
                let Some(view) = scroll_target(surface, x, y, dx, dy) else {
                    return false;
                };
                *input.wheel.borrow_mut() = Some(Wheel::new(view.clone(), time, phased));
                crate::scroll::wheel_begin(surface, &view);
            }
            let view = {
                let mut wheel = input.wheel.borrow_mut();
                let Some(w) = wheel.as_mut() else {
                    return false;
                };
                w.last = time;
                w.total.0 += dx;
                w.total.1 += dy;
                w.velocity.add(
                    time,
                    nsh_core::geometry::point(Points(w.total.0), Points(w.total.1)),
                );
                w.view.clone()
            };
            if !phased {
                watch_touchpad(surface);
            }
            crate::scroll::wheel(surface, &view, dx, dy);
            true
        }
        WheelInput::Stop { fling } => {
            let Some(mut w) = input.wheel.borrow_mut().take() else {
                return false;
            };
            // A gesture that ended by its phase (fingers lifted) glides on
            // with its velocity, and the system's momentum that follows is
            // swallowed; one that ended by a pause glides no more (Windows'
            // own inertia comes as more deltas, which it took).
            let (vx, vy) = if fling && !w.held {
                w.velocity.add(
                    time,
                    nsh_core::geometry::point(Points(w.total.0), Points(w.total.1)),
                );
                input.momentum.set(Some(time));
                w.velocity.velocity()
            } else {
                (0.0, 0.0)
            };
            crate::scroll::wheel_end(surface, &w.view, vx, vy);
            true
        }
    }
}

/// Whether a precise scroll delta at `time` (ms), with no gesture going
/// on, is the system's momentum after a gesture that ended by its phase
/// (which glides by itself): deltas that keep coming after it are.
fn swallow_momentum(input: &Input, time: f64) -> bool {
    match input.momentum.get() {
        Some(last) if time - last < TOUCHPAD_IDLE => {
            input.momentum.set(Some(time));
            true
        }
        _ => {
            input.momentum.set(None);
            false
        }
    }
}

/// The phase of a touchpad gesture (GPUI's `TouchPhase`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GesturePhase {
    /// Fingers touched (the event may have no delta yet).
    Began,
    /// They moved (or the system's momentum moved, with no phase).
    Moved,
    /// They lifted.
    Ended,
    /// The system took the gesture.
    Cancelled,
}

/// A touchpad scroll event in `phase`: [`wheel`] with the gesture's begin
/// and end. Fingers resting on the touchpad stop a glide; lifting them
/// glides on with the gesture's velocity. Touchpads whose events have no
/// phases (every event [`GesturePhase::Moved`]) end once they pause.
pub(crate) fn touchpad(surface: &Rc<Surface>, event: WheelEvent, phase: GesturePhase) {
    let input = &surface.input;
    let time = event.time * 1000.0;
    let (x, y) = (event.x, event.y);
    match phase {
        GesturePhase::Began => {
            input.momentum.set(None);
            wheel_input(
                surface,
                x,
                y,
                0.0,
                0.0,
                time,
                WheelInput::Stop { fling: false },
            );
            if event.dx == 0.0 && event.dy == 0.0 {
                touchpad_hold(surface, x, y, time);
                return;
            }
        }
        GesturePhase::Moved => {
            // A held gesture whose fingers lifted without scrolling (the
            // system cancelled it: GPUI sends a phaseless empty event).
            let held = input.wheel.borrow().as_ref().is_some_and(|w| w.held);
            if held && event.dx == 0.0 && event.dy == 0.0 {
                wheel_input(
                    surface,
                    x,
                    y,
                    0.0,
                    0.0,
                    time,
                    WheelInput::Stop { fling: false },
                );
                return;
            }
        }
        GesturePhase::Ended | GesturePhase::Cancelled => {}
    }
    let phased =
        phase != GesturePhase::Moved || input.wheel.borrow().as_ref().is_some_and(|w| w.phased);
    if event.modifiers.ctrl && event.dy != 0.0 && wheel_zoom(surface, x, y, event.dy) {
        return;
    }
    wheel_input(
        surface,
        x,
        y,
        event.dx,
        event.dy,
        time,
        WheelInput::Touchpad { phased },
    );
    if matches!(phase, GesturePhase::Ended | GesturePhase::Cancelled) {
        let fling = phase == GesturePhase::Ended;
        wheel_input(surface, x, y, 0.0, 0.0, time, WheelInput::Stop { fling });
    }
}

/// A touchpad pinch event in `phase` at `(x, y)`: `delta` is GPUI's (the
/// change of the scale: relative to the last one on macOS and Windows, of
/// the gesture's absolute scale on Linux). Pinches the view under the
/// pointer that pinches, from the gesture's begin to its end.
pub(crate) fn touchpad_pinch(
    surface: &Rc<Surface>,
    x: f32,
    y: f32,
    delta: f32,
    phase: GesturePhase,
) {
    let input = &surface.input;
    if phase == GesturePhase::Began {
        if let Some((old, _)) = input.pinch.borrow_mut().take() {
            transform_end(surface, &old);
        }
        let Some(hit) = pick(&surface.root, x, y, true) else {
            return;
        };
        let view = chain(&hit.view).into_iter().find(|v| {
            let s = v.state();
            transforms(&gesture_values(surface, v, s.id), s.events).0
        });
        let Some(view) = view else { return };
        let mut t = transform_view(surface, &view);
        t.rotation = false;
        transform_begin(surface, &t);
        *input.pinch.borrow_mut() = Some((t, 1.0));
    }
    if delta != 0.0 {
        let t = {
            let mut pinch = input.pinch.borrow_mut();
            let Some((t, absolute)) = pinch.as_mut() else {
                return;
            };
            *absolute = pinch_scale(t.scale, *absolute, delta);
            t.scale = *absolute;
            t.clone()
        };
        transform_move(surface, &t);
    }
    if matches!(phase, GesturePhase::Ended | GesturePhase::Cancelled)
        && let Some((t, _)) = input.pinch.borrow_mut().take()
    {
        transform_end(surface, &t);
    }
}

/// The scale of a touchpad pinch at `scale` (its absolute scale so far
/// `absolute`) after GPUI's `delta`.
fn pinch_scale(scale: f32, absolute: f32, delta: f32) -> f32 {
    let next = if cfg!(target_os = "linux") {
        // The platform's absolute scale changed by `delta`.
        absolute + delta
    } else {
        scale * (1.0 + delta)
    };
    next.max(0.01)
}

/// Fingers rest on the touchpad at `(x, y)`: the scroll view under them
/// stops gliding, until they scroll or lift.
fn touchpad_hold(surface: &Rc<Surface>, x: f32, y: f32, time: f64) {
    let Some(hit) = pick(&surface.root, x, y, true) else {
        return;
    };
    let Some(view) = chain(&hit.view)
        .into_iter()
        .find(|v| v.kind() == ViewKind::Scroll)
    else {
        return;
    };
    let mut wheel = Wheel::new(view.clone(), time, true);
    wheel.held = true;
    *surface.input.wheel.borrow_mut() = Some(wheel);
    crate::scroll::wheel_begin(surface, &view);
}

/// Watches the precise scroll for the pause that ends it.
fn watch_touchpad(surface: &Rc<Surface>) {
    if surface.input.wheel_watch.replace(true) {
        return;
    }
    check_touchpad(surface, TOUCHPAD_IDLE);
}

fn check_touchpad(surface: &Rc<Surface>, ms: f64) {
    let weak = Rc::downgrade(surface);
    after_ms(ms.ceil() as u64, move || {
        let Some(surface) = weak.upgrade() else {
            return;
        };
        let now = surface.clock() * 1000.0;
        // Phased gestures end by their phases (fingers may rest a while).
        let last = surface
            .input
            .wheel
            .borrow()
            .as_ref()
            .filter(|w| !w.phased)
            .map(|w| w.last);
        match touchpad_wait(last, now) {
            Some(wait) => check_touchpad(&surface, wait),
            None => {
                surface.input.wheel_watch.set(false);
                if last.is_some() {
                    let stop = WheelInput::Stop { fling: false };
                    wheel_input(&surface, 0.0, 0.0, 0.0, 0.0, now, stop);
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use nsh_core::{EdgeInsets, Points, TransformKind};

    use super::*;
    use crate::view::Hosted;

    fn view(
        parent: &NshView,
        id: NodeId,
        kind: ViewKind,
        frame: nsh_core::Bounds<Points>,
    ) -> NshView {
        let v = NshView::new(id, kind);
        v.state_mut().frame = frame;
        v.insert_before(parent, None);
        v
    }

    fn root() -> NshView {
        let root = NshView::new(ROOT_ID, ViewKind::View);
        root.state_mut().frame = crate::geom::rect(0.0, 0.0, 400.0, 400.0);
        root.set_invalidate(Rc::new(|| {}));
        root
    }

    fn at(root: &NshView, x: f32, y: f32) -> Option<NodeId> {
        pick(root, x, y, true).map(|h| h.view.id())
    }

    use crate::geom::rect;

    #[test]
    fn picks_the_innermost_topmost_view() {
        let root = root();
        let a = view(&root, 2, ViewKind::View, rect(10.0, 10.0, 100.0, 100.0));
        view(&a, 4, ViewKind::View, rect(10.0, 10.0, 20.0, 20.0));
        // Above `a`, overlapping it.
        view(&root, 6, ViewKind::View, rect(80.0, 80.0, 50.0, 50.0));
        assert_eq!(at(&root, 25.0, 25.0), Some(4));
        assert_eq!(at(&root, 50.0, 50.0), Some(2));
        assert_eq!(at(&root, 90.0, 90.0), Some(6));
        assert_eq!(at(&root, 300.0, 300.0), Some(ROOT_ID));
    }

    #[test]
    fn hidden_and_concealed_views_are_not_hit() {
        let root = root();
        let a = view(&root, 2, ViewKind::View, rect(0.0, 0.0, 100.0, 100.0));
        a.state_mut().style.hidden = true;
        let b = view(&root, 4, ViewKind::View, rect(200.0, 0.0, 100.0, 100.0));
        b.state_mut().concealed = true;
        assert_eq!(at(&root, 50.0, 50.0), Some(ROOT_ID));
        assert_eq!(at(&root, 250.0, 50.0), Some(ROOT_ID));
    }

    #[test]
    fn pointer_events() {
        let root = root();
        let a = view(&root, 2, ViewKind::View, rect(0.0, 0.0, 100.0, 100.0));
        view(&a, 4, ViewKind::View, rect(0.0, 0.0, 50.0, 50.0));
        a.state_mut().props.pointer_events = PointerEvents::BoxNone;
        assert_eq!(at(&root, 25.0, 25.0), Some(4));
        assert_eq!(at(&root, 75.0, 75.0), Some(ROOT_ID));
        a.state_mut().props.pointer_events = PointerEvents::BoxOnly;
        assert_eq!(at(&root, 25.0, 25.0), Some(2));
        a.state_mut().props.pointer_events = PointerEvents::None;
        assert_eq!(at(&root, 25.0, 25.0), Some(ROOT_ID));
    }

    #[test]
    fn hit_slop_extends_the_view() {
        let root = root();
        let a = view(&root, 2, ViewKind::View, rect(100.0, 100.0, 20.0, 20.0));
        a.state_mut().props.hit_slop = EdgeInsets {
            top: Points(10.0),
            right: Points(10.0),
            bottom: Points(10.0),
            left: Points(10.0),
        };
        assert_eq!(at(&root, 95.0, 125.0), Some(2));
        assert_eq!(at(&root, 85.0, 125.0), Some(ROOT_ID));
    }

    #[test]
    fn overflow_hidden_clips_and_overflow_areas_extend() {
        let root = root();
        let a = view(&root, 2, ViewKind::View, rect(0.0, 0.0, 100.0, 100.0));
        // A dropdown below its field.
        view(&a, 4, ViewKind::View, rect(0.0, 100.0, 100.0, 50.0));
        assert_eq!(at(&root, 50.0, 120.0), Some(ROOT_ID));
        a.state_mut().overflow = Some(rect(0.0, 0.0, 100.0, 150.0));
        assert_eq!(at(&root, 50.0, 120.0), Some(4));
        a.state_mut().style.overflow_hidden = true;
        assert_eq!(at(&root, 50.0, 120.0), Some(ROOT_ID));
    }

    #[test]
    fn scroll_offsets_and_mirrored_lists() {
        let root = root();
        let s = view(&root, 2, ViewKind::Scroll, rect(0.0, 0.0, 200.0, 200.0));
        view(&s, 4, ViewKind::View, rect(0.0, 300.0, 200.0, 50.0));
        assert_eq!(at(&root, 100.0, 20.0), Some(2));
        s.state_mut().scroll.as_mut().unwrap().y = Points(290.0);
        assert_eq!(at(&root, 100.0, 20.0), Some(4));
        let m = view(&root, 6, ViewKind::Scroll, rect(0.0, 250.0, 200.0, 100.0));
        view(&m, 8, ViewKind::View, rect(0.0, 0.0, 50.0, 100.0));
        m.state_mut().scroll.as_mut().unwrap().mirrored = true;
        assert_eq!(at(&root, 175.0, 300.0), Some(8));
        assert_eq!(at(&root, 25.0, 300.0), Some(6));
    }

    #[test]
    fn transforms_move_the_hit_area() {
        let root = root();
        let a = view(&root, 2, ViewKind::View, rect(0.0, 0.0, 100.0, 100.0));
        a.state_mut()
            .style
            .transform
            .push(TransformKind::TranslateX, 200.0);
        assert_eq!(at(&root, 50.0, 50.0), Some(ROOT_ID));
        assert_eq!(at(&root, 250.0, 50.0), Some(2));
        // Scaled around its center: 50..150 becomes 0..200.
        let b = view(&root, 4, ViewKind::View, rect(50.0, 250.0, 100.0, 100.0));
        b.state_mut()
            .style
            .transform
            .push(TransformKind::Scale, 2.0);
        assert_eq!(at(&root, 10.0, 210.0), Some(4));
        // Rotated in 3D with perspective: still hit at its middle, but not
        // at the corner that turned away.
        let c = view(&root, 6, ViewKind::View, rect(250.0, 250.0, 100.0, 100.0));
        {
            let mut s = c.state_mut();
            s.style.transform.push(TransformKind::Perspective, 300.0);
            s.style.transform.push(TransformKind::RotateY, 60.0);
        }
        assert_eq!(at(&root, 300.0, 300.0), Some(6));
        assert_eq!(at(&root, 252.0, 300.0), Some(ROOT_ID));
    }

    struct Square;

    impl Hosted for Square {
        fn draw(&self, _cx: &mut crate::render::DrawCx<'_>, _size: (f32, f32)) {}
    }

    #[test]
    fn hosted_items_are_above_children() {
        let root = root();
        let a = view(&root, 2, ViewKind::View, rect(0.0, 0.0, 100.0, 100.0));
        view(&a, 4, ViewKind::View, rect(0.0, 0.0, 100.0, 100.0));
        let item: Rc<dyn Hosted> = Rc::new(Square);
        a.set_native(&item, rect(0.0, 0.0, 50.0, 50.0));
        let hit = pick(&root, 25.0, 25.0, true).unwrap();
        assert_eq!(hit.view.id(), 2);
        assert!(hit.item.is_some_and(|n| Rc::ptr_eq(&n.item, &item)));
        let below = pick(&root, 25.0, 25.0, false).unwrap();
        assert_eq!(below.view.id(), 4);
        assert!(below.item.is_none());
        assert_eq!(at(&root, 75.0, 75.0), Some(4));
    }

    #[test]
    fn local_points_go_through_the_chain() {
        let root = root();
        let s = view(&root, 2, ViewKind::Scroll, rect(10.0, 10.0, 200.0, 200.0));
        s.state_mut().scroll.as_mut().unwrap().y = Points(40.0);
        let c = view(&s, 4, ViewKind::View, rect(5.0, 50.0, 50.0, 50.0));
        let (x, y) = local(&c, 20.0, 30.0);
        assert!((x - 5.0).abs() < 1e-4 && (y - 10.0).abs() < 1e-4, "{x} {y}");
    }

    #[test]
    fn cursors() {
        use nsh_core::Cursor as C;
        assert_eq!(css_cursor(C::Auto), None);
        assert_eq!(css_cursor(C::Default), Some(Cursor::Arrow));
        assert_eq!(css_cursor(C::Pointer), Some(Cursor::Hand));
        assert_eq!(css_cursor(C::NotAllowed), Some(Cursor::NotAllowed));
        assert_eq!(css_cursor(C::None), Some(Cursor::Hidden));
    }

    #[test]
    fn ctrl_wheel_zooms_by_notches() {
        assert!((zoom_factor(-ZOOM_NOTCH.0) - 1.1).abs() < 1e-5);
        assert!((zoom_factor(2.0 * ZOOM_NOTCH.0) - 1.0 / 1.21).abs() < 1e-5);
    }

    fn move_to(surface: &Rc<Surface>, x: f32, y: f32) {
        pointer(
            surface,
            PointerEvent {
                kind: PointerKind::Move,
                x,
                y,
                button: 0,
                pointer: 1,
                touch: false,
                pen: false,
                time: 1.0,
                modifiers: Modifiers::default(),
                clicks: 0,
                pressed_button: None,
            },
        );
    }

    fn hovered_ids(surface: &Surface) -> Vec<NodeId> {
        surface
            .input
            .hovered
            .borrow()
            .iter()
            .map(NshView::id)
            .collect()
    }

    #[test]
    fn hover_listeners_are_hovered_along_the_chain() {
        let surface = Surface::new(nsh_engine::Environment::default());
        surface.root.state_mut().frame = rect(0.0, 0.0, 400.0, 400.0);
        let outer = view(
            &surface.root,
            2,
            ViewKind::View,
            rect(0.0, 0.0, 200.0, 200.0),
        );
        let inner = view(&outer, 4, ViewKind::View, rect(10.0, 10.0, 50.0, 50.0));
        // Neither listens nor styles hovers: never hovered.
        let quiet = view(&outer, 6, ViewKind::View, rect(100.0, 100.0, 50.0, 50.0));
        outer.state_mut().events.set(EventKind::MouseLeave, true);
        events_changed(&surface, &outer);
        inner.state_mut().events.set(EventKind::MouseEnter, true);
        events_changed(&surface, &inner);

        move_to(&surface, 20.0, 20.0);
        assert_eq!(hovered_ids(&surface), vec![4, 2]);
        move_to(&surface, 120.0, 120.0);
        assert_eq!(hovered_ids(&surface), vec![2]);
        move_to(&surface, 300.0, 300.0);
        assert!(hovered_ids(&surface).is_empty());

        // A listener that stops listening is no longer hovered.
        move_to(&surface, 20.0, 20.0);
        inner.state_mut().events.set(EventKind::MouseEnter, false);
        events_changed(&surface, &inner);
        assert_eq!(hovered_ids(&surface), vec![2]);

        // A move listener alone has its view hovered.
        quiet.state_mut().events.set(EventKind::MouseMove, true);
        events_changed(&surface, &quiet);
        move_to(&surface, 120.0, 120.0);
        assert_eq!(hovered_ids(&surface), vec![6, 2]);
    }

    #[test]
    fn a_batch_rehovers_under_a_still_mouse() {
        let surface = Surface::new(nsh_engine::Environment::default());
        surface.root.state_mut().frame = rect(0.0, 0.0, 400.0, 400.0);
        move_to(&surface, 20.0, 20.0);
        assert!(hovered_ids(&surface).is_empty());
        // A listening view appears under the mouse without it moving.
        let v = view(
            &surface.root,
            2,
            ViewKind::View,
            rect(0.0, 0.0, 100.0, 100.0),
        );
        v.state_mut().events.set(EventKind::MouseEnter, true);
        events_changed(&surface, &v);
        rehover(&surface);
        assert_eq!(hovered_ids(&surface), vec![2]);
    }

    /// A surface with a 300 × 400 vertical scroll view over 2000 points.
    fn scrolling_surface() -> (Rc<Surface>, NshView) {
        let surface = Surface::new(nsh_engine::Environment::default());
        surface.root.state_mut().frame = rect(0.0, 0.0, 300.0, 400.0);
        let scroll = view(
            &surface.root,
            2,
            ViewKind::Scroll,
            rect(0.0, 0.0, 300.0, 400.0),
        );
        {
            let mut state = scroll.state_mut();
            let s = state.scroll.as_mut().unwrap();
            s.content = nsh_core::geometry::size(Points(300.0), Points(2000.0));
            s.behavior.enabled = true;
        }
        (surface, scroll)
    }

    fn pad(surface: &Rc<Surface>, dy: f32, time: f64, phase: GesturePhase) {
        let event = WheelEvent {
            x: 100.0,
            y: 100.0,
            dx: 0.0,
            dy,
            precise: true,
            modifiers: Modifiers::default(),
            time,
        };
        touchpad(surface, event, phase);
    }

    fn offset(view: &NshView) -> f32 {
        view.state().scroll.as_ref().unwrap().y.0
    }

    #[test]
    fn touchpad_phases_begin_and_end_the_gesture() {
        let (surface, scroll) = scrolling_surface();
        pad(&surface, 10.0, 1.0, GesturePhase::Began);
        assert!(
            surface
                .input
                .wheel
                .borrow()
                .as_ref()
                .is_some_and(|w| w.phased)
        );
        pad(&surface, 10.0, 1.016, GesturePhase::Moved);
        pad(&surface, 10.0, 1.032, GesturePhase::Moved);
        assert_eq!(offset(&scroll), 30.0);
        pad(&surface, 0.0, 1.040, GesturePhase::Ended);
        assert!(surface.input.wheel.borrow().is_none());
        // The system's momentum that follows is swallowed: the view glides
        // by itself.
        pad(&surface, 8.0, 1.056, GesturePhase::Moved);
        pad(&surface, 6.0, 1.072, GesturePhase::Moved);
        assert_eq!(offset(&scroll), 30.0);
        assert!(surface.input.wheel.borrow().is_none());
        // Deltas after a pause are a new scroll.
        pad(&surface, 5.0, 1.5, GesturePhase::Moved);
        assert!(
            surface
                .input
                .wheel
                .borrow()
                .as_ref()
                .is_some_and(|w| !w.phased)
        );
        assert_eq!(offset(&scroll), 35.0);
    }

    #[test]
    fn resting_fingers_hold_until_they_scroll_or_lift() {
        let (surface, scroll) = scrolling_surface();
        pad(&surface, 0.0, 1.0, GesturePhase::Began);
        assert!(
            surface
                .input
                .wheel
                .borrow()
                .as_ref()
                .is_some_and(|w| w.held)
        );
        // Lifted without scrolling (cancelled: a phaseless empty event).
        pad(&surface, 0.0, 1.1, GesturePhase::Moved);
        assert!(surface.input.wheel.borrow().is_none());
        pad(&surface, 0.0, 2.0, GesturePhase::Began);
        pad(&surface, 12.0, 2.016, GesturePhase::Moved);
        let wheel = surface.input.wheel.borrow();
        let w = wheel.as_ref().unwrap();
        assert!(w.phased && !w.held);
        assert_eq!(offset(&scroll), 12.0);
    }

    #[test]
    fn a_touchpad_fling_carries_its_velocity() {
        let mut w = Wheel::new(NshView::new(2, ViewKind::Scroll), 0.0, true);
        for i in 1..=6 {
            w.total.1 += 10.0;
            w.velocity.add(
                i as f64 * 10.0,
                nsh_core::geometry::point(Points(0.0), Points(w.total.1)),
            );
        }
        let (_, vy) = w.velocity.velocity();
        assert!((vy - 1000.0).abs() < 1.0, "{vy}");
    }

    #[test]
    fn touchpad_pinch_scales() {
        if cfg!(target_os = "linux") {
            // Deltas change the gesture's absolute scale.
            assert!((pinch_scale(1.0, 1.0, 0.25) - 1.25).abs() < 1e-6);
            assert!((pinch_scale(1.25, 1.25, 0.25) - 1.5).abs() < 1e-6);
        } else {
            assert!((pinch_scale(2.0, 2.0, 0.1) - 2.2).abs() < 1e-6);
        }
        assert_eq!(pinch_scale(0.02, 0.02, -5.0), 0.01);
    }
}
