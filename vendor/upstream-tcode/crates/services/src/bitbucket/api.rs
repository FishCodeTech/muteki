//! Bitbucket Cloud's REST 2.0 API at api.bitbucket.org, reached with the bitbucket.org
//! credential: an API token sent with its Atlassian account's email, or an access token alone.
//! Reads of a public repository need neither; writes do.

use crate::{
    forge::{ForgeError, ForgeErrorKind},
    settings::SettingsStore,
};
use serde_json::Value;
use std::{
    collections::{BTreeMap, HashMap},
    io::Read as _,
    sync::{Arc, Mutex, RwLock},
    time::{Duration, Instant, SystemTime},
};
use tcode_core::{
    pull_request::HostKind,
    settings::{CredentialSource, HostSettings},
};

/// Bitbucket Cloud's one web host, which settings and keys name it by.
pub(super) const HOST: &str = "bitbucket.org";
const API: &str = "https://api.bitbucket.org/2.0";
const BODY_LIMIT: usize = 8 * 1024 * 1024;
const DEADLINE: Duration = Duration::from_secs(60);
/// Bitbucket's rate-limit window.
const WINDOW: Duration = Duration::from_secs(3600);
/// Same-origin redirects followed for one read, as `/diff` and `/diffstat` answer with one.
const REDIRECTS: usize = 3;
/// The token for bitbucket.org, an API token when `ENV_EMAIL` names its account.
pub(super) const ENV_TOKEN: &str = "BITBUCKET_TOKEN";
pub(super) const ENV_EMAIL: &str = "BITBUCKET_EMAIL";
/// The page size asked for, the largest Bitbucket takes for comments and activity.
pub(super) const PAGE_LIMIT: usize = 50;

pub(super) fn error(kind: ForgeErrorKind, description: impl Into<String>) -> ForgeError {
    ForgeError {
        kind,
        description: description.into(),
    }
}

#[derive(Clone)]
pub(super) struct Credential {
    /// The Authorization header's value.
    authorization: String,
    verification: String,
    pub(super) source: CredentialSource,
}

impl Credential {
    /// An API token goes with its account's email; an access token goes alone. A token that
    /// is not one header-safe word is none at all, so it never reaches a request.
    fn new(token: &str, email: Option<&str>, source: CredentialSource) -> Option<Self> {
        use base64::Engine as _;
        let token = token.trim();
        if token.is_empty() || !token.bytes().all(|b| b.is_ascii_graphic()) {
            return None;
        }
        let authorization = match email.map(str::trim).filter(|email| !email.is_empty()) {
            Some(email) => format!(
                "Basic {}",
                base64::engine::general_purpose::STANDARD.encode(format!("{email}:{token}"))
            ),
            None => format!("Bearer {token}"),
        };
        Some(Self {
            verification: crate::settings::credential_fingerprint(
                token,
                email.map(str::trim).filter(|email| !email.is_empty()),
            ),
            authorization,
            source,
        })
    }

    /// Opaque, and different for each credential.
    pub(super) fn fingerprint(&self) -> String {
        use sha2::Digest as _;
        let digest = sha2::Sha256::digest(self.authorization.as_bytes());
        digest[..8].iter().map(|b| format!("{b:02x}")).collect()
    }

    /// Whether Bitbucket may refuse to say whose it is: an access token belongs to a
    /// repository, project or workspace, not to an account.
    pub(super) fn bearer(&self) -> bool {
        self.authorization.starts_with("Bearer ")
    }
}

#[derive(Debug)]
pub(super) struct Response {
    pub(super) status: u16,
    pub(super) body: Vec<u8>,
    pub(super) truncated: bool,
}

impl Response {
    pub(super) fn json<T: serde::de::DeserializeOwned>(&self) -> Result<T, ForgeError> {
        if self.truncated {
            return Err(error(
                ForgeErrorKind::TooLarge,
                "Bitbucket response too large",
            ));
        }
        serde_json::from_slice(&self.body)
            .map_err(|_| error(ForgeErrorKind::Uncertain, "Bitbucket response unreadable"))
    }
}

pub(super) struct Request<'a> {
    pub(super) method: &'a str,
    /// Below `/2.0`, query included, or a `next` page Bitbucket named, which must be on the
    /// API's own origin.
    pub(super) path: String,
    pub(super) body: Option<Value>,
    pub(super) accept: &'a str,
    pub(super) operation: &'static str,
    /// A write goes only with a credential.
    pub(super) write: bool,
    pub(super) limit: usize,
}

