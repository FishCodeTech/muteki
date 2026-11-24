//! Azure DevOps Services' REST API, each organization reached at dev.azure.com with its own
//! credential: a personal access token, or the Entra token of az's signed-in account. Reads of
//! a public project need neither for the pull request and its threads; writes do.

use crate::{
    forge::{ForgeError, ForgeErrorKind},
    settings::SettingsStore,
};
use serde_json::Value;
use std::{
    collections::{BTreeMap, HashMap},
    io::Read as _,
    path::PathBuf,
    sync::{Arc, Mutex, RwLock},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tcode_core::{
    pull_request::HostKind,
    settings::{CredentialSource, HostSettings},
};

/// The API version every request names unless it names its own.
pub(super) const VERSION: &str = "7.1";
/// For resources Azure still versions as previews (statuses, labels, policy evaluations); a
/// released resource takes it too.
pub(super) const PREVIEW: &str = "7.1-preview.1";
const BODY_LIMIT: usize = 8 * 1024 * 1024;
const DEADLINE: Duration = Duration::from_secs(60);
/// The personal access token az's devops extension reads, which Tcode sends only to the
/// organization `ENV_ORGANIZATION` names; az itself sends it to any.
pub(super) const ENV_TOKEN: &str = "AZURE_DEVOPS_EXT_PAT";
pub(super) const ENV_ORGANIZATION: &str = "AZURE_DEVOPS_ORG";
/// Azure DevOps' resource id, which az mints an Entra token for.
const RESOURCE: &str = "499b84ac-1321-427f-aa17-267ca6975798";
/// The page size asked for.
pub(super) const PAGE_LIMIT: usize = 100;

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
    /// A personal access token goes as Basic with an empty user; az's token as a bearer. A
    /// token that is not one header-safe word is none at all, so it never reaches a request.
    fn new(token: &str, bearer: bool, source: CredentialSource) -> Option<Self> {
        use base64::Engine as _;
        let token = token.trim();
        if token.is_empty() || !token.bytes().all(|b| b.is_ascii_graphic()) {
            return None;
        }
        let authorization = if bearer {
            format!("Bearer {token}")
        } else {
            format!(
                "Basic {}",
                base64::engine::general_purpose::STANDARD.encode(format!(":{token}"))
            )
        };
        Some(Self {
            verification: crate::settings::credential_fingerprint(token, None),
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
}

#[derive(Debug)]
pub(super) struct Response {
    pub(super) body: Vec<u8>,
    pub(super) truncated: bool,
}

impl Response {
    pub(super) fn json<T: serde::de::DeserializeOwned>(&self) -> Result<T, ForgeError> {
        if self.truncated {
            return Err(error(
                ForgeErrorKind::TooLarge,
                "Azure DevOps response too large",
            ));
        }
        serde_json::from_slice(&self.body).map_err(|_| {
            error(
                ForgeErrorKind::Uncertain,
                "Azure DevOps response unreadable",
            )
        })
    }
}

pub(super) struct Request<'a> {
    pub(super) method: &'a str,
    /// Below the organization, query included, without the API version.
    pub(super) path: String,
    pub(super) version: &'static str,
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
            version: VERSION,
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

struct Cached<V> {
    value: V,
    expires: Instant,
}

pub(super) struct Api {
    store: SettingsStore,
    environment: BTreeMap<String, String>,
    hosts: RwLock<BTreeMap<String, HostSettings>>,
    pub(super) agent: ureq::Agent,
    /// az's token, which is its signed-in account's for every organization.
    az: Mutex<Option<Cached<Option<String>>>>,
    rate_limits: Mutex<crate::forge::RateLimits>,
    /// The account each credential reads as, by fingerprint, for a few minutes.
    viewers: Mutex<HashMap<String, Cached<Value>>>,
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
            // No redirects: Azure answers a missing sign-in with one, and the credential never
            // follows it off dev.azure.com.
            agent: ureq::AgentBuilder::new()
                .redirects(0)
                .max_idle_connections(0)
                .build(),
            az: Mutex::default(),
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

    fn enabled(&self, authority: &str) -> bool {
        self.hosts
            .read()
            .unwrap()
            .get(authority)
            .is_none_or(|choice| choice.enabled)
    }

    /// The token saved for the organization, then `AZURE_DEVOPS_EXT_PAT` when the organization
    /// is the one `AZURE_DEVOPS_ORG` names, then az's signed-in account. `None` reads
    /// anonymously. One organization's token never goes to another.
    pub(super) fn credential(&self, authority: &str) -> Result<Option<Credential>, ForgeError> {
        // Every request asks for its credential first, so no other authority is ever reached.
        if HostKind::Azure.authority(authority).as_deref() != Ok(authority) {
            return Err(error(
                ForgeErrorKind::InvalidInput,
                "not an Azure DevOps organization",
            ));
        }
        if !self.enabled(authority) {
            return Err(error(
                ForgeErrorKind::HostDisabled,
                "Azure DevOps host turned off",
            ));
        }
        if let Some(token) = self.store.token(HostKind::Azure, authority) {
            return Ok(Credential::new(&token, false, CredentialSource::Saved));
        }
        if self.environment_host().as_deref() == Some(authority) {
            return Ok(self.environment.get(ENV_TOKEN).and_then(|token| {
                Credential::new(
                    token,
                    false,
                    CredentialSource::Env {
                        name: ENV_TOKEN.into(),
                    },
                )
            }));
        }
        Ok(self.az_token().and_then(|token| {
            Credential::new(&token, true, CredentialSource::Cli { tool: "az".into() })
        }))
    }

    pub(super) fn forget(&self, _: &str) {
        *self.az.lock().unwrap() = None;
    }

    pub(super) fn token_saved(&self, authority: &str) -> bool {
        self.store.token(HostKind::Azure, authority).is_some()
    }

    /// Whether the organization has answered a request sent with `credential`, a saved token.
    pub(super) fn verified(&self, authority: &str, credential: &Credential) -> bool {
        self.store
            .credential_verified(HostKind::Azure, authority, &credential.verification)
    }

    /// The organization `AZURE_DEVOPS_EXT_PAT` is for, when both variables are set: by name, as
    /// `dev.azure.com/{organization}`, or as its URL.
    pub(super) fn environment_host(&self) -> Option<String> {
        self.environment.get(ENV_TOKEN)?;
        HostKind::Azure
            .authority(self.environment.get(ENV_ORGANIZATION)?)
            .ok()
    }

    pub(super) fn az_program(&self) -> Option<PathBuf> {
        let path = self.environment.get("PATH")?;
        std::env::split_paths(path)
            .flat_map(|dir| {
                if cfg!(windows) {
                    vec![dir.join("az.cmd"), dir.join("az.exe")]
                } else {
                    vec![dir.join("az")]
                }
            })
            .find(|program| program.is_file())
    }

    /// The Entra token az mints for Azure DevOps while it is signed in, held five minutes, or
    /// ten seconds when there is none.
    fn az_token(&self) -> Option<String> {
        let mut held = self.az.lock().unwrap();
        if let Some(cached) = held
            .as_ref()
            .filter(|cached| cached.expires > Instant::now())
        {
            return cached.value.clone();
        }
        let token = self.az_program().and_then(|program| {
            crate::forge::run_cli(
                program,
                &[
                    "account",
                    "get-access-token",
                    "--resource",
                    RESOURCE,
                    "--query",
                    "accessToken",
                    "--output",
                    "tsv",
                ],
                None,
                &self.environment,
                &[],
                true,
            )
        });
        let token = token
            .map(|token| token.trim().to_owned())
            .filter(|token| !token.is_empty());
        let ttl = if token.is_some() { 300 } else { 10 };
        *held = Some(Cached {
            value: token.clone(),
            expires: Instant::now() + Duration::from_secs(ttl),
        });
        token
    }

    /// The account the credential reads the organization as: `authenticatedUser` of its
    /// connection data. `None` anonymously.
    pub(super) fn viewer(&self, authority: &str) -> Result<Option<Value>, ForgeError> {
        let Some(credential) = self.credential(authority)? else {
            return Ok(None);
        };
        let slot = format!("{authority} {}", credential.fingerprint());
        if let Some(cached) = self.viewers.lock().unwrap().get(&slot)
            && cached.expires > Instant::now()
        {
            return Ok(Some(cached.value.clone()));
        }
        let data: Value = self
            .send(authority, Request::get("/_apis/connectionData", "Viewer"))?
            .json()?;
        let user = data["authenticatedUser"].clone();
        if user["id"].as_str().is_none() {
            return Err(error(
                ForgeErrorKind::Uncertain,
                "Azure DevOps account unreadable",
            ));
        }
        let mut viewers = self.viewers.lock().unwrap();
        viewers.retain(|_, cached| cached.expires > Instant::now());
        viewers.insert(
            slot,
            Cached {
                value: user.clone(),
                expires: Instant::now() + Duration::from_secs(300),
            },
        );
        Ok(Some(user))
    }

    pub(super) fn send(
        &self,
        authority: &str,
        request: Request<'_>,
    ) -> Result<Response, ForgeError> {
        let credential = self.credential(authority)?;
        let fingerprint = credential
            .as_ref()
            .map_or("anonymous", |credential| &credential.verification);
        let lease = self
            .rate_limits
            .lock()
            .unwrap()
            .admit(authority, fingerprint, false)
            .map_err(|retry_at| {
                error(
                    ForgeErrorKind::Paused { retry_at },
                    "Azure DevOps request paused",
                )
            })?;
        if request.write && credential.is_none() {
            return Err(error(
                ForgeErrorKind::NoCredential,
                "Azure DevOps credential unavailable",
            ));
        }
        let separator = if request.path.contains('?') { '&' } else { '?' };
        let url = format!(
            "https://{authority}{}{separator}api-version={}",
            request.path, request.version
        );
        let started = Instant::now();
        let mut call = self
            .agent
            .request(request.method, &url)
            .timeout(DEADLINE)
            .set("Accept", request.accept)
            .set("User-Agent", "tcode")
            // Without it a missing or refused sign-in answers with a redirect to a sign-in page.
            .set("X-TFS-FedAuthRedirect", "Suppress");
        if let Some(credential) = &credential {
            call = call.set("Authorization", &credential.authorization);
        }
        let answer = match &request.body {
            Some(body) => call
                .set(
                    "Content-Type",
                    if request.method == "PATCH" && body.is_array() {
                        "application/json-patch+json"
                    } else {
                        "application/json"
                    },
                )
                .send_bytes(&serde_json::to_vec(body).unwrap_or_default()),
            None => call.call(),
        };
        let response = match answer {
            Ok(response) | Err(ureq::Error::Status(_, response)) => response,
            Err(ureq::Error::Transport(_)) => {
                return Err(if started.elapsed() >= DEADLINE {
                    error(ForgeErrorKind::Deadline, "Azure DevOps request deadline")
                } else {
                    error(
                        ForgeErrorKind::Uncertain,
                        "Azure DevOps request failed (network)",
                    )
                });
            }
        };
        let status = response.status();
        log::debug!(
            "azure host={} path={} operation={} status={}",
            authority,
            request.path.split('?').next().unwrap_or("/"),
            request.operation,
            status
        );
        let header = |name: &str| response.header(name).map(str::to_owned);
        let retry_after = header("retry-after");
        let reset = header("x-ratelimit-reset");
        let limit = request.limit.min(BODY_LIMIT);
        let mut body = Vec::new();
        response
            .into_reader()
            .take(limit as u64 + 1)
            .read_to_end(&mut body)
            .map_err(|_| error(ForgeErrorKind::Uncertain, "Azure DevOps response cut off"))?;
        let truncated = body.len() > limit;
        body.truncate(limit);
        if status == 429 {
            let named = retry_after
                .and_then(|value| value.trim().parse().ok())
                .map(|seconds| SystemTime::now() + Duration::from_secs(seconds))
                .or_else(|| {
                    reset
                        .and_then(|value| value.trim().parse().ok())
                        .map(|at| UNIX_EPOCH + Duration::from_secs(at))
                });
            let retry_at =
                self.rate_limits
                    .lock()
                    .unwrap()
                    .refused(authority, fingerprint, lease, named);
            return Err(error(
                ForgeErrorKind::RateLimited { retry_at },
                "Azure DevOps request failed (rate limited)",
            ));
        }
        // A 203 is Azure's sign-in page served in place of the answer.
        if (200..300).contains(&status) && status != 203 {
            self.rate_limits
                .lock()
                .unwrap()
                .succeeded(authority, fingerprint, lease);
            if let Some(credential) = credential
                .as_ref()
                .filter(|credential| credential.source == CredentialSource::Saved)
                && let Err(error) = self.store.record_verification(
                    HostKind::Azure,
                    authority,
                    &credential.verification,
                    true,
                )
            {
                log::warn!("failed to save credential verification: {error}");
            }
            return Ok(Response { body, truncated });
        }
        Err(match status {
            // Anonymously, a resource a public project keeps to its members asks for a sign-in.
            203 | 300..=399 | 401 if credential.is_none() => error(
                ForgeErrorKind::NoCredential,
                "Azure DevOps request needs a sign-in",
            ),
            203 | 300..=399 | 401 => {
                if let Some(credential) = credential.as_ref()
                    && let Err(error) = self.store.record_verification(
                        HostKind::Azure,
                        authority,
                        &credential.verification,
                        false,
                    )
                {
                    log::warn!("failed to clear credential verification: {error}");
                }
                self.forget(authority);
                error(
                    ForgeErrorKind::Unauthorized,
                    "Azure DevOps request failed (unauthorized)",
                )
            }
            404 => error(
                ForgeErrorKind::NotFound,
                "Azure DevOps request failed (not found)",
            ),
            500.. => error(
                ForgeErrorKind::Uncertain,
                format!("Azure DevOps response failed ({status})"),
            ),
            _ => error(
                ForgeErrorKind::Refused {
                    messages: messages(&body),
                },
                format!("Azure DevOps response failed ({status})"),
            ),
        })
    }

    /// An attachment on the organization, read with its credential.
    pub(super) fn media(
        &self,
        authority: &str,
        url: &url::Url,
        validator: Option<&str>,
    ) -> Result<tcode_protocol::PullRequestMedia, ForgeError> {
        let authorization = self
            .credential(authority)
            .ok()
            .flatten()
            .map(|credential| credential.authorization);
        crate::forge::media(
            &self.agent,
            url,
            authorization.as_deref(),
            validator,
            "Azure DevOps",
        )
    }
}

/// Azure's words for a refusal: its `message`.
pub(super) fn messages(body: &[u8]) -> Vec<String> {
    serde_json::from_slice::<Value>(body)
        .ok()
        .and_then(|value| value["message"].as_str().map(str::to_owned))
        .into_iter()
        .collect()
}

#[cfg(test)]
mod tests {
    /// A saved token belongs to its organization alone. `AZURE_DEVOPS_EXT_PAT` goes only to the
    /// organization `AZURE_DEVOPS_ORG` names, however it is written, and nowhere without it;
    /// az's token goes as a bearer to an organization with neither. A token is sent as Basic
    /// with an empty user.
    #[cfg(unix)]
    #[test]
    fn a_token_goes_only_to_the_organization_it_is_for() {
        use super::*;
        use base64::Engine as _;
        use std::os::unix::fs::PermissionsExt as _;
        let root = std::env::temp_dir().join(format!("tcode-azure-token-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let az = root.join("az");
        std::fs::write(
            &az,
            "#!/bin/sh\n[ \"$1 $2 $4\" = \"account get-access-token 499b84ac-1321-427f-aa17-267ca6975798\" ] && echo az-secret\n",
        )
        .unwrap();
        std::fs::set_permissions(&az, std::fs::Permissions::from_mode(0o700)).unwrap();
        let path = ("PATH".to_owned(), root.to_string_lossy().into_owned());
        let store = SettingsStore::new(root.clone());
        let read = |api: &Api, authority: &str| {
            api.credential(authority)
                .unwrap()
                .map(|credential| (credential.source, credential.authorization))
        };
        let basic = |token: &str| {
            format!(
                "Basic {}",
                base64::engine::general_purpose::STANDARD.encode(format!(":{token}"))
            )
        };
        let pat = (ENV_TOKEN.to_owned(), "env-secret".to_owned());
        let env = Some((
            CredentialSource::Env {
                name: ENV_TOKEN.into(),
            },
            basic("env-secret"),
        ));
        let cli = Some((
            CredentialSource::Cli { tool: "az".into() },
            "Bearer az-secret".to_owned(),
        ));
        let unnamed = Api::new(store.clone(), [pat.clone()]);
        assert_eq!(read(&unnamed, "dev.azure.com/fabrikam"), None);
        let named = Api::new(
            store.clone(),
            [
                path,
                pat,
                (
                    ENV_ORGANIZATION.to_owned(),
                    "https://dev.azure.com/Fabrikam/".to_owned(),
                ),
            ],
        );
        assert_eq!(read(&named, "dev.azure.com/fabrikam"), env);
        assert_eq!(read(&named, "dev.azure.com/contoso"), cli);
        store
            .set_token(
                HostKind::Azure,
                "dev.azure.com/contoso",
                Some("contoso-secret"),
            )
            .unwrap();
        assert_eq!(
            read(&named, "dev.azure.com/contoso"),
            Some((CredentialSource::Saved, basic("contoso-secret")))
        );
        assert_eq!(read(&named, "dev.azure.com/fabrikam"), env);
        assert_eq!(read(&named, "dev.azure.com/tailspin"), cli);
        // A host that is no organization's never gets one, whatever the key carries.
        for elsewhere in [
            "dev.azure.com:x@evil.test",
            "evil.test",
            "dev.azure.com/Contoso",
        ] {
            assert_eq!(
                named
                    .credential(elsewhere)
                    .map(|_| ())
                    .map_err(|error| error.kind),
                Err(ForgeErrorKind::InvalidInput),
                "{elsewhere}"
            );
        }
        let _ = std::fs::remove_dir_all(root);
    }
}
