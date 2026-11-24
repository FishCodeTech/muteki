//! Plays every agent-cursor effect in turn on the live desktop, drawing only
//! the overlay: no input reaches any application.
//!
//! `cargo run -p computer-use-mcp --example agent_cursor`

#[cfg(target_os = "macos")]
use computer_use_mcp::outline;

// The overlay's own modules, as siblings at this root; the demo drives the
// cursor alone, so the owner's submission and visibility code is not used.
#[cfg(target_os = "macos")]
#[allow(dead_code)]
#[path = "../src/backend/macos/overlay/cursor.rs"]
mod cursor;
#[cfg(target_os = "macos")]
#[allow(dead_code)]
#[path = "../src/backend/macos/overlay/ffi.rs"]
mod ffi;
#[cfg(target_os = "macos")]
#[allow(dead_code)]
#[path = "../src/backend/macos/overlay/geometry.rs"]
mod geometry;
#[cfg(target_os = "macos")]
#[allow(dead_code)]
#[path = "../src/backend/macos/overlay/motion.rs"]
mod motion;

#[cfg(target_os = "macos")]
mod overlay {
    use std::time::{Duration, Instant};

    use super::{ffi, motion};

    use super::cursor::CursorUi;
    use motion::{Effect, Waypoint};

    fn pump(cursor: &mut CursorUi, duration: Duration) {
        let end = Instant::now() + duration;
        while Instant::now() < end {
            // SAFETY: the demo owns the process main thread and its run loop.
            unsafe {
                core_foundation::runloop::CFRunLoopRunInMode(
                    core_foundation::runloop::kCFRunLoopDefaultMode,
                    0.008,
                    0,
                );
            }
            cursor.advance(Instant::now());
        }
    }

    pub fn run() {
        let _ = ffi::class(c"NSApplication")
            .and_then(|application| ffi::send_id(application, c"sharedApplication"));
        let display = ffi::display_frame_for_ax_point((1.0, 1.0)).expect("a display");
        let (w, h) = (display.ax.w, display.ax.h);
        let at = |x: f64, y: f64| (display.ax.x + w * x, display.ax.y + h * y);
        let mut cursor = CursorUi::new().expect("cursor panel");
        cursor.set_shown(true);
        let step = |cursor: &mut CursorUi, caption: &str, waypoints: Vec<Waypoint>, rest: u64| {
            println!("{caption}");
            cursor.push(Instant::now(), waypoints);
            pump(cursor, Duration::from_millis(rest));
        };

        step(
            &mut cursor,
            "1. appears on its first action point with a click (glow + ripple)",
            vec![Waypoint::new(at(0.5, 0.5), Some(Effect::Click))],
            1_500,
        );
        println!("2. glides: curved, eased, sometimes hesitating, looping or overshooting");
        for (x, y) in [
            (0.2, 0.25),
            (0.75, 0.3),
            (0.7, 0.75),
            (0.3, 0.7),
            (0.55, 0.45),
            (0.15, 0.5),
            (0.85, 0.55),
            (0.45, 0.2),
            (0.5, 0.8),
            (0.25, 0.35),
        ] {
            step(
                &mut cursor,
                "   glide",
                vec![Waypoint::new(at(x, y), None)],
                1_100,
            );
        }
        println!("3. clicks");
        for (x, y) in [(0.6, 0.35), (0.65, 0.4), (0.35, 0.6)] {
            step(
                &mut cursor,
                "   click",
                vec![Waypoint::new(at(x, y), Some(Effect::Click))],
                1_100,
            );
        }
        step(
            &mut cursor,
            "4. drag: shrinks, trails ripples, releases",
            vec![
                Waypoint::new(at(0.25, 0.3), Some(Effect::Press)),
                Waypoint::new(at(0.7, 0.65), Some(Effect::Release)),
            ],
            2_200,
        );
        step(
            &mut cursor,
            "5. a short drag",
            vec![
                Waypoint::new(at(0.6, 0.6), Some(Effect::Press)),
                Waypoint::new(at(0.45, 0.62), Some(Effect::Release)),
            ],
            1_800,
        );
        println!("6. three clicks arriving at once land in order, each on its own point");
        cursor.push(
            Instant::now(),
            vec![Waypoint::new(at(0.3, 0.4), Some(Effect::Click))],
        );
        cursor.push(
            Instant::now(),
            vec![Waypoint::new(at(0.5, 0.3), Some(Effect::Click))],
        );
        cursor.push(
            Instant::now(),
            vec![Waypoint::new(at(0.7, 0.4), Some(Effect::Click))],
        );
        pump(&mut cursor, Duration::from_millis(3_000));
        println!("7. idle retirement: fades out");
        cursor.hide(Instant::now());
        pump(&mut cursor, Duration::from_millis(1_200));
        println!("8. the next action fades in where it left off and glides on");
        cursor.set_shown(true);
        cursor.push(
            Instant::now(),
            vec![Waypoint::new(at(0.5, 0.6), Some(Effect::Click))],
        );
        pump(&mut cursor, Duration::from_millis(1_800));
        cursor.hide(Instant::now());
        pump(&mut cursor, Duration::from_millis(600));
        println!("done");
    }
}

fn main() {
    #[cfg(target_os = "macos")]
    overlay::run();
    #[cfg(not(target_os = "macos"))]
    println!("the agent cursor is macOS only");
}
