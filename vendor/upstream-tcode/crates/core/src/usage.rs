//! Provider account usage / rate-limit windows (Settings → Usage and the
//! composer's context-window popover).
//!
//! Presentation-free: labels, colors, and "resets in …" copy are derived by
//! the UI from [`UsageWindowKind`], `used_percent`, and `resets_at`.

use serde::{Deserialize, Serialize};

/// Which rolling window a limit applies to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UsageWindowKind {
    /// The rolling 5-hour window (Claude "session", Codex 300-minute window).
    FiveHour,
    /// The rolling 7-day window (Claude "weekly_*", Codex 10080-minute window).
    Weekly,
    /// Anything else, carrying the provider-reported window length in minutes.
    Other { minutes: u32 },
}

impl UsageWindowKind {
    /// Map a provider-reported window length in minutes onto a kind.
    pub fn from_minutes(minutes: u32) -> Self {
        match minutes {
            300 => Self::FiveHour,
            10_080 => Self::Weekly,
            other => Self::Other { minutes: other },
        }
    }
}

/// One rate-limit window as reported by the provider.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UsageWindow {
    pub kind: UsageWindowKind,
    /// A model-scoped sub-limit (e.g. Claude's separate "Fable" weekly
    /// window): the provider's display name for the scope. `None` for the
    /// account-wide window.
    pub scope: Option<String>,
    /// 0..=100.
    pub used_percent: f32,
    /// Unix seconds when the window resets, when reported.
    pub resets_at: Option<u64>,
}

/// The latest usage fetch for one provider profile.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct ProviderUsage {
    /// Unix seconds of the fetch.
    pub fetched_at: u64,
    /// Provider-reported plan / subscription name (e.g. `pro`, `max`).
    pub plan: Option<String>,
    /// Windows in display order: account-wide 5h, account-wide weekly, then
    /// scoped windows. With `error` set they are the last successful fetch's
    /// windows, dated by `fetched_at`, or empty when there never was one.
    pub windows: Vec<UsageWindow>,
    /// Why the latest fetch produced no windows (provider error, not signed
    /// in, …). Presentation-free but human-readable; the UI shows it as-is.
    pub error: Option<String>,
}
