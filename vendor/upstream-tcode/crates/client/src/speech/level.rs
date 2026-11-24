// RMS mapping and smoothing adapted from gpui-component 0.7.1.
// Copyright 2024 - 2026 Longbridge <https://longbridge.com>
// Licensed under Apache-2.0; see LICENSE-APACHE.

/// The mic glow follows 80 ms RMS windows, with a noise gate and fast attack /
/// slow release. Capture callbacks carry incomplete windows across buffers.
pub(super) struct LevelMeter {
    window: usize,
    sum: f64,
    count: usize,
    smoothed: f32,
}

impl LevelMeter {
    pub(super) fn new(sample_rate: f64) -> Self {
        Self {
            window: (sample_rate * 0.08).max(1.) as usize,
            sum: 0.,
            count: 0,
            smoothed: 0.,
        }
    }

    pub(super) fn push(&mut self, sample: f32) -> Option<f32> {
        self.sum += (sample as f64).powi(2);
        self.count += 1;
        if self.count < self.window {
            return None;
        }
        let rms = (self.sum / self.window as f64).sqrt();
        let raw = if rms > 0. {
            ((20. * rms.log10() + 50.) / 50.).clamp(0., 1.) as f32
        } else {
            0.
        };
        let raw = if raw < 0.06 { 0. } else { raw };
        let rate = if raw > self.smoothed { 0.85 } else { 0.3 };
        self.smoothed += (raw - self.smoothed) * rate;
        self.sum = 0.;
        self.count = 0;
        Some(self.smoothed)
    }
}
