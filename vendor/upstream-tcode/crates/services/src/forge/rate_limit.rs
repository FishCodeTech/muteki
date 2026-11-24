use std::{
    collections::HashMap,
    time::{Duration, SystemTime},
};

/// How long a host is left alone after its rate limit refuses a request, per host and credential:
/// until the time the host names, else `min(30 s·2^(n−1), 15 min)` for the n-th refusal in a row.
/// A success after the pause starts the count again.
///
/// A request runs under the lease [`Self::admit`] gave it, so the refusals of requests already in
/// flight when a pause began count once, not once each.
#[derive(Default)]
pub(crate) struct RateLimits {
    pauses: HashMap<(String, String), Pause>,
}

#[derive(Default)]
struct Pause {
    attempt: u32,
    generation: u64,
    retry_at: Option<SystemTime>,
}

impl RateLimits {
    /// The lease for a request, or the time the pause in force ends. An interactive request goes
    /// through a pause.
    pub(crate) fn admit(
        &self,
        host: &str,
        credential: &str,
        interactive: bool,
    ) -> Result<u64, SystemTime> {
        let pause = self.pauses.get(&(host.to_owned(), credential.to_owned()));
        if !interactive
            && let Some(retry_at) = pause
                .and_then(|pause| pause.retry_at)
                .filter(|retry_at| *retry_at > SystemTime::now())
        {
            return Err(retry_at);
        }
        Ok(pause.map_or(0, |pause| pause.generation))
    }

    /// The host refused a request admitted under `lease`; `explicit` is the time it named.
    /// Returns when the pause ends.
    pub(crate) fn refused(
        &mut self,
        host: &str,
        credential: &str,
        lease: u64,
        explicit: Option<SystemTime>,
    ) -> SystemTime {
        let now = SystemTime::now();
        let explicit = explicit.filter(|time| *time > now);
        let pause = self
            .pauses
            .entry((host.to_owned(), credential.to_owned()))
            .or_default();
        if pause.generation > lease {
            if let Some(explicit) = explicit
                && pause.retry_at.is_none_or(|old| explicit > old)
            {
                pause.retry_at = Some(explicit);
            }
            return pause.retry_at.unwrap_or(now);
        }
        pause.attempt = pause.attempt.saturating_add(1);
        let cooldown = 30_u64
            .saturating_mul(2_u64.saturating_pow(pause.attempt - 1))
            .min(900);
        let proposed = explicit.unwrap_or(now + Duration::from_secs(cooldown));
        let retry_at = pause
            .retry_at
            .filter(|time| *time > now)
            .map_or(proposed, |old| old.max(proposed));
        pause.retry_at = Some(retry_at);
        pause.generation = pause.generation.max(lease) + 1;
        retry_at
    }

    /// A request admitted under `lease` succeeded.
    pub(crate) fn succeeded(&mut self, host: &str, credential: &str, lease: u64) {
        if let Some(pause) = self
            .pauses
            .get_mut(&(host.to_owned(), credential.to_owned()))
            && pause.generation == lease
            && pause.retry_at.is_none_or(|time| time <= SystemTime::now())
        {
            pause.attempt = 0;
            pause.retry_at = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Seconds from now until `at`, rounded to the second.
    fn seconds_until(at: SystemTime) -> u64 {
        (at.duration_since(SystemTime::now()).unwrap() + Duration::from_millis(500)).as_secs()
    }

    #[test]
    fn refusals_in_a_row_double_the_pause_up_to_fifteen_minutes_per_credential() {
        let mut limits = RateLimits::default();
        for expected in [30, 60, 120, 240, 480, 900, 900] {
            let lease = limits.admit("gitlab.com", "a", true).unwrap();
            let retry_at = limits.refused("gitlab.com", "a", lease, None);
            assert_eq!(seconds_until(retry_at), expected);
            assert_eq!(limits.admit("gitlab.com", "a", false), Err(retry_at));
        }
        assert!(limits.admit("gitlab.com", "b", false).is_ok());
        assert!(limits.admit("codeberg.org", "a", false).is_ok());
    }

    #[test]
    fn the_hosts_time_wins_and_a_refusal_already_in_flight_counts_once() {
        let mut limits = RateLimits::default();
        let lease = limits.admit("codeberg.org", "a", false).unwrap();
        let named = SystemTime::now() + Duration::from_secs(300);
        assert_eq!(
            limits.refused("codeberg.org", "a", lease, Some(named)),
            named
        );
        let in_flight = limits.refused("codeberg.org", "a", lease, None);
        assert_eq!(in_flight, named);
        let lease = limits.admit("codeberg.org", "a", true).unwrap();
        assert_eq!(
            seconds_until(limits.refused("codeberg.org", "a", lease, None)),
            300,
            "the second refusal in a row proposes 60 s, short of the pause in force"
        );
    }

    #[test]
    fn a_success_after_the_pause_starts_the_count_again() {
        let mut limits = RateLimits::default();
        let lease = limits.admit("dev.azure.com", "a", false).unwrap();
        let named = SystemTime::now() + Duration::from_millis(50);
        limits.refused("dev.azure.com", "a", lease, Some(named));
        std::thread::sleep(Duration::from_millis(60));
        let lease = limits.admit("dev.azure.com", "a", false).unwrap();
        limits.succeeded("dev.azure.com", "a", lease);
        let lease = limits.admit("dev.azure.com", "a", false).unwrap();
        assert_eq!(
            seconds_until(limits.refused("dev.azure.com", "a", lease, None)),
            30
        );
    }
}
