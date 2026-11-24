use std::ffi::CStr;
use std::ptr;
use std::time::{Duration, Instant};

use core_graphics::geometry::{CGPoint, CGRect, CGSize};

use super::ffi::{
    Id, class, display_frame_for_ax_point, send_id, send_id_cstr, send_id_f64, send_id_id,
    send_id_rect, send_id_window_init, send_void, send_void_bool, send_void_f32, send_void_f64,
    send_void_id, send_void_id_id, send_void_isize, send_void_point, send_void_rect,
    send_void_rect_bool, send_void_size, send_void_usize, status_window_level,
};
use super::geometry::ax_screen_to_appkit;
use super::motion::{Effect, Motion, Point, Waypoint};
use crate::outline::Frame;

// The pointer is a translucent disc whose centre is the action point.
const POINTER_RADIUS: f64 = 10.0;
const RIPPLE_RADIUS: f64 = 20.0;
const PRESSED_SCALE: f64 = 0.72;
const FADE_IN: Duration = Duration::from_millis(140);
const FADE_OUT: Duration = Duration::from_millis(240);
const MAX_FRAME_STEP: Duration = Duration::from_millis(50);
// Ripples pulse at this fixed rate for as long as a drag holds the button,
// whether or not the cursor is moving.
const TRAIL_INTERVAL: Duration = Duration::from_millis(33);
const RIPPLE_POOL: usize = 24;

const NS_WINDOW_STYLE_NONACTIVATING_PANEL: usize = 1 << 7;
const NS_BACKING_STORE_BUFFERED: usize = 2;
const NS_WINDOW_COLLECTION_BEHAVIOR: usize = (1 << 0) | (1 << 3) | (1 << 9);

#[derive(Clone, Copy)]
enum Ripple {
    Click,
    Trail,
}

impl Ripple {
    /// (from scale, to scale, from opacity, duration, line width)
    fn animation(self) -> (f64, f64, f64, f64, f64) {
        match self {
            Self::Click => (0.45, 1.7, 0.9, 0.5, 2.0),
            Self::Trail => (0.35, 0.95, 0.55, 0.38, 1.25),
        }
    }
}

/// One click-through panel covering the display the cursor is on. The pointer
/// and its ripples are layers on it, so a drag's ripples stay where they were
/// emitted while the pointer moves on.
pub(super) struct CursorUi {
    window: Id,
    root: Id,
    pointer: Id,
    glow: Id,
    accent: Id,
    ripples: Vec<(Id, Instant)>,
    panel: Option<Frame>,
    motion: Motion,
    shown: bool,
    alpha: f64,
    ordered: bool,
    position: Option<Point>,
    last_frame: Instant,
    next_trail: Option<Instant>,
}

