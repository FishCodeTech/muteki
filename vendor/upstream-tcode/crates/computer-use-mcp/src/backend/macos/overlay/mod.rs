mod cursor;
mod ffi;
mod geometry;
mod motion;

use std::cell::RefCell;
use std::ffi::c_void;
use std::sync::Mutex;
use std::sync::mpsc::{Receiver, sync_channel};
use std::time::{Duration, Instant};

use cursor::CursorUi;

use self::ffi::{class, dispatch_main, send_id, send_void};
use self::geometry::is_finite_point;
use self::motion::{Effect, Point, Waypoint};

static SUBMISSIONS: Mutex<Submissions> = Mutex::new(Submissions {
    enabled: false,
    revision: 0,
    owner: None,
});

// The producer and main queue share this ordering boundary. Invalidation cannot
// race a producer into enqueueing an obsolete action after the clear callback.
struct Submissions {
    enabled: bool,
    revision: u64,
    owner: Option<(u64, u64)>,
}

impl Submissions {
    fn reserve(
        &mut self,
        feedback: Option<crate::feedback::FeedbackTicket>,
        now: Instant,
    ) -> Publication {
        self.revision += 1;
        self.owner = feedback
            .as_ref()
            .map(|ticket| (ticket.owner, ticket.action));
        Publication {
            revision: self.revision,
            expires_at: now + ACTION_LIFETIME,
            feedback,
        }
    }

    fn accepts(&self, queued: &QueuedCommand, now: Instant) -> bool {
        queued.publication.revision == self.revision
            && (matches!(queued.command, UiCommand::Clear)
                || (self.enabled && queued.publication.is_current(now)))
    }

    fn clear(
        &mut self,
        owner: Option<u64>,
        action: Option<u64>,
        now: Instant,
    ) -> Option<QueuedCommand> {
        if let Some(owner) = owner
            && !self.owner.is_some_and(|current| {
                current.0 == owner && action.is_none_or(|action| current.1 == action)
            })
        {
            return None;
        }
        Some(QueuedCommand {
            command: UiCommand::Clear,
            publication: self.reserve(None, now),
        })
    }
}

#[derive(Clone, Debug)]
pub(crate) struct Publication {
    revision: u64,
    expires_at: Instant,
    feedback: Option<crate::feedback::FeedbackTicket>,
}

impl Publication {
    fn is_current(&self, now: Instant) -> bool {
        now < self.expires_at
            && self
                .feedback
                .as_ref()
                .is_none_or(|ticket| ticket.is_current())
    }
}

struct QueuedCommand {
    command: UiCommand,
    publication: Publication,
}

// Long enough for the cursor to rest between the agent's steps instead of
// blinking out, short enough that it retires soon after the agent stops.
const ACTION_LIFETIME: Duration = Duration::from_secs(8);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OverlayActionKind {
    Click,
    Point,
}

pub(crate) fn set_enabled(on: bool) {
    let mut submissions = SUBMISSIONS.lock().unwrap();
    let was_enabled = submissions.enabled;
    submissions.enabled = on;
    if was_enabled
        && !on
        && let Some(command) = submissions.clear(None, None, Instant::now())
    {
        enqueue(command);
    }
}

// Reserve before AX point lookup: an older, slow lookup cannot overwrite a
// newer action or a disable/cancel that arrived while the lookup was blocked.
pub(crate) fn begin_action(
    feedback: Option<crate::feedback::FeedbackTicket>,
) -> Option<Publication> {
    let mut submissions = SUBMISSIONS.lock().unwrap();
    (submissions.enabled && feedback.as_ref().is_none_or(|ticket| ticket.is_current()))
        .then(|| submissions.reserve(feedback, Instant::now()))
}

/// Released once the cursor has reached an action's point, or at once when
/// nothing will be drawn there, so input is delivered where the user just saw
/// the cursor arrive.
pub(crate) struct CursorArrival(Option<Receiver<()>>);

// Bounds the wait if the main queue is stalled; the cursor's longest planned
// path is well inside it.
const MAX_ARRIVAL_WAIT: Duration = Duration::from_secs(2);

impl CursorArrival {
    pub(crate) fn none() -> Self {
        Self(None)
    }

    pub(crate) fn wait(self) {
        if let Some(arrival) = self.0 {
            let _ = arrival.recv_timeout(MAX_ARRIVAL_WAIT);
        }
    }
}

fn awaited(waypoint: &mut Waypoint, paced: bool) -> CursorArrival {
    if !paced {
        return CursorArrival::none();
    }
    let (reached, arrival) = sync_channel(1);
    waypoint.reached = Some(reached);
    CursorArrival(Some(arrival))
}

