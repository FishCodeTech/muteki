//! Retry and foreground policy shared by browser and native transports.

#[derive(Default)]
pub struct Backoff {
    failures: u32,
    /// Consecutive failures since the last network change; drives the delay
    /// while `failures` keeps numbering attempts for the banner.
    exponent: u32,
}

impl Backoff {
    pub fn attempt(&self) -> u32 {
        self.failures.saturating_add(1)
    }

    /// `sample` is uniform in [0, 1]. The final delay, including jitter, is capped.
    pub fn failed(&mut self, stable_ms: u64, sample: f64) -> u64 {
        let base = if stable_ms >= 30_000 {
            self.failures = 0;
            self.exponent = 0;
            1_000
        } else {
            let base = (1_000_u64 << self.exponent.min(5)).min(30_000);
            self.failures = self.failures.saturating_add(1);
            self.exponent = self.exponent.saturating_add(1);
            base
        };
        ((base as f64 * (0.8 + 0.4 * sample.clamp(0., 1.))) as u64).min(30_000)
    }

    /// The network changed under the retry loop: the next failure waits the
    /// shortest delay again, while the attempt count keeps increasing so an
    /// observer can tell the retries apart.
    pub fn network_changed(&mut self) {
        self.exponent = 0;
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Wake {
    Reconnect,
    Probe,
}

#[derive(Default)]
pub struct Lifecycle {
    background_ms: Option<u64>,
}

impl Lifecycle {
    pub fn background(&mut self, now_ms: u64) {
        self.background_ms.get_or_insert(now_ms);
    }

    /// Consumes the background interval: Foreground followed by Active probes once.
    pub fn foreground(&mut self, now_ms: u64) -> Option<Wake> {
        self.background_ms.take().map(|start| {
            if now_ms.saturating_sub(start) >= 10_000 {
                Wake::Reconnect
            } else {
                Wake::Probe
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retry_jitter_stays_bounded_and_flapping_does_not_reset() {
        for sample in [0., 0.5, 1.] {
            let mut backoff = Backoff::default();
            for base in [1000, 2000, 4000, 8000, 16000, 30000, 30000] {
                let delay = backoff.failed(29_999, sample);
                assert!((base * 8 / 10..=(base * 12 / 10).min(30000)).contains(&delay));
            }
            assert_eq!(backoff.attempt(), 8);
            assert!((800..=1200).contains(&backoff.failed(30_000, sample)));
            assert_eq!(backoff.attempt(), 1);
            assert!((800..=1200).contains(&backoff.failed(0, sample)));
            assert_eq!(backoff.attempt(), 2);
        }
    }

    #[test]
    fn network_change_shortens_the_next_delay_without_restarting_attempt_numbers() {
        let mut backoff = Backoff::default();
        for _ in 0..4 {
            backoff.failed(0, 0.5);
        }
        assert_eq!(backoff.attempt(), 5);
        backoff.network_changed();
        assert_eq!(backoff.attempt(), 5);
        assert!((800..=1200).contains(&backoff.failed(0, 0.5)));
        assert_eq!(backoff.attempt(), 6);
        assert!((1600..=2400).contains(&backoff.failed(0, 0.5)));
    }

    #[test]
    fn foreground_uses_elapsed_background_time_and_deduplicates_phases() {
        let mut lifecycle = Lifecycle::default();
        assert_eq!(lifecycle.foreground(0), None);
        lifecycle.background(100);
        lifecycle.background(200);
        assert_eq!(lifecycle.foreground(10_099), Some(Wake::Probe));
        assert_eq!(lifecycle.foreground(10_100), None);
        lifecycle.background(20_000);
        assert_eq!(lifecycle.foreground(30_000), Some(Wake::Reconnect));
        lifecycle.background(40_000);
        assert_eq!(lifecycle.foreground(55_000), Some(Wake::Reconnect));
    }
}