impl CursorUi {
    /// Must only be called from the process main queue.
    pub(super) fn new() -> Option<Self> {
        let panel_class = class(c"NSPanel")?;
        let panel = send_id(panel_class, c"alloc")?;
        let window = send_id_window_init(
            panel,
            c"initWithContentRect:styleMask:backing:defer:",
            rect(0.0, 0.0, 1.0, 1.0),
            NS_WINDOW_STYLE_NONACTIVATING_PANEL,
            NS_BACKING_STORE_BUFFERED,
            false,
        )?;

        let clear = send_id(class(c"NSColor")?, c"clearColor")?;
        let configured = send_void_bool(window, c"setReleasedWhenClosed:", false)
            && send_void_bool(window, c"setIgnoresMouseEvents:", true)
            && send_void_bool(window, c"setOpaque:", false)
            && send_void_bool(window, c"setHasShadow:", false)
            && send_void_bool(window, c"setHidesOnDeactivate:", false)
            && send_void_id(window, c"setBackgroundColor:", clear)
            && send_void_usize(
                window,
                c"setCollectionBehavior:",
                NS_WINDOW_COLLECTION_BEHAVIOR,
            )
            && send_void_isize(window, c"setLevel:", status_window_level());
        if !configured {
            return None;
        }

        let view = send_id_rect(
            send_id(class(c"NSView")?, c"alloc")?,
            c"initWithFrame:",
            rect(0.0, 0.0, 1.0, 1.0),
        )?;
        if !send_void_bool(view, c"setWantsLayer:", true)
            || !send_void_id(window, c"setContentView:", view)
        {
            return None;
        }
        let root = send_id(view, c"layer")?;
        let accent = send_id(class(c"NSColor")?, c"controlAccentColor")?;
        let white = send_id(class(c"NSColor")?, c"whiteColor")?;
        let black = send_id(class(c"NSColor")?, c"blackColor")?;
        let disc = rect(0.0, 0.0, POINTER_RADIUS * 2.0, POINTER_RADIUS * 2.0);

        let pointer = send_id(class(c"CALayer")?, c"layer")?;
        let glow = send_id(class(c"CAShapeLayer")?, c"layer")?;
        let body = send_id(class(c"CAShapeLayer")?, c"layer")?;
        let configured = send_void_rect(pointer, c"setBounds:", disc)
            && send_void_f32(pointer, c"setOpacity:", 0.0)
            && send_void_rect(glow, c"setFrame:", disc)
            && send_void_id(
                glow,
                c"setPath:",
                circle_path(POINTER_RADIUS, POINTER_RADIUS)?,
            )
            && send_void_id(glow, c"setFillColor:", cg(accent, 0.85)?)
            && send_void_id(glow, c"setShadowColor:", cg(accent, 1.0)?)
            && send_void_f32(glow, c"setShadowOpacity:", 1.0)
            && send_void_f64(glow, c"setShadowRadius:", 12.0)
            && send_void_size(glow, c"setShadowOffset:", CGSize::new(0.0, 0.0))
            && send_void_f32(glow, c"setOpacity:", 0.0)
            && send_void_rect(body, c"setFrame:", disc)
            && send_void_id(
                body,
                c"setPath:",
                circle_path(POINTER_RADIUS, POINTER_RADIUS)?,
            )
            && send_void_id(body, c"setFillColor:", cg(accent, 0.42)?)
            && send_void_id(body, c"setStrokeColor:", cg(white, 0.75)?)
            && send_void_f64(body, c"setLineWidth:", 1.0)
            && send_void_id(body, c"setShadowColor:", cg(black, 1.0)?)
            && send_void_f32(body, c"setShadowOpacity:", 0.22)
            && send_void_f64(body, c"setShadowRadius:", 2.5)
            && send_void_size(body, c"setShadowOffset:", CGSize::new(0.0, -1.0))
            && send_void_id(pointer, c"addSublayer:", glow)
            && send_void_id(pointer, c"addSublayer:", body)
            && send_void_id(root, c"addSublayer:", pointer);
        if !configured {
            return None;
        }

        Some(Self {
            window,
            root,
            pointer,
            glow,
            accent,
            ripples: Vec::new(),
            panel: None,
            motion: Motion::new(seed()),
            shown: false,
            alpha: 0.0,
            ordered: false,
            position: None,
            last_frame: Instant::now(),
            next_trail: None,
        })
    }

    /// Must only be called from the process main queue.
    pub(super) fn push(&mut self, now: Instant, waypoints: Vec<Waypoint>) {
        for effect in self.motion.push(now, waypoints) {
            self.play(effect, now);
        }
        self.place(now);
    }

    /// Whether the target window currently shows the cursor's point. The
    /// cursor keeps moving while it is not shown, so it reappears in place.
    pub(super) fn set_shown(&mut self, shown: bool) {
        self.shown = shown;
    }

    /// Must only be called from the process main queue. The last position is
    /// kept, so the next action glides from it instead of popping in.
    pub(super) fn hide(&mut self, now: Instant) {
        self.motion.halt(now);
        self.shown = false;
        self.set_pressed(false);
    }