/// An action without a point renews the cursor where it already is.
pub(crate) fn show_action(
    publication: Publication,
    window_id: u32,
    kind: OverlayActionKind,
    point: Option<Point>,
    paced: bool,
) -> CursorArrival {
    let Some(point) = point else {
        enqueue(QueuedCommand {
            publication,
            command: UiCommand::Show {
                window_id,
                point: None,
                waypoints: Vec::new(),
            },
        });
        return CursorArrival::none();
    };
    if !is_finite_point(point) {
        return CursorArrival::none();
    }
    let effect = (kind == OverlayActionKind::Click).then_some(Effect::Click);
    let mut waypoint = Waypoint::new(point, effect);
    let arrival = awaited(&mut waypoint, paced);
    enqueue(QueuedCommand {
        publication,
        command: UiCommand::Show {
            window_id,
            point: Some(point),
            waypoints: vec![waypoint],
        },
    });
    arrival
}

/// The returned arrival is the drag's start: the drag itself is delivered
/// while the cursor travels it.
pub(crate) fn show_drag(
    publication: Publication,
    window_id: u32,
    from: Point,
    to: Point,
    paced: bool,
) -> CursorArrival {
    if !is_finite_point(from) || !is_finite_point(to) {
        return CursorArrival::none();
    }
    let mut start = Waypoint::new(from, Some(Effect::Press));
    let arrival = awaited(&mut start, paced);
    enqueue(QueuedCommand {
        publication,
        command: UiCommand::Show {
            window_id,
            point: Some(to),
            waypoints: vec![start, Waypoint::new(to, Some(Effect::Release))],
        },
    });
    arrival
}

pub(crate) fn clear(owner: Option<u64>, action: Option<u64>) {
    let mut submissions = SUBMISSIONS.lock().unwrap();
    if let Some(command) = submissions.clear(owner, action, Instant::now()) {
        enqueue(command);
    }
}

enum UiCommand {
    Show {
        window_id: u32,
        point: Option<Point>,
        waypoints: Vec<Waypoint>,
    },
    Clear,
}

struct OverlayState {
    cursor: Option<CursorUi>,
    target: Option<Target>,
    poll_armed: bool,
    frame_armed: bool,
}

#[derive(Clone, Debug)]
struct Target {
    window_id: u32,
    point: Option<Point>,
    publication: Publication,
}

impl OverlayState {
    fn show(&mut self, target: Target, mut waypoints: Vec<Waypoint>, now: Instant) {
        let point = target.point.or_else(|| {
            self.target
                .as_ref()
                .filter(|current| current.window_id == target.window_id)
                .and_then(|current| current.point)
        });
        let Some(shows_point) = ffi::window_shows_point(target.window_id, point) else {
            self.clear(now);
            return;
        };
        self.set_target(Target { point, ..target });
        if !shows_point {
            // Nothing is drawn there, so the action need not wait for it.
            waypoints
                .iter_mut()
                .for_each(|waypoint| waypoint.reached = None);
        }
        if self.cursor.is_none() && !waypoints.is_empty() {
            self.cursor = CursorUi::new();
        }
        if let Some(cursor) = self.cursor.as_mut() {
            cursor.push(now, waypoints);
            cursor.set_shown(shows_point);
        }
        self.arm_frame();
    }

    fn set_target(&mut self, target: Target) {
        self.target = Some(target);
        if !self.poll_armed && ffi::dispatch_main_after(TARGET_POLL_INTERVAL_NS, poll_target) {
            self.poll_armed = true;
        }
    }

    /// `shows_point` is `None` once the target window has left the screen.
    fn refresh_visibility(&mut self, now: Instant, shows_point: Option<bool>) {
        let Some(target) = self.target.as_ref() else {
            return;
        };
        let Some(shows_point) = shows_point.filter(|_| target.publication.is_current(now)) else {
            self.clear(now);
            return;
        };
        if let Some(cursor) = self.cursor.as_mut() {
            cursor.set_shown(shows_point);
        }
        self.arm_frame();
    }

    fn clear(&mut self, now: Instant) {
        if let Some(cursor) = self.cursor.as_mut() {
            cursor.hide(now);
        }
        self.target = None;
        self.arm_frame();
    }

    fn arm_frame(&mut self) {
        if self.cursor.is_some()
            && !self.frame_armed
            && ffi::dispatch_main_after(FRAME_INTERVAL_NS, advance_frame)
        {
            self.frame_armed = true;
        }
    }
}

const TARGET_POLL_INTERVAL_NS: i64 = 200_000_000;
const FRAME_INTERVAL_NS: i64 = 16_666_667;

