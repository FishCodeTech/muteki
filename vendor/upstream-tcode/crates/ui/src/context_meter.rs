//! Circular context-window meter math and compact token formatting.

use agent::TokenUsage;

/// The used tokens a meter reflects: the provider's reported total-in-use, else
/// the input-token count.
pub fn used_tokens(usage: &TokenUsage) -> Option<u64> {
    if matches!(
        usage.freshness,
        agent::ContextFreshness::Unknown | agent::ContextFreshness::AwaitingObservation
    ) {
        return None;
    }
    usage.used_tokens.or(usage.input_tokens)
}

/// Used-percentage of the context window (0..=100), or `None` when either the
/// used count or the window size is unknown. Capped at 100.
pub fn used_percentage(usage: &TokenUsage) -> Option<f32> {
    let used = used_tokens(usage)? as f32;
    let max = usage.context_window? as f32;
    if max <= 0.0 {
        return None;
    }
    Some((used / max * 100.0).min(100.0))
}

/// Whether the meter is in the red "overloaded" band (>90%).
pub fn is_overloaded(percentage: f32) -> bool {
    percentage > 90.0
}

/// One decimal below 10% (trim `.0`), otherwise a whole number.
/// `None` or a non-finite input returns `None`.
pub fn format_percentage(percentage: Option<f32>) -> Option<String> {
    let value = percentage?;
    if !value.is_finite() {
        return None;
    }
    if value < 10.0 {
        let s = format!("{value:.1}");
        let s = s.strip_suffix(".0").map(str::to_string).unwrap_or(s);
        Some(format!("{s}%"))
    } else {
        Some(format!("{}%", value.round() as i64))
    }
}

/// Compact token count: `<1000` verbatim, `<10_000` as `x.yk`,
/// `<1_000_000` as `Nk`, otherwise `x.ym`; trailing `.0` is omitted.
pub fn format_tokens(value: Option<u64>) -> String {
    let Some(v) = value else {
        return crate::tr!("composer.context_unknown").into_owned();
    };
    let v = v as f64;
    if v < 1_000.0 {
        format!("{}", v.round() as i64)
    } else if v < 10_000.0 {
        let s = format!("{:.1}", v / 1_000.0);
        s.strip_suffix(".0").unwrap_or(&s).to_string() + "k"
    } else if v < 1_000_000.0 {
        format!("{}k", (v / 1_000.0).round() as i64)
    } else {
        let s = format!("{:.1}", v / 1_000_000.0);
        s.strip_suffix(".0").unwrap_or(&s).to_string() + "m"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent::ContextFreshness;

    #[test]
    fn meter_uses_only_observed_usage_and_caps_a_known_nonzero_window() {
        for (freshness, used, input, window, expected_used, expected_percent) in [
            (
                ContextFreshness::Current,
                Some(100_000),
                Some(3),
                Some(200_000),
                Some(100_000),
                Some(50.0),
            ),
            (
                ContextFreshness::Current,
                None,
                Some(50),
                Some(200),
                Some(50),
                Some(25.0),
            ),
            (
                ContextFreshness::Current,
                Some(300),
                None,
                Some(200),
                Some(300),
                Some(100.0),
            ),
            (
                ContextFreshness::Current,
                Some(0),
                Some(50),
                Some(200),
                Some(0),
                Some(0.0),
            ),
            (
                ContextFreshness::Current,
                Some(50),
                None,
                None,
                Some(50),
                None,
            ),
            (
                ContextFreshness::Current,
                Some(50),
                None,
                Some(0),
                Some(50),
                None,
            ),
            (ContextFreshness::Current, None, None, Some(200), None, None),
            (
                ContextFreshness::Unknown,
                Some(50),
                Some(10),
                Some(200),
                None,
                None,
            ),
            (
                ContextFreshness::AwaitingObservation,
                Some(50),
                Some(10),
                Some(200),
                None,
                None,
            ),
        ] {
            let usage = TokenUsage {
                freshness,
                used_tokens: used,
                input_tokens: input,
                context_window: window,
                ..Default::default()
            };
            assert_eq!(used_tokens(&usage), expected_used, "{usage:?}");
            assert_eq!(used_percentage(&usage), expected_percent, "{usage:?}");
        }
        assert!(!is_overloaded(90.0));
        assert!(is_overloaded(90.1));
    }

    #[test]
    fn compact_labels_keep_small_values_precise_and_unknown_values_explicit() {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        for (value, expected) in [
            (Some(5.0), Some("5%")),
            (Some(5.5), Some("5.5%")),
            (Some(42.4), Some("42%")),
            (Some(90.6), Some("91%")),
            (None, None),
            (Some(f32::NAN), None),
            (Some(f32::INFINITY), None),
        ] {
            assert_eq!(format_percentage(value).as_deref(), expected);
        }
        for (value, expected) in [
            (Some(0), "0"),
            (Some(999), "999"),
            (Some(1_500), "1.5k"),
            (Some(4_000), "4k"),
            (Some(42_000), "42k"),
            (Some(1_000_000), "1m"),
            (Some(1_250_000), "1.2m"),
            (None, "Unknown"),
        ] {
            assert_eq!(format_tokens(value), expected);
        }
    }
}