    /// Must only be called from the process main queue. Returns whether
    /// another frame is needed.
    pub(super) fn advance(&mut self, now: Instant) -> bool {
        let step = now
            .saturating_duration_since(self.last_frame)
            .min(MAX_FRAME_STEP);
        self.last_frame = now;
        for effect in self.motion.advance(now) {
            self.play(effect, now);
        }
        self.place(now);
        if !self.motion.is_pressed() {
            self.next_trail = None;
        } else if let Some(next) = self.next_trail.filter(|next| now >= *next) {
            self.ripple(Ripple::Trail, now);
            // Fixed cadence: a late frame does not shift later pulses.
            let mut following = next + TRAIL_INTERVAL;
            while following <= now {
                following += TRAIL_INTERVAL;
            }
            self.next_trail = Some(following);
        }

        let target = if self.shown && self.position.is_some() {
            1.0
        } else {
            0.0
        };
        if self.alpha != target {
            let fade = if target > self.alpha {
                FADE_IN
            } else {
                FADE_OUT
            };
            let delta = step.as_secs_f64() / fade.as_secs_f64();
            self.alpha = if target > self.alpha {
                (self.alpha + delta).min(target)
            } else {
                (self.alpha - delta).max(target)
            };
            without_implicit_animation(|| {
                let _ = send_void_f32(self.pointer, c"setOpacity:", self.alpha as f32);
            });
        }
        let rippling = self.ripples.iter().any(|(_, until)| now < *until);
        let needed = self.alpha > 0.0 || target > 0.0 || rippling;
        if needed && !self.ordered {
            let _ = send_void(self.window, c"orderFrontRegardless");
            self.ordered = true;
        } else if !needed && self.ordered {
            let _ = send_void_id(self.window, c"orderOut:", ptr::null_mut());
            self.ordered = false;
        }
        self.motion.is_moving() || self.motion.is_pressed() || self.alpha != target || rippling
    }

    fn place(&mut self, now: Instant) {
        let Some(point) = self.motion.position(now) else {
            return;
        };
        if self.position == Some(point) {
            return;
        }
        let Some(display) = display_frame_for_ax_point(point) else {
            return;
        };
        if self
            .panel
            .is_none_or(|panel| !same_frame(panel, display.appkit))
        {
            let frame = display.appkit;
            let _ = send_void_rect_bool(
                self.window,
                c"setFrame:display:",
                rect(frame.x, frame.y, frame.w, frame.h),
                false,
            );
            self.panel = Some(frame);
        }
        self.position = Some(point);
        let layer_point = self.layer_point(point);
        without_implicit_animation(|| {
            let _ = send_void_point(self.pointer, c"setPosition:", layer_point);
        });
    }

    fn layer_point(&self, point: Point) -> CGPoint {
        let panel = self.panel.unwrap_or(Frame {
            x: 0.0,
            y: 0.0,
            w: 0.0,
            h: 0.0,
        });
        let (x, y) = display_frame_for_ax_point(point)
            .map(|display| ax_screen_to_appkit(point, display))
            .unwrap_or(point);
        CGPoint::new(x - panel.x, y - panel.y)
    }

    fn play(&mut self, effect: Effect, now: Instant) {
        match effect {
            Effect::Click => {
                animate(self.glow, c"opacity", 1.0, 0.0, 0.45, false);
                animate(
                    self.pointer,
                    c"transform.scale",
                    1.0,
                    PRESSED_SCALE,
                    0.09,
                    true,
                );
                self.ripple(Ripple::Click, now);
            }
            Effect::Press => {
                self.set_pressed(true);
                self.ripple(Ripple::Trail, now);
                self.next_trail = Some(now + TRAIL_INTERVAL);
            }
            Effect::Release => {
                self.set_pressed(false);
                self.ripple(Ripple::Trail, now);
            }
        }
    }