impl<'a> Request<'a> {
    pub(super) fn get(path: impl Into<String>, operation: &'static str) -> Self {
        Self {
            method: "GET",
            path: path.into(),
            body: None,
            accept: "application/json",
            operation,
            write: false,
            limit: BODY_LIMIT,
        }
    }
    pub(super) fn write(
        method: &'a str,
        path: impl Into<String>,
        body: Option<Value>,
        operation: &'static str,
    ) -> Self {
        Self {
            method,
            body,
            write: true,
            ..Self::get(path, operation)
        }
    }
}

pub(super) struct Api {
    store: SettingsStore,
    environment: BTreeMap<String, String>,
    hosts: RwLock<BTreeMap<String, HostSettings>>,
    pub(super) agent: ureq::Agent,
    /// Until when, and for which credential's fingerprint: Bitbucket limits each credential, and
    /// anonymous reads by address, apart.
    rate_limits: Mutex<crate::forge::RateLimits>,
    /// The account each credential reads as, by fingerprint, for a few minutes: every read
    /// that names the viewer would otherwise spend one of Bitbucket's hourly requests on it.
    viewers: Mutex<HashMap<String, (Instant, Option<Value>)>>,
}

impl Api {
    pub(super) fn new(
        store: SettingsStore,
        environment: impl IntoIterator<Item = (String, String)>,
    ) -> Arc<Self> {
        Arc::new(Self {
            store,
            environment: environment.into_iter().collect(),
            hosts: RwLock::default(),
            // No pooling: ureq retries a stale pooled socket, and a write must never be retried.
            // Redirects are followed by hand, on the API's origin only.
            agent: ureq::AgentBuilder::new()
                .redirects(0)
                .max_idle_connections(0)
                .build(),
            rate_limits: Mutex::default(),
            viewers: Mutex::default(),
        })
    }

    pub(super) fn configure(&self, hosts: BTreeMap<String, HostSettings>) {
        *self.hosts.write().unwrap() = hosts;
    }

    pub(super) fn configured(&self) -> BTreeMap<String, HostSettings> {
        self.hosts.read().unwrap().clone()
    }

    fn enabled(&self) -> bool {
        self.hosts
            .read()
            .unwrap()
            .get(HOST)
            .is_none_or(|choice| choice.enabled)
    }

    /// The credential saved for bitbucket.org, then `BITBUCKET_TOKEN` (with `BITBUCKET_EMAIL`
    /// when it is an API token). `None` reads anonymously. No other server is ever sent one.
    pub(super) fn credential(&self, authority: &str) -> Result<Option<Credential>, ForgeError> {
        if authority != HOST {
            return Ok(None);
        }
        if !self.enabled() {
            return Err(error(
                ForgeErrorKind::HostDisabled,
                "Bitbucket host turned off",
            ));
        }
        if let Some((token, email)) = self.store.saved_credential(HostKind::Bitbucket, HOST) {
            let source = match &email {
                Some(email) => CredentialSource::SavedBasic {
                    email: email.clone(),
                },
                None => CredentialSource::SavedBearer,
            };
            return Ok(Credential::new(&token, email.as_deref(), source));
        }
        Ok(self.environment.get(ENV_TOKEN).and_then(|token| {
            Credential::new(
                token,
                self.environment.get(ENV_EMAIL).map(String::as_str),
                CredentialSource::Env {
                    name: ENV_TOKEN.into(),
                },
            )
        }))
    }

    pub(super) fn token_saved(&self) -> bool {
        self.store.token(HostKind::Bitbucket, HOST).is_some()
    }

    /// Whether Bitbucket has answered a request sent with `credential`, a saved access token.
    pub(super) fn verified(&self, credential: &Credential) -> bool {
        self.store
            .credential_verified(HostKind::Bitbucket, HOST, &credential.verification)
    }

    pub(super) fn forget(&self) {
        self.viewers.lock().unwrap().clear();
    }

    pub(super) fn environment_names_host(&self) -> bool {
        self.environment.contains_key(ENV_TOKEN)
    }

