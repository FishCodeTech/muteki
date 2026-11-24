use std::time::Duration;

use gpui::{App, Pixels, Point, TouchPhase, Window, px};
use gpui_base::motion::{Spring, spring};

// gpui-base has no edge gesture or interactive NavStack/Sheet progress API.
// Keep recognition shared here; focus, dismissal and settling remain base primitives.
#[derive(Default)]
pub(crate) struct BackProgress {
    pub travel: Pixels,
    pub velocity: f32,
    pub width: Pixels,
}

impl BackProgress {
    pub fn progress(&self) -> f32 {
        (f32::from(self.travel) / f32::from(self.width).max(1.)).clamp(0., 1.)
    }

    /// Whether the travel alone commits, whatever the release speed.
    pub fn past_threshold(&self) -> bool {
        self.travel >= self.width / 3.
    }

    pub fn commits(&self) -> bool {
        self.past_threshold() || (self.travel >= px(32.) && self.velocity >= 600.)
    }

    pub fn offset(progress: f32, extent: Pixels) -> Pixels {
        extent * progress.clamp(0., 1.)
    }
}

pub(crate) const SETTLE: Duration = Duration::from_millis(200);

pub(crate) fn settle(
    id: impl Into<gpui_base::motion::TransitionId>,
    progress: f32,
    dragging: bool,
    window: &mut Window,
    cx: &mut App,
) -> f32 {
    spring(
        id,
        progress,
        Spring::new(SETTLE).with_travel(!dragging),
        window,
        cx,
    )
}

/// GPUI flings every released pan, a swipe it does not scroll too, and spends
/// the next touch, a tap included, on catching that invisible fling. A touch
/// that starts and cancels at once at the pan's `origin` catches it instead.
pub(crate) fn catch_fling(origin: Point<Pixels>, window: &mut Window, cx: &mut App) {
    window.defer(cx, move |window, cx| {
        for phase in [TouchPhase::Started, TouchPhase::Cancelled] {
            window.dispatch_event(
                gpui::PlatformInput::Touch(gpui::TouchEvent {
                    id: gpui::TouchId(u64::MAX),
                    phase,
                    position: origin,
                    predicted_position: None,
                    force: None,
                }),
                cx,
            );
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edge_distance_and_flick_share_progress_and_commit_mapping() {
        for (travel, velocity, expected_progress, commit) in [
            (-10., 0., 0., false),
            (60., 100., 0.15, false),
            (132., 100., 0.33, false),
            (134., 0., 0.335, true),
            (31., 900., 0.0775, false),
            (32., 599., 0.08, false),
            (32., 600., 0.08, true),
            (500., 0., 1., true),
        ] {
            let swipe = BackProgress {
                travel: px(travel),
                velocity,
                width: px(400.),
            };
            assert!((swipe.progress() - expected_progress).abs() < 0.0001);
            assert_eq!(swipe.commits(), commit);
            assert!(
                (f32::from(BackProgress::offset(swipe.progress(), px(600.)))
                    - expected_progress * 600.)
                    .abs()
                    < 0.001
            );
        }
    }
}
