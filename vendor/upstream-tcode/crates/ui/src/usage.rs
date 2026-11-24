//! Presentation helpers for provider usage windows (Settings → Usage and the
//! composer's context-window popover).
//!
//! [`tcode_core::usage`] is presentation-free: it reports a window kind, a
//! percentage, and a reset timestamp. Everything a human reads about those —
//! the window's label, the "resets in …" copy, the bar tint — is derived here
//! so both surfaces stay in sync.

use gpui::{App, Hsla, rgb};
use tcode_core::usage::{UsageWindow, UsageWindowKind};

use crate::theme::ActiveTheme as _;

/// The context meter's fill colors, shared with the usage bars so a usage row
/// and the context ring read as the same instrument.
pub(crate) const METER_BLUE: u32 = 0x3B82F6;
pub(crate) const METER_RED: u32 = 0xEF4444;
/// Amber fallback for themes without a `warning` role.
const METER_AMBER: u32 = 0xF59E0B;

/// The window's display label, suffixed with its scope when the window is a
/// model-scoped sub-limit (e.g. "Weekly · Fable").
pub(crate) fn window_label(window: &UsageWindow) -> String {
    let base = match window.kind {
        UsageWindowKind::FiveHour => crate::tr!("usage.window.five_hour").into_owned(),
        UsageWindowKind::Weekly => crate::tr!("usage.window.weekly").into_owned(),
        UsageWindowKind::Other { minutes } => {
            crate::tr!("usage.window.minutes", count = minutes).into_owned()
        }
    };
    match &window.scope {
        Some(scope) => crate::tr!("usage.window.scoped", window = base, scope = scope).into_owned(),
        None => base,
    }
}

/// "Resets in 2h 15m" / "Resets now", or `None` when the provider did not
/// report a reset time.
pub(crate) fn resets_label(resets_at: Option<u64>, now: u64) -> Option<String> {
    let resets_at = resets_at?;
    if resets_at <= now {
        return Some(crate::tr!("usage.resets_now").into_owned());
    }
    let remaining = resets_at - now;
    let (days, hours, minutes) = (
        remaining / 86_400,
        (remaining % 86_400) / 3_600,
        (remaining % 3_600) / 60,
    );
    let duration = if days > 0 {
        format!("{days}d {hours}h")
    } else if hours > 0 {
        format!("{hours}h {minutes}m")
    } else {
        format!("{minutes}m")
    };
    Some(crate::tr!("usage.resets_in", duration = duration).into_owned())
}

/// The used percentage as a whole number (e.g. "83%").
pub(crate) fn percent_label(pct: f32) -> String {
    format!("{}%", pct.clamp(0.0, 100.0).round() as u32)
}

/// Bar tint: blue until 75%, amber to 90%, red beyond.
pub(crate) fn bar_color(pct: f32, cx: &App) -> Hsla {
    if pct >= 90.0 {
        rgb(METER_RED).into()
    } else if pct >= 75.0 {
        let warning = cx.theme().warning;
        // A theme that leaves `warning` fully transparent has no amber role.
        if warning.a > 0.0 {
            warning
        } else {
            rgb(METER_AMBER).into()
        }
    } else {
        rgb(METER_BLUE).into()
    }
}

/// A provider-reported plan id as a display name ("pro" → "Pro").
pub(crate) fn plan_label(plan: &str) -> String {
    let mut chars = plan.trim().chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usage_labels_preserve_scope_and_distinguish_missing_future_and_elapsed_resets() {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        for (kind, scope, expected) in [
            (UsageWindowKind::FiveHour, None, "5h"),
            (UsageWindowKind::Weekly, None, "Weekly"),
            (UsageWindowKind::Other { minutes: 60 }, None, "60 min"),
            (UsageWindowKind::Weekly, Some("Fable"), "Weekly · Fable"),
        ] {
            assert_eq!(
                window_label(&UsageWindow {
                    kind,
                    scope: scope.map(str::to_owned),
                    used_percent: 0.0,
                    resets_at: None,
                }),
                expected
            );
        }
        for (reset, expected) in [
            (None, None),
            (Some(900), Some("Resets now")),
            (Some(1_000), Some("Resets now")),
            (Some(1_040), Some("Resets in 0m")),
            (Some(9_100), Some("Resets in 2h 15m")),
            (Some(98_200), Some("Resets in 1d 3h")),
        ] {
            assert_eq!(resets_label(reset, 1_000).as_deref(), expected, "{reset:?}");
        }
        for (percent, expected) in [(-1.0, "0%"), (12.4, "12%"), (12.5, "13%"), (130.0, "100%")] {
            assert_eq!(percent_label(percent), expected);
        }
    }
}