    /// The account the credential reads as, `None` anonymously or with a credential Bitbucket
    /// will not name an account for: an access token, or a token without the account scope.
    pub(super) fn viewer(&self) -> Result<Option<Value>, ForgeError> {
        let Some(credential) = self.credential(HOST)? else {
            return Ok(None);
        };
        let fingerprint = credential.fingerprint();
        if let Some((at, viewer)) = self.viewers.lock().unwrap().get(&fingerprint)
            && at.elapsed() < Duration::from_secs(300)
        {
            return Ok(viewer.clone());
        }
        let viewer = match self.send(Request::get(
            "/user?fields=uuid,nickname,display_name",
            "Viewer",
        )) {
            Ok(response) => Some(response.json::<Value>()?),
            // A token without the account scope may still read pull requests.
            Err(ForgeError {
                kind: ForgeErrorKind::Refused { .. },
                ..
            }) => None,
            Err(ForgeError {
                kind: ForgeErrorKind::Unauthorized,
                ..
            }) if credential.bearer() => None,
            Err(failure) => return Err(failure),
        };
        let mut viewers = self.viewers.lock().unwrap();
        viewers.retain(|_, (at, _)| at.elapsed() < Duration::from_secs(300));
        viewers.insert(fingerprint, (Instant::now(), viewer.clone()));
        Ok(viewer)
    }

    /// The URL a request path names: below the API, or a `next` page on the API's origin.
    fn url(path: &str) -> Result<url::Url, ForgeError> {
        let invalid = || error(ForgeErrorKind::InvalidInput, "Bitbucket URL off the API");
        let api = url::Url::parse(API).map_err(|_| invalid())?;
        if path.starts_with('/') {
            return url::Url::parse(&format!("{API}{path}")).map_err(|_| invalid());
        }
        let url = url::Url::parse(path).map_err(|_| invalid())?;
        (crate::forge::same_origin(&url, &api) && url.path().starts_with("/2.0/"))
            .then_some(url)
            .ok_or_else(invalid)
    }

    pub(super) fn send(&self, request: Request<'_>) -> Result<Response, ForgeError> {
        let credential = self.credential(HOST)?;
        let fingerprint = credential
            .as_ref()
            .map_or_else(|| "anonymous".to_owned(), Credential::fingerprint);
        let lease = self
            .rate_limits
            .lock()
            .unwrap()
            .admit(HOST, &fingerprint, false)
            .map_err(|retry_at| {
                error(
                    ForgeErrorKind::Paused { retry_at },
                    "Bitbucket request paused",
                )
            })?;
        if request.write && credential.is_none() {
            return Err(error(
                ForgeErrorKind::NoCredential,
                "Bitbucket credential unavailable",
            ));
        }
        let mut url = Self::url(&request.path)?;
        let started = Instant::now();
        let mut redirects = 0;
        let response = loop {
            let mut call = self
                .agent
                .request(request.method, url.as_str())
                .timeout(DEADLINE.saturating_sub(started.elapsed()))
                .set("Accept", request.accept)
                .set("User-Agent", "tcode");
            if let Some(credential) = &credential {
                call = call.set("Authorization", &credential.authorization);
            }
            let answer = match &request.body {
                Some(body) => call
                    .set("Content-Type", "application/json")
                    .send_bytes(&serde_json::to_vec(body).unwrap_or_default()),
                None => call.call(),
            };
            let response = match answer {
                Ok(response) | Err(ureq::Error::Status(_, response)) => response,
                Err(ureq::Error::Transport(_)) => {
                    return Err(if started.elapsed() >= DEADLINE {
                        error(ForgeErrorKind::Deadline, "Bitbucket request deadline")
                    } else {
                        error(
                            ForgeErrorKind::Uncertain,
                            "Bitbucket request failed (network)",
                        )
                    });
                }
            };
            log::debug!(
                "bitbucket path={} operation={} status={}",
                url.path(),
                request.operation,
                response.status()
            );
            // `/diff` and `/diffstat` answer with a redirect to the comparison they name. One
            // off the API's origin is refused, so the credential never leaves it.
            if (300..400).contains(&response.status()) && request.method == "GET" {
                let next = response
                    .header("location")
                    .and_then(|location| url.join(location).ok())
                    .filter(|next| crate::forge::same_origin(next, &url))
                    .ok_or_else(|| {
                        error(ForgeErrorKind::Uncertain, "Bitbucket redirect off the API")
                    })?;
                redirects += 1;
                if redirects > REDIRECTS {
                    return Err(error(
                        ForgeErrorKind::Uncertain,
                        "Bitbucket redirects without end",
                    ));
                }
                url = next;
                continue;
            }
            break response;
        };
        let status = response.status();
        // `X-RateLimit-Reset` is the seconds left in Bitbucket's hourly window; a 429 may carry
        // it without `Retry-After`.
        let retry_after = response
            .header("retry-after")
            .or_else(|| response.header("x-ratelimit-reset"))
            .map(str::to_owned);
        let limit = request.limit.min(BODY_LIMIT);
        let mut body = Vec::new();
        response
            .into_reader()
            .take(limit as u64 + 1)
            .read_to_end(&mut body)
            .map_err(|_| error(ForgeErrorKind::Uncertain, "Bitbucket response cut off"))?;
        let truncated = body.len() > limit;
        body.truncate(limit);
        if status == 429 {
            let named = retry_after
                .as_deref()
                .and_then(retry_at)
                // No answer outlasts Bitbucket's hourly window.
                .map(|at| at.min(SystemTime::now() + WINDOW));
            let retry_at =
                self.rate_limits
                    .lock()
                    .unwrap()
                    .refused(HOST, &fingerprint, lease, named);
            return Err(error(
                ForgeErrorKind::RateLimited { retry_at },
                "Bitbucket request failed (rate limited)",
            ));
        }
        if (200..300).contains(&status) {
            self.rate_limits
                .lock()
                .unwrap()
                .succeeded(HOST, &fingerprint, lease);
            if let Some(credential) = credential
                .as_ref()
                .filter(|credential| credential.source == CredentialSource::SavedBearer)
                && let Err(error) = self.store.record_verification(
                    HostKind::Bitbucket,
                    HOST,
                    &credential.verification,
                    true,
                )
            {
                log::warn!("failed to save credential verification: {error}");
            }
            return Ok(Response {
                status,
                body,
                truncated,
            });
        }
        Err(match status {
            401 => {
                if let Some(credential) = credential.as_ref()
                    && let Err(error) = self.store.record_verification(
                        HostKind::Bitbucket,
                        HOST,
                        &credential.verification,
                        false,
                    )
                {
                    log::warn!("failed to clear credential verification: {error}");
                }
                self.forget();
                error(
                    ForgeErrorKind::Unauthorized,
                    "Bitbucket request failed (unauthorized)",
                )
            }
            404 => error(
                ForgeErrorKind::NotFound,
                "Bitbucket request failed (not found)",
            ),
            500.. => error(
                ForgeErrorKind::Uncertain,
                format!("Bitbucket response failed ({status})"),
            ),
            _ => error(
                ForgeErrorKind::Refused {
                    messages: messages(&body),
                },
                format!("Bitbucket response failed ({status})"),
            ),
        })
    }