// This thread-local is intentionally read only by callbacks submitted to the
// process main queue. Objective-C window and layer pointers therefore never
// cross back into background-thread UI code.
thread_local! {
    static MAIN_STATE: RefCell<OverlayState> = const {
        RefCell::new(OverlayState {
            cursor: None,
            target: None,
            poll_armed: false,
            frame_armed: false,
        })
    };
}

fn enqueue(command: QueuedCommand) {
    let context = Box::into_raw(Box::new(command)).cast::<c_void>();
    if !dispatch_main(context, run_command) {
        // SAFETY: context came from Box::into_raw above and dispatch rejected it,
        // so no callback can own or free it.
        drop(unsafe { Box::from_raw(context.cast::<QueuedCommand>()) });
    }
}

// SAFETY: libdispatch calls this only with the Box<QueuedCommand> created by enqueue.
unsafe extern "C" fn run_command(context: *mut c_void) {
    if context.is_null() {
        return;
    }
    // SAFETY: enqueue passes exactly one Box<QueuedCommand> to a callback that
    // libdispatch invokes exactly once.
    let command = unsafe { Box::from_raw(context.cast::<QueuedCommand>()) };
    let submissions = SUBMISSIONS.lock().unwrap();
    if !submissions.accepts(&command, Instant::now()) {
        return;
    }

    let pool = class(c"NSAutoreleasePool").and_then(|pool| send_id(pool, c"new"));
    // sharedApplication initializes AppKit if needed; it does not activate the app.
    let _ =
        class(c"NSApplication").and_then(|application| send_id(application, c"sharedApplication"));
    let _ = MAIN_STATE.try_with(|state| {
        let Ok(mut state) = state.try_borrow_mut() else {
            return;
        };
        let now = Instant::now();
        match command.command {
            UiCommand::Show {
                window_id,
                point,
                waypoints,
            } => state.show(
                Target {
                    window_id,
                    point,
                    publication: command.publication.clone(),
                },
                waypoints,
                now,
            ),
            UiCommand::Clear => state.clear(now),
        }
    });
    if let Some(pool) = pool {
        let _ = send_void(pool, c"drain");
    }
}

// SAFETY: libdispatch calls this with the null context supplied when the poll is armed.
unsafe extern "C" fn poll_target(_context: *mut c_void) {
    let pool = class(c"NSAutoreleasePool").and_then(|pool| send_id(pool, c"new"));
    let _ = MAIN_STATE.try_with(|state| {
        let Ok(mut state) = state.try_borrow_mut() else {
            return;
        };
        state.poll_armed = false;
        let now = Instant::now();
        if SUBMISSIONS.lock().unwrap().enabled
            && let Some(target) = state.target.clone()
        {
            state.refresh_visibility(now, ffi::window_shows_point(target.window_id, target.point));
            if state.target.is_some()
                && ffi::dispatch_main_after(TARGET_POLL_INTERVAL_NS, poll_target)
            {
                state.poll_armed = true;
            }
        } else {
            state.clear(now);
        }
    });
    if let Some(pool) = pool {
        let _ = send_void(pool, c"drain");
    }
}

// SAFETY: libdispatch calls this with the null context supplied when a frame is armed.
unsafe extern "C" fn advance_frame(_context: *mut c_void) {
    let pool = class(c"NSAutoreleasePool").and_then(|pool| send_id(pool, c"new"));
    let _ = MAIN_STATE.try_with(|state| {
        let Ok(mut state) = state.try_borrow_mut() else {
            return;
        };
        state.frame_armed = false;
        let animating = state
            .cursor
            .as_mut()
            .is_some_and(|cursor| cursor.advance(Instant::now()));
        if animating {
            state.arm_frame();
        }
    });
    if let Some(pool) = pool {
        let _ = send_void(pool, c"drain");
    }
}