    fn ripple(&mut self, kind: Ripple, now: Instant) {
        let Some(point) = self.position else {
            return;
        };
        let (from, to, opacity, duration, line_width) = kind.animation();
        let layer = match self.ripples.iter().position(|(_, until)| now >= *until) {
            Some(free) => Some(free),
            None if self.ripples.len() < RIPPLE_POOL => new_ripple(self.accent).map(|layer| {
                let _ = send_void_id(self.root, c"addSublayer:", layer);
                self.ripples.push((layer, now));
                self.ripples.len() - 1
            }),
            None => None,
        };
        let Some(index) = layer else {
            return;
        };
        let layer = self.ripples[index].0;
        self.ripples[index].1 = now + Duration::from_secs_f64(duration);
        let position = self.layer_point(point);
        without_implicit_animation(|| {
            let _ = send_void_point(layer, c"setPosition:", position)
                && send_void_f64(layer, c"setLineWidth:", line_width);
        });
        animate(layer, c"transform.scale", from, to, duration, false);
        animate(layer, c"opacity", opacity, 0.0, duration, false);
    }

    fn set_pressed(&self, pressed: bool) {
        let scale = if pressed { PRESSED_SCALE } else { 1.0 };
        if let (Some(value), Some(key)) = (ns_number(scale), ns_string(c"transform.scale")) {
            let _ = send_void_id_id(self.pointer, c"setValue:forKeyPath:", value, key);
        }
    }
}

fn new_ripple(accent: Id) -> Option<Id> {
    let layer = send_id(class(c"CAShapeLayer")?, c"layer")?;
    let size = RIPPLE_RADIUS * 2.0;
    let configured = send_void_rect(layer, c"setBounds:", rect(0.0, 0.0, size, size))
        && send_void_id(
            layer,
            c"setPath:",
            circle_path(RIPPLE_RADIUS, RIPPLE_RADIUS)?,
        )
        && send_void_id(layer, c"setFillColor:", cg(accent, 0.12)?)
        && send_void_id(layer, c"setStrokeColor:", cg(accent, 0.9)?)
        && send_void_f32(layer, c"setOpacity:", 0.0);
    configured.then_some(layer)
}

fn animate(layer: Id, key_path: &CStr, from: f64, to: f64, duration: f64, autoreverses: bool) {
    let animation = || {
        let key = ns_string(key_path)?;
        let animation = send_id_id(class(c"CABasicAnimation")?, c"animationWithKeyPath:", key)?;
        let timing = send_id_id(
            class(c"CAMediaTimingFunction")?,
            c"functionWithName:",
            ns_string(c"easeOut")?,
        )?;
        let configured = send_void_id(animation, c"setFromValue:", ns_number(from)?)
            && send_void_id(animation, c"setToValue:", ns_number(to)?)
            && send_void_f64(animation, c"setDuration:", duration)
            && send_void_bool(animation, c"setAutoreverses:", autoreverses)
            && send_void_id(animation, c"setTimingFunction:", timing);
        configured.then(|| send_void_id_id(layer, c"addAnimation:forKey:", animation, key))
    };
    let _ = animation();
}

// Position and fade are driven per frame; implicit layer animations would
// make them lag a quarter second behind.
fn without_implicit_animation(change: impl FnOnce()) {
    let transaction = class(c"CATransaction");
    let began = transaction.is_some_and(|transaction| {
        send_void(transaction, c"begin") && send_void_bool(transaction, c"setDisableActions:", true)
    });
    change();
    if let Some(transaction) = transaction.filter(|_| began) {
        let _ = send_void(transaction, c"commit");
    }
}

fn circle_path(center: f64, radius: f64) -> Option<Id> {
    let oval = rect(center - radius, center - radius, radius * 2.0, radius * 2.0);
    let path = send_id_rect(class(c"NSBezierPath")?, c"bezierPathWithOvalInRect:", oval)?;
    send_id(path, c"CGPath")
}

fn cg(color: Id, alpha: f64) -> Option<Id> {
    send_id(
        send_id_f64(color, c"colorWithAlphaComponent:", alpha)?,
        c"CGColor",
    )
}