    /// A list read page by page while Bitbucket names a `next` page, up to `pages`; with where
    /// the next page starts when more remained.
    pub(super) fn list(
        &self,
        path: &str,
        operation: &'static str,
        pages: usize,
    ) -> Result<(Vec<Value>, Option<String>), ForgeError> {
        let mut rows = Vec::new();
        let mut next = Some(path.to_owned());
        for _ in 0..pages {
            let Some(at) = next.take() else {
                break;
            };
            let page: Value = self.send(Request::get(at, operation))?.json()?;
            rows.extend(page["values"].as_array().into_iter().flatten().cloned());
            next = page["next"].as_str().map(str::to_owned);
        }
        Ok((rows, next))
    }

    /// An attachment on bitbucket.org, read with the credential.
    pub(super) fn media(
        &self,
        url: &url::Url,
        validator: Option<&str>,
    ) -> Result<tcode_protocol::PullRequestMedia, ForgeError> {
        let authorization = self
            .credential(HOST)
            .ok()
            .flatten()
            .map(|credential| credential.authorization);
        crate::forge::media(
            &self.agent,
            url,
            authorization.as_deref(),
            validator,
            "Bitbucket",
        )
    }
}

/// `Retry-After` or `X-RateLimit-Reset` as seconds, or `Retry-After` as an HTTP date.
fn retry_at(value: &str) -> Option<SystemTime> {
    let value = value.trim();
    if let Ok(seconds) = value.parse::<u64>() {
        return Some(SystemTime::now() + Duration::from_secs(seconds));
    }
    let at = chrono::DateTime::parse_from_rfc2822(value).ok()?;
    Some(SystemTime::UNIX_EPOCH + Duration::from_secs(u64::try_from(at.timestamp()).ok()?))
}

