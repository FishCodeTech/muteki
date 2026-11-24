//! Frame demand shared by Tcode's mobile platform backends.

use std::time::{Duration, Instant};

const ACTIVITY_GRACE: Duration = Duration::from_millis(100);

#[derive(Default)]
pub struct FramePacer {
    pending: bool,
    touching: bool,
    active_until: Option<Instant>,
}

impl FramePacer {
    pub fn request(&mut self) {
        self.pending = true;
    }

    pub fn input(&mut self, now: Instant) {
        self.active_until = Some(now + ACTIVITY_GRACE);
        self.request();
    }

    pub fn set_touching(&mut self, touching: bool, now: Instant) {
        self.touching = touching;
        self.input(now);
    }

    pub fn begin_frame(&mut self, now: Instant) -> bool {
        std::mem::take(&mut self.pending) || self.active(now)
    }

    pub fn active(&self, now: Instant) -> bool {
        self.touching || self.active_until.is_some_and(|until| now < until)
    }

    pub fn end_frame(&mut self, now: Instant) -> bool {
        // Demand raised while drawing includes GPUI's next-frame animations.
        // Isolated timer invalidations (such as caret blink) get no grace.
        if self.pending {
            self.active_until = Some(now + ACTIVITY_GRACE);
        }
        self.pending || self.active(now)
    }

    pub fn suspend(&mut self) {
        self.touching = false;
        self.active_until = None;
        // Keep invalidation pending across surface loss and backgrounding.
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn isolated_refreshes_and_caret_blinks_return_to_pause() {
        let mut pacer = FramePacer::default();
        let start = Instant::now();
        assert!(!pacer.begin_frame(start));
        for blink in 0..8 {
            let now = start + Duration::from_millis(blink * 500);
            pacer.request();
            pacer.request();
            assert!(pacer.begin_frame(now));
            assert!(!pacer.end_frame(now));
            assert!(!pacer.begin_frame(now + Duration::from_millis(8)));
        }
    }

    #[test]
    fn animation_demand_keeps_max_rate_then_expires_after_grace() {
        let mut pacer = FramePacer::default();
        let start = Instant::now();
        pacer.request();
        for frame in 0..120 {
            let now = start + Duration::from_nanos(frame * 8_333_333);
            assert!(pacer.begin_frame(now));
            pacer.request();
            assert!(pacer.end_frame(now));
        }
        let last = start + Duration::from_secs(1);
        assert!(pacer.begin_frame(last));
        assert!(pacer.end_frame(last + Duration::from_millis(50)));
        assert!(!pacer.end_frame(last + Duration::from_millis(110)));
    }

    #[test]
    fn input_wakes_a_paused_source_and_touch_holds_it_until_release() {
        let mut pacer = FramePacer::default();
        let start = Instant::now();
        assert!(!pacer.end_frame(start));
        pacer.input(start);
        assert!(pacer.begin_frame(start));
        assert!(pacer.end_frame(start + Duration::from_millis(50)));
        assert!(!pacer.end_frame(start + Duration::from_millis(100)));
        let touch = start + Duration::from_secs(1);
        pacer.set_touching(true, touch);
        assert!(pacer.begin_frame(touch + Duration::from_secs(5)));
        pacer.set_touching(false, touch + Duration::from_secs(5));
        assert!(pacer.begin_frame(touch + Duration::from_secs(5)));
        assert!(!pacer.end_frame(touch + Duration::from_secs(6)));
    }

    #[test]
    fn backgrounding_releases_touch_without_losing_pending_refresh() {
        let mut pacer = FramePacer::default();
        let now = Instant::now();
        pacer.set_touching(true, now);
        pacer.suspend();
        assert!(pacer.begin_frame(now + Duration::from_secs(60)));
        assert!(!pacer.end_frame(now + Duration::from_secs(60)));
    }
}
