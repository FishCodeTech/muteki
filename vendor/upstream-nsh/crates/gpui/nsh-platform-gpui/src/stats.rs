//! Frame statistics of a surface ([`crate::AppOptions::fps`],
//! [`crate::AppView::set_fps_monitor`]): once a second, one `nsh::perf`
//! line of how often frames were drawn and what each cost on the main
//! thread.
//!
//! `fps 145 active 0/145 gap 6.94/6.94/7.10/7.85 tick … paint … canvas
//! 0.04 paths 26.0 raf 145 notifies 0`: frames drawn in the second, how
//! many of them in a window GPUI counts active (unfocused windows are drawn
//! at 30 fps unless their options say otherwise), the gaps between frames,
//! the animation tick of each (prepaint), the tree's paint and the share of
//! it rasterizing canvases (milliseconds as mean/p50/p95/max), paths
//! painted per frame (each batch of paths costs GPUI a viewport-sized
//! MSAA pass), animation frames requested and notifies sent to the view
//! (what made GPUI draw). GPUI's own drawing and presenting are not in
//! these: `ZED_MEASUREMENTS=1` logs them (`frame duration`).

use std::time::Instant;

/// Readings over the current second.
#[derive(Default)]
pub(crate) struct FrameStats {
    started: Option<Instant>,
    last: Option<Instant>,
    gaps: Vec<f64>,
    ticks: Vec<f64>,
    paints: Vec<f64>,
    canvas: f64,
    active: u32,
    paths: u32,
    requested: u32,
    notifies: u32,
}

impl FrameStats {
    /// The animation tick of a frame took since `start`.
    pub(crate) fn tick(&mut self, start: Instant) {
        self.ticks.push(ms(start));
    }

    /// The frame painted `paths` paths, `canvas` milliseconds of it
    /// rasterizing canvases.
    pub(crate) fn content(&mut self, paths: u32, canvas: f64) {
        self.paths += paths;
        self.canvas += canvas;
    }

    /// An animation frame was requested.
    pub(crate) fn requested(&mut self) {
        self.requested += 1;
    }

    /// The view was notified.
    pub(crate) fn notified(&mut self) {
        self.notifies += 1;
    }

    /// A frame's paint, begun at `start`, ended (in a window GPUI counts
    /// `active`): logs the second that ended with it.
    pub(crate) fn painted(&mut self, start: Instant, active: bool) {
        let now = Instant::now();
        self.paints.push(ms(start));
        if let Some(last) = self.last {
            self.gaps.push(now.duration_since(last).as_secs_f64() * 1e3);
        }
        self.last = Some(now);
        self.active += active as u32;
        let started = *self.started.get_or_insert(now);
        if now.duration_since(started).as_secs_f64() < 1.0 {
            return;
        }
        let n = self.paints.len();
        log::info!(
            target: "nsh::perf",
            "fps {n} active {}/{n} gap {} tick {} paint {} canvas {:.2} paths {:.1} raf {} notifies {}",
            self.active,
            summary(&mut self.gaps),
            summary(&mut self.ticks),
            summary(&mut self.paints),
            self.canvas / n as f64,
            self.paths as f64 / n as f64,
            self.requested,
            self.notifies,
        );
        *self = FrameStats {
            last: self.last,
            ..Default::default()
        };
    }
}

/// Milliseconds since `start`.
fn ms(start: Instant) -> f64 {
    start.elapsed().as_secs_f64() * 1e3
}

/// `mean/p50/p95/max` of `v` (milliseconds), or `-`.
fn summary(v: &mut [f64]) -> String {
    if v.is_empty() {
        return "-".into();
    }
    v.sort_by(f64::total_cmp);
    let n = v.len();
    let mean = v.iter().sum::<f64>() / n as f64;
    format!(
        "{mean:.2}/{:.2}/{:.2}/{:.2}",
        v[n / 2],
        v[(n * 95 / 100).min(n - 1)],
        v[n - 1]
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summaries_are_mean_and_percentiles() {
        let mut v = vec![4.0, 1.0, 3.0, 2.0];
        assert_eq!(summary(&mut v), "2.50/3.00/4.00/4.00");
        assert_eq!(summary(&mut []), "-");
    }

    #[test]
    fn a_second_is_logged_and_started_again() {
        let mut s = FrameStats::default();
        let start = Instant::now();
        s.requested();
        s.painted(start, true);
        assert_eq!((s.paints.len(), s.requested), (1, 1));
        s.started = Some(Instant::now() - std::time::Duration::from_secs(2));
        s.painted(start, false);
        // Logged: only the time of the last frame stays.
        assert!(s.paints.is_empty() && s.last.is_some() && s.requested == 0);
    }
}
