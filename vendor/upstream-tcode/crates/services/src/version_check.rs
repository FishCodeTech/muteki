//! Update assessments and their host-facing adapters.

use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

use crate::github::{GitHubApi, GitHubError, RequestOptions, RestRequest, api::Authentication};
use serde::Deserialize;

pub mod provider_updates;

/// Tcode's own releases, read without a credential from GitHub, where Tcode is published.
pub struct ReleaseFeed {
    api: Arc<GitHubApi>,
    cached: Mutex<Option<(String, Vec<u8>)>>,
}
impl ReleaseFeed {
    pub fn new(api: Arc<GitHubApi>) -> Self {
        Self {
            api,
            cached: Mutex::new(None),
        }
    }

    pub fn latest_json(&self) -> Result<Vec<u8>, FetchError> {
        let mut cached = self.cached.lock().unwrap();
        let mut request = RestRequest::get("/repos/Tryanks/tcode/releases/latest");
        request.if_none_match = cached.as_ref().map(|(etag, _)| etag.as_str());
        let response = self
            .api
            .rest(
                "github.com",
                request,
                &RequestOptions {
                    authentication: Authentication::Anonymous,
                    operation: "LatestRelease",
                    timeout: Duration::from_secs(10),
                    body_limit: 1024 * 1024,
                    ..Default::default()
                },
            )
            .map_err(|error| match error {
                GitHubError::RateLimited { status, .. } => FetchError::RateLimited { status },
                GitHubError::Paused { .. } => FetchError::RateLimited { status: 429 },
                GitHubError::Response { status, .. } => FetchError::Http { status },
                GitHubError::Unauthorized => FetchError::Http { status: 401 },
                GitHubError::NotFound => FetchError::Http { status: 404 },
                GitHubError::BodyTooLarge => FetchError::ResponseTooLarge,
                _ => FetchError::Network,
            })?;
        if response.status == 304 {
            return cached
                .as_ref()
                .map(|(_, body)| body.clone())
                .ok_or(FetchError::Network);
        }
        if response.truncated {
            return Err(FetchError::ResponseTooLarge);
        }
        *cached = response
            .headers
            .get("etag")
            .map(|etag| (etag.clone(), response.body.clone()));
        Ok(response.body)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FetchError {
    Network,
    RateLimited { status: u16 },
    Http { status: u16 },
    Read,
    ResponseTooLarge,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Assessment {
    pub latest: Option<String>,
    pub release_url: Option<String>,
    /// True only when a parsed release is newer and permitted by prerelease policy.
    pub update_available: bool,
}

#[derive(Debug, Deserialize)]
struct Release {
    tag_name: String,
    html_url: String,
    #[serde(default)]
    prerelease: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Version {
    core: (u32, u32, u32),
    prerelease: bool,
}

/// Assess the running version using a fetched GitHub release payload.
///
/// App release tags deliberately require exactly three numeric parts because
/// they identify a precise published build; prerelease state also participates
/// in the app's stable/prerelease update policy. Provider CLI output is looser
/// and is parsed independently in `provider_updates`.
pub fn check(current: &str, fetched_release_json: Result<&[u8], FetchError>) -> Assessment {
    let Some(release) = fetched_release_json
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Release>(bytes).ok())
    else {
        return Assessment::default();
    };
    let mut assessment = Assessment {
        latest: Some(release.tag_name.trim_start_matches('v').to_string()),
        release_url: Some(release.html_url),
        update_available: false,
    };
    let Some(current_version) = parse_app_version(current) else {
        return assessment;
    };
    let Some(latest_version) = parse_app_version(&release.tag_name) else {
        return assessment;
    };

    // Preserve the release-metadata policy as well as the tag-based policy:
    // stable builds do not opt into releases GitHub marks as prereleases.
    assessment.update_available = (!release.prerelease || current.contains('-'))
        && if latest_version.prerelease && !current_version.prerelease {
            false
        } else {
            match latest_version.core.cmp(&current_version.core) {
                std::cmp::Ordering::Greater => true,
                std::cmp::Ordering::Less => false,
                std::cmp::Ordering::Equal => {
                    current_version.prerelease && !latest_version.prerelease
                }
            }
        };

    assessment
}

fn parse_app_version(raw: &str) -> Option<Version> {
    let raw = raw.trim().strip_prefix('v').unwrap_or(raw.trim());
    let without_build = raw.split_once('+').map_or(raw, |(core, _)| core);
    let (core, prerelease) = match without_build.split_once('-') {
        Some((core, suffix)) if !suffix.is_empty() => (core, true),
        Some(_) => return None,
        None => (without_build, false),
    };
    let mut parts = core.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts.next()?.parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    Some(Version {
        core: (major, minor, patch),
        prerelease,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const RELEASE_URL: &str = "https://github.com/Tryanks/tcode/releases/tag/v0.4.1";

    fn release(tag: &str, prerelease: bool) -> Vec<u8> {
        format!(
            r#"{{"tag_name":"{tag}","html_url":"{RELEASE_URL}","prerelease":{prerelease},"assets":[{{"name":"SHA256SUMS.txt"}}]}}"#
        )
        .into_bytes()
    }

    #[test]
    fn assesses_release_comparisons_and_prerelease_policy() {
        for (current, latest, prerelease, available) in [
            ("0.4.0", "v0.4.0", false, false),
            ("0.4.0", "v0.4.1", false, true),
            ("0.4.1", "v0.4.0", false, false),
            ("0.4.0", "v0.5.0-beta.1", true, false),
            ("0.5.0-beta.1", "v0.5.0", false, true),
            ("0.5.0-beta.1", "v0.6.0-beta.1", true, true),
            ("0.4", "v0.4.1", false, false),
            ("0.4.0", "latest", false, false),
        ] {
            let assessment = check(current, Ok(&release(latest, prerelease)));
            assert_eq!(
                assessment.update_available, available,
                "{current} -> {latest}"
            );
            assert_eq!(
                assessment.latest.as_deref(),
                Some(latest.trim_start_matches('v'))
            );
            assert_eq!(assessment.release_url.as_deref(), Some(RELEASE_URL));
        }
        assert_eq!(
            check("0.4.0", Err(FetchError::Network)),
            Assessment::default()
        );
        assert_eq!(check("0.4.0", Ok(b"not json")), Assessment::default());
    }
}