fn ns_string(value: &CStr) -> Option<Id> {
    send_id_cstr(
        class(c"NSString")?,
        c"stringWithUTF8String:",
        value.as_ptr(),
    )
}

fn ns_number(value: f64) -> Option<Id> {
    send_id_f64(class(c"NSNumber")?, c"numberWithDouble:", value)
}

fn seed() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_nanos() as u64)
}

fn same_frame(a: Frame, b: Frame) -> bool {
    a.x == b.x && a.y == b.y && a.w == b.w && a.h == b.h
}

fn rect(x: f64, y: f64, width: f64, height: f64) -> CGRect {
    CGRect::new(&CGPoint::new(x, y), &CGSize::new(width, height))
}

// Called by the main-thread, opt-in test harness. Normal libtest workers cannot
// construct AppKit windows, so this regression has a dedicated native runner.
// The pump ticks the cursor the way the overlay's main-queue frame timer does.
#[cfg(all(test, target_arch = "aarch64"))]
#[allow(dead_code)] // Also compiled by ordinary libtest, which cannot run AppKit.
pub(super) fn verify_native() -> u32 {
    use super::ffi::{layer_position, native_panel_state, window_shows_point};
    fn pump(cursor: &mut CursorUi) {
        let end = Instant::now() + Duration::from_millis(2_000);
        while Instant::now() < end {
            // SAFETY: this runner owns the process main thread and its run loop.
            unsafe {
                core_foundation::runloop::CFRunLoopRunInMode(
                    core_foundation::runloop::kCFRunLoopDefaultMode,
                    0.016,
                    0,
                );
            }
            cursor.advance(Instant::now());
        }
    }
    fn assert_moved(cursor: &CursorUi, initial: CGPoint, by: (f64, f64), what: &str) {
        let now = layer_position(cursor.pointer);
        assert!(
            (now.x - initial.x - by.0).abs() < 1.0 && (now.y - initial.y - by.1).abs() < 1.0,
            "{what}: pointer at {now:?}, initial {initial:?}, expected offset {by:?}"
        );
    }
    let mut cursor = CursorUi::new().expect("native cursor panel");
    cursor.set_shown(true);
    cursor.push(
        Instant::now(),
        vec![Waypoint::new((100.0, 100.0), Some(Effect::Click))],
    );
    pump(&mut cursor);
    let initial = layer_position(cursor.pointer);
    cursor.push(Instant::now(), vec![Waypoint::new((300.0, 200.0), None)]);
    pump(&mut cursor);
    let (_, visible, passthrough, number) = native_panel_state(cursor.window);
    assert_moved(
        &cursor,
        initial,
        (200.0, -100.0),
        "a glide must reach its endpoint",
    );
    assert!(visible && passthrough);
    cursor.push(
        Instant::now(),
        vec![
            Waypoint::new((300.0, 200.0), Some(Effect::Press)),
            Waypoint::new((500.0, 300.0), Some(Effect::Release)),
        ],
    );
    pump(&mut cursor);
    assert_moved(&cursor, initial, (400.0, -200.0), "drag must reach its end");
    cursor.push(Instant::now(), vec![Waypoint::new((700.0, 100.0), None)]);
    cursor.hide(Instant::now());
    pump(&mut cursor);
    assert!(
        !native_panel_state(cursor.window).1,
        "a hidden cursor fades out and leaves the screen"
    );
    cursor.set_shown(true);
    cursor.push(
        Instant::now(),
        vec![Waypoint::new((200.0, 400.0), Some(Effect::Click))],
    );
    pump(&mut cursor);
    assert!(native_panel_state(cursor.window).1);
    assert_moved(
        &cursor,
        initial,
        (100.0, -300.0),
        "new feedback after hide must end on its own point",
    );
    assert!(window_shows_point(number, None).is_some());
    let _ = send_void(cursor.window, c"close");
    pump(&mut cursor);
    assert!(
        window_shows_point(number, None).is_none(),
        "closed but retained native window must retire target feedback"
    );
    number
}
