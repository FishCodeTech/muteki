use super::api::{GitHubError, Headers};
use std::{
    collections::HashMap,
    time::{Duration, SystemTime},
};

#[derive(Clone)]
struct Snapshot {
    limit: u64,
    remaining: u64,
    reset: SystemTime,
}
#[derive(Default)]
pub(super) struct Ledger {
    quotas: HashMap<(String, String, String), Snapshot>,
    pauses: crate::forge::RateLimits,
}

impl Ledger {
    pub fn admit(
        &self,
        host: &str,
        scope: &str,
        resource: &str,
        interactive: bool,
    ) -> Result<u64, GitHubError> {
        let now = SystemTime::now();
        let lease = self
            .pauses
            .admit(host, scope, interactive)
            .map_err(|retry_at| GitHubError::Paused { retry_at })?;
        if let Some(snapshot) =
            self.quotas
                .get(&(host.to_owned(), resource.to_owned(), scope.to_owned()))
            && snapshot.reset > now
            && (snapshot.remaining == 0
                || (!interactive && (snapshot.remaining as f64) < (snapshot.limit as f64) * 0.1))
        {
            return Err(GitHubError::Paused {
                retry_at: snapshot.reset,
            });
        }
        Ok(lease)
    }

    pub fn observe(&mut self, host: &str, scope: &str, headers: &Headers) {
        let Some(resource) = headers
            .get("x-ratelimit-resource")
            .map(|value| value.trim())
            .filter(|value| !value.is_empty())
        else {
            return;
        };
        let (Some(limit), Some(remaining), Some(reset)) = (
            number(headers, "x-ratelimit-limit"),
            number(headers, "x-ratelimit-remaining"),
            reset_time(headers),
        ) else {
            return;
        };
        if limit == 0 {
            return;
        }
        let key = (host.to_owned(), resource.to_owned(), scope.to_owned());
        if self
            .quotas
            .get(&key)
            .is_some_and(|snapshot| snapshot.reset > reset)
        {
            return;
        }
        self.quotas.insert(
            key,
            Snapshot {
                limit,
                remaining,
                reset,
            },
        );
    }

    pub fn refused(
        &mut self,
        host: &str,
        scope: &str,
        lease: u64,
        headers: &Headers,
    ) -> SystemTime {
        let now = SystemTime::now();
        let explicit = headers
            .get("retry-after")
            .and_then(|value| {
                if let Ok(seconds) = value.trim().parse::<u64>() {
                    now.checked_add(Duration::from_secs(seconds))
                } else {
                    chrono::DateTime::parse_from_rfc2822(value)
                        .ok()
                        .map(SystemTime::from)
                        .filter(|time| *time > now)
                }
            })
            .or_else(|| reset_time(headers).filter(|time| *time > now));
        self.pauses.refused(host, scope, lease, explicit)
    }

    pub fn succeeded(&mut self, host: &str, scope: &str, lease: u64) {
        self.pauses.succeeded(host, scope, lease);
    }
}

pub(super) fn number(headers: &Headers, name: &str) -> Option<u64> {
    headers.get(name)?.trim().parse().ok()
}
fn reset_time(headers: &Headers) -> Option<SystemTime> {
    SystemTime::UNIX_EPOCH.checked_add(Duration::from_secs(number(headers, "x-ratelimit-reset")?))
}