#[cfg(all(test, target_arch = "aarch64"))]
#[allow(dead_code)] // Entry point for the opt-in main-thread integration runner.
pub(crate) fn verify_native() {
    let _ = class(c"NSApplication").and_then(|app| send_id(app, c"sharedApplication"));
    let foreground = super::ax::frontmost_application_pid();
    let closed_window = cursor::verify_native();
    let mut state = OverlayState {
        cursor: None,
        target: None,
        poll_armed: true,
        frame_armed: false,
    };
    let target = Target {
        window_id: closed_window,
        point: Some((300.0, 200.0)),
        publication: Publication {
            revision: 1,
            expires_at: Instant::now() + ACTION_LIFETIME,
            feedback: None,
        },
    };
    state.show(
        target,
        vec![Waypoint::new((300.0, 200.0), None)],
        Instant::now(),
    );
    assert!(
        state.target.is_none() && state.cursor.is_none(),
        "a queued action reaching a closed native target must not recreate a marker"
    );
    assert_eq!(
        super::ax::frontmost_application_pid(),
        foreground,
        "overlay must not activate an app"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn action(publication: Publication) -> QueuedCommand {
        QueuedCommand {
            publication,
            command: UiCommand::Show {
                window_id: 7,
                point: Some((10.0, 20.0)),
                waypoints: Vec::new(),
            },
        }
    }
    fn submissions() -> Submissions {
        Submissions {
            enabled: true,
            revision: 0,
            owner: None,
        }
    }
    fn target(publication: Publication) -> Target {
        Target {
            window_id: 7,
            point: Some((10.0, 20.0)),
            publication,
        }
    }

    #[test]
    fn inactive_action_retires_target_before_visibility_refresh() {
        let now = Instant::now();
        let mut state = OverlayState {
            cursor: None,
            target: None,
            poll_armed: true,
            frame_armed: false,
        };
        state.set_target(target(submissions().reserve(None, now)));
        state.refresh_visibility(now, Some(true));
        assert!(state.target.is_some());
        let expired = now + ACTION_LIFETIME + Duration::from_secs(1);
        state.refresh_visibility(expired, Some(false));
        assert!(
            state.target.is_none(),
            "idle feedback must not reappear on visibility refresh"
        );
        state.refresh_visibility(expired + Duration::from_secs(1), Some(true));
        assert!(state.target.is_none());
    }

    #[test]
    fn newer_action_renews_lifetime_but_closed_window_retires_it() {
        let now = Instant::now();
        let mut state = OverlayState {
            cursor: None,
            target: None,
            poll_armed: true,
            frame_armed: false,
        };
        let mut submissions = submissions();
        state.set_target(target(submissions.reserve(None, now)));
        state.set_target(target(
            submissions.reserve(None, now + Duration::from_millis(500)),
        ));
        state.refresh_visibility(now + ACTION_LIFETIME, Some(false));
        assert!(
            state.target.is_some(),
            "a covered target does not retire a live action"
        );
        state.refresh_visibility(now + ACTION_LIFETIME, None);
        assert!(
            state.target.is_none(),
            "closed window retires feedback while its app stays open"
        );
    }

    #[test]
    fn delayed_callbacks_cannot_restore_cleared_superseded_or_expired_feedback() {
        let now = Instant::now();
        let mut submissions = submissions();
        let first = action(submissions.reserve(None, now));
        assert!(submissions.accepts(&first, now));
        let clear = submissions.clear(None, None, now).unwrap();
        assert!(!submissions.accepts(&first, now));
        assert!(submissions.accepts(&clear, now));
        let newer = action(submissions.reserve(None, now));
        assert!(
            !submissions.accepts(&clear, now),
            "late clear cannot erase newer feedback"
        );
        assert!(submissions.accepts(&newer, now));
        assert!(
            !submissions.accepts(&newer, now + ACTION_LIFETIME),
            "queue delay consumes lifetime"
        );
        submissions.enabled = false;
        let _disable = submissions.clear(None, None, now);
        submissions.enabled = true;
        assert!(
            !submissions.accepts(&newer, now),
            "re-enable cannot revive pre-disable work"
        );
    }

    #[test]
    fn cancelling_one_session_does_not_clear_another_sessions_newer_feedback() {
        let now = Instant::now();
        let mut submissions = submissions();
        let first = crate::feedback::FeedbackSession::new();
        let second = crate::feedback::FeedbackSession::new();
        let old_run = first.begin(None);
        let current_run = second.begin(None);
        let old = action(submissions.reserve(Some(old_run.ticket()), now));
        let current = action(submissions.reserve(Some(current_run.ticket()), now));
        first.cancel();
        assert!(!submissions.accepts(&old, now));
        assert!(
            submissions
                .clear(Some(old_run.ticket().owner), None, now)
                .is_none()
        );
        assert!(submissions.accepts(&current, now));
        second.cancel();
        assert!(
            !submissions.accepts(&current, now),
            "even an undelivered cancel callback invalidates the marker"
        );
        let restarted = second.begin(None);
        let current = action(submissions.reserve(Some(restarted.ticket()), now));
        assert!(
            submissions
                .clear(
                    Some(current_run.ticket().owner),
                    Some(current_run.ticket().action),
                    now
                )
                .is_none(),
            "a dropped older request cannot clear its successor"
        );
        assert!(submissions.accepts(&current, now));
        drop(second);
        assert!(
            !submissions.accepts(&current, now),
            "actual service teardown invalidates queued publication"
        );
    }
}