/// Bitbucket's words for a refusal: `error.message`, then each field's own.
pub(super) fn messages(body: &[u8]) -> Vec<String> {
    let Ok(value) = serde_json::from_slice::<Value>(body) else {
        return Vec::new();
    };
    let error = &value["error"];
    error["message"]
        .as_str()
        .map(str::to_owned)
        .into_iter()
        .chain(
            error["fields"].as_object().into_iter().flatten().flat_map(
                |(field, said)| match said {
                    Value::Array(items) => items
                        .iter()
                        .filter_map(|item| item.as_str())
                        .map(|item| format!("{field}: {item}"))
                        .collect(),
                    Value::String(item) => vec![format!("{field}: {item}")],
                    _ => Vec::new(),
                },
            ),
        )
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A files cursor is the `next` page Bitbucket named, which a client sends back: only a page
    /// below the API on its own origin is read, so a cursor never carries the credential away.
    #[test]
    fn a_cursor_is_only_a_page_of_the_api() {
        let next = "https://api.bitbucket.org/2.0/repositories/a/b/diffstat/a/b:1%0D2?page=2";
        assert_eq!(Api::url(next).unwrap().as_str(), next);
        assert_eq!(
            Api::url("/repositories/a/b/pullrequests/1")
                .unwrap()
                .as_str(),
            "https://api.bitbucket.org/2.0/repositories/a/b/pullrequests/1"
        );
        for elsewhere in [
            "https://evil.test/2.0/repositories/a/b",
            "http://api.bitbucket.org/2.0/repositories/a/b",
            "https://api.bitbucket.org:8443/2.0/repositories/a/b",
            "https://api.bitbucket.org/internal/a",
            "repositories/a/b",
        ] {
            assert_eq!(
                Api::url(elsewhere).map_err(|error| error.kind),
                Err(ForgeErrorKind::InvalidInput),
                "{elsewhere}"
            );
        }
    }

    /// The saved credential is an API token sent with its account's email, or an access token
    /// sent alone, and saving one method clears the other; without one, `BITBUCKET_TOKEN` (with
    /// `BITBUCKET_EMAIL` when it is an API token) is used. Neither ever goes to another host,
    /// and a token that is not one header-safe word is never sent.
    #[test]
    fn a_credential_is_the_saved_method_then_the_environment() {
        use base64::Engine as _;
        let root =
            std::env::temp_dir().join(format!("tcode-bitbucket-token-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let store = SettingsStore::new(root.clone());
        let env = [
            (ENV_TOKEN.to_owned(), "env-secret".to_owned()),
            (ENV_EMAIL.to_owned(), "ci@acme.test".to_owned()),
        ];
        let api = Api::new(store.clone(), env.clone());
        let read = |api: &Api, authority: &str| {
            api.credential(authority)
                .unwrap()
                .map(|credential| (credential.source, credential.authorization))
        };
        let basic = |email: &str, token: &str| {
            format!(
                "Basic {}",
                base64::engine::general_purpose::STANDARD.encode(format!("{email}:{token}"))
            )
        };
        let from_env = Some((
            CredentialSource::Env {
                name: ENV_TOKEN.into(),
            },
            basic("ci@acme.test", "env-secret"),
        ));
        assert_eq!(read(&api, HOST), from_env);
        assert_eq!(read(&api, "bitbucket.acme.test"), None);
        let bearer_env = Api::new(store.clone(), [env[0].clone()]);
        assert_eq!(
            read(&bearer_env, HOST).map(|(_, authorization)| authorization),
            Some("Bearer env-secret".to_owned())
        );

        store
            .set_credential(
                HostKind::Bitbucket,
                HOST,
                Some("api-secret"),
                Some("me@acme.test"),
            )
            .unwrap();
        assert_eq!(
            read(&api, HOST),
            Some((
                CredentialSource::SavedBasic {
                    email: "me@acme.test".into()
                },
                basic("me@acme.test", "api-secret"),
            ))
        );
        // A new email keeps the API token it is saved with.
        store
            .set_credential(HostKind::Bitbucket, HOST, None, Some("you@acme.test"))
            .unwrap();
        assert_eq!(
            read(&api, HOST).map(|(_, authorization)| authorization),
            Some(basic("you@acme.test", "api-secret"))
        );
        store
            .set_token(HostKind::Bitbucket, HOST, Some("access-secret"))
            .unwrap();
        assert_eq!(
            read(&api, HOST),
            Some((CredentialSource::SavedBearer, "Bearer access-secret".into()))
        );
        // An access token is no API token: an email alone does not turn it into one.
        assert!(
            store
                .set_credential(HostKind::Bitbucket, HOST, None, Some("me@acme.test"))
                .is_err()
        );
        store
            .set_token(HostKind::Bitbucket, HOST, Some("two words"))
            .unwrap();
        assert_eq!(read(&api, HOST), None);
        store.set_token(HostKind::Bitbucket, HOST, None).unwrap();
        assert_eq!(read(&api, HOST), from_env);
        let _ = std::fs::remove_dir_all(root);
    }
}
