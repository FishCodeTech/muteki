#[cfg(not(target_family = "wasm"))]
use std::time::{SystemTime, UNIX_EPOCH};
#[cfg(target_family = "wasm")]
use web_time::{SystemTime, UNIX_EPOCH};

pub(crate) fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

/// Compact countdown: hours retain an hour column, shorter waits use minutes
/// and seconds.
pub(crate) fn format_countdown(secs: u64) -> String {
    let hours = secs / 3_600;
    let minutes = (secs % 3_600) / 60;
    let seconds = secs % 60;
    if hours > 0 {
        format!("{hours}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes}:{seconds:02}")
    }
}

/// Compact relative-time label (e.g. "5m ago") from an elapsed-seconds count.
pub fn humanize_ago(secs: u64) -> String {
    if secs < 60 {
        crate::tr!("time.just_now").into_owned()
    } else if secs < 3600 {
        crate::tr!("time.minutes_ago", count = secs / 60).into_owned()
    } else if secs < 86_400 {
        crate::tr!("time.hours_ago", count = secs / 3600).into_owned()
    } else {
        crate::tr!("time.days_ago", count = secs / 86_400).into_owned()
    }
}

/// Compact label for a time still ahead (e.g. "in 5m") from the seconds until it, rounded up so
/// it never names a time already past.
pub fn humanize_in(secs: u64) -> String {
    if secs < 3600 {
        crate::tr!("time.in_minutes", count = secs.div_ceil(60).max(1)).into_owned()
    } else if secs < 86_400 {
        crate::tr!("time.in_hours", count = secs.div_ceil(3600)).into_owned()
    } else {
        crate::tr!("time.in_days", count = secs.div_ceil(86_400)).into_owned()
    }
}

pub(crate) fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::humanize_in;

    /// A rate limit's resume time reads ahead and rounds up, so it never names a time already
    /// gone; the past-tense label once rendered "resumes 5m ago".
    #[test]
    fn a_time_ahead_reads_in_the_future() {
        assert_eq!(humanize_in(1), "in 1m");
        assert_eq!(humanize_in(301), "in 6m");
        assert_eq!(humanize_in(3601), "in 2h");
        assert_eq!(humanize_in(86_400), "in 1d");
    }
}
