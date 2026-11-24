//! Forgejo and Gitea REST, each server reached directly with its own token. Reads of a public
//! repository need none; writes do.

use crate::{
    forge::{ForgeError, ForgeErrorKind},
    settings::SettingsStore,
};
use std::{
    collections::{BTreeMap, HashMap},
    io::Read as _,
    path::PathBuf,
    sync::{Arc, Mutex, RwLock},
    time::{Duration, Instant, SystemTime},
};
use tcode_core::{
    pull_request::HostKind,
    settings::{CredentialSource, HostSettings},
};

const BODY_LIMIT: usize = 8 * 1024 * 1024;
const DEADLINE: Duration = Duration::from_secs(60);
/// The environment token tea itself reads, for the server its companion variable names.
pub(super) const ENV_TOKEN: &str = "GITEA_TOKEN";
const ENV_URL: &str = "GITEA_INSTANCE_URL";

pub(super) fn error(kind: ForgeErrorKind, description: impl Into<String>) -> ForgeError {
    ForgeError {
        kind,
        description: description.into(),
    }
}

/// The software and version a server reports, where Forgejo and Gitea's APIs differ.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Server {
    /// Gitea's `major.minor`; `None` for Forgejo, which reports its own numbering with a
    /// `+gitea-` suffix, and for a server whose version could not be read.
    gitea: Option<(u64, u64)>,
}

impl Server {
    pub(super) fn parse(version: &str) -> Self {
        let gitea = (!version.contains("+gitea-"))
            .then(|| {
                let mut parts = version.split(['.', '+', '-']);
                Some((parts.next()?.parse().ok()?, parts.next()?.parse().ok()?))
            })
            .flatten();
        Self { gitea }
    }
    fn at_least(&self, version: (u64, u64)) -> bool {
        self.gitea.is_some_and(|gitea| gitea >= version)
    }
    /// Resolving review comments, added in Gitea 1.26.
    pub(super) fn resolves(&self) -> bool {
        self.at_least((1, 26))
    }
    /// Replying to a review comment, added in Gitea 1.27.
    pub(super) fn replies(&self) -> bool {
        self.at_least((1, 27))
    }
    /// Gitea 1.26 renamed the merge body's title and message fields to snake case.
    pub(super) fn snake_case_merge(&self) -> bool {
        self.at_least((1, 26))
    }
}

#[derive(Clone)]
pub(super) struct Credential {
    token: String,
    pub(super) source: CredentialSource,
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
                "Forgejo response too large",
            ));
        }
        serde_json::from_slice(&self.body)
            .map_err(|_| error(ForgeErrorKind::Uncertain, "Forgejo response unreadable"))
    }
}

pub(super) struct Request<'a> {
    pub(super) method: &'a str,
    /// Below `/api/v1`, query included.
    pub(super) path: String,
    pub(super) body: Option<serde_json::Value>,
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
        body: Option<serde_json::Value>,
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
    agent: ureq::Agent,
    tokens: Mutex<HashMap<String, Cached<Option<Credential>>>>,
    servers: Mutex<HashMap<String, Cached<Server>>>,
    rate_limits: Mutex<crate::forge::RateLimits>,
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
            agent: ureq::AgentBuilder::new()
                .redirects(0)
                .max_idle_connections(0)
                .build(),
            tokens: Mutex::default(),
            servers: Mutex::default(),
            rate_limits: Mutex::default(),
        })
    }

    pub(super) fn configure(&self, hosts: BTreeMap<String, HostSettings>) {
        *self.hosts.write().unwrap() = hosts;
    }

    pub(super) fn configured(&self) -> BTreeMap<String, HostSettings> {
        self.hosts.read().unwrap().clone()
    }

    pub(super) fn enabled(&self, authority: &str) -> bool {
        self.hosts
            .read()
            .unwrap()
            .get(authority)
            .is_none_or(|choice| choice.enabled)
    }

    /// The token saved for the server, then `GITEA_TOKEN` when `GITEA_INSTANCE_URL` names it,
    /// then the one tea stored for it. `None` reads anonymously.
    pub(super) fn credential(&self, authority: &str) -> Result<Option<Credential>, ForgeError> {
        if !self.enabled(authority) {
            return Err(error(
                ForgeErrorKind::HostDisabled,
                "Forgejo host turned off",
            ));
        }
        let kind = self
            .hosts
            .read()
            .unwrap()
            .get(authority)
            .map(|choice| choice.kind)
            .unwrap_or(HostKind::Forgejo);
        if let Some(token) = self.store.token(kind, authority).and_then(nonempty) {
            return Ok(Some(Credential {
                token,
                source: CredentialSource::Saved,
            }));
        }
        if let Some(token) = self.environment_token(authority) {
            return Ok(Some(Credential {
                token,
                source: CredentialSource::Env {
                    name: ENV_TOKEN.into(),
                },
            }));
        }
        let mut tokens = self.tokens.lock().unwrap();
        tokens.retain(|_, cached| cached.expires > Instant::now());
        if let Some(cached) = tokens.get(authority) {
            return Ok(cached.value.clone());
        }
        let found = self.tea_token(authority);
        let ttl = if found.is_some() { 300 } else { 10 };
        let value = found.map(|token| Credential {
            token,
            source: CredentialSource::Cli { tool: "tea".into() },
        });
        tokens.insert(
            authority.to_owned(),
            Cached {
                value: value.clone(),
                expires: Instant::now() + Duration::from_secs(ttl),
            },
        );
        Ok(value)
    }

    pub(super) fn forget(&self, authority: &str) {
        self.tokens.lock().unwrap().remove(authority);
    }

    pub(super) fn token_saved(&self, kind: HostKind, authority: &str) -> bool {
        self.store
            .token(kind, authority)
            .and_then(nonempty)
            .is_some()
    }

    fn environment_token(&self, authority: &str) -> Option<String> {
        let named = self.environment.get(ENV_URL)?;
        (HostKind::Gitea.authority(named).ok()? == authority)
            .then(|| self.environment.get(ENV_TOKEN).cloned().and_then(nonempty))?
    }

    /// The server `GITEA_INSTANCE_URL` names, when it is one.
    pub(super) fn environment_host(&self) -> Option<String> {
        HostKind::Gitea
            .authority(self.environment.get(ENV_URL)?)
            .ok()
    }

    pub(super) fn tea_program(&self) -> Option<PathBuf> {
        let path = self.environment.get("PATH")?;
        std::env::split_paths(path)
            .map(|dir| dir.join(if cfg!(windows) { "tea.exe" } else { "tea" }))
            .find(|program| program.is_file())
    }

    /// tea's git credential helper answers with the stored token of the login whose URL is the
    /// server's; `tea login list` never prints one.
    fn tea_token(&self, authority: &str) -> Option<String> {
        let (host, path) = authority.split_once('/').unwrap_or((authority, ""));
        let mut input = format!("protocol=https\nhost={host}\n");
        if !path.is_empty() {
            input.push_str(&format!("path={path}\n"));
        }
        input.push('\n');
        let output = self.run_tea(&["login", "helper", "get"], Some(&input))?;
        output
            .lines()
            .find_map(|line| line.strip_prefix("password="))
            .map(str::to_owned)
            .and_then(nonempty)
    }

    /// The servers tea has logins for.
    pub(super) fn tea_logins(&self) -> Vec<String> {
        let Some(output) = self.run_tea(&["logins", "list", "--output", "json"], None) else {
            return Vec::new();
        };
        let logins: Vec<serde_json::Value> = serde_json::from_str(&output).unwrap_or_default();
        logins
            .iter()
            .filter_map(|login| login["url"].as_str())
            .filter_map(|url| HostKind::Gitea.authority(url).ok())
            .collect()
    }

    fn run_tea(&self, args: &[&str], input: Option<&str>) -> Option<String> {
        // tea would otherwise prefer these over its stored login.
        crate::forge::run_cli(
            self.tea_program()?,
            args,
            input,
            &self.environment,
            &[ENV_TOKEN, ENV_URL],
            true,
        )
    }

    /// What the server runs, read once an hour.
    pub(super) fn server(&self, authority: &str) -> Server {
        if let Some(cached) = self.servers.lock().unwrap().get(authority)
            && cached.expires > Instant::now()
        {
            return cached.value;
        }
        let read = self
            .send(
                authority,
                Request {
                    operation: "Version",
                    ..Request::get("/version", "Version")
                },
            )
            .and_then(|response| response.json::<serde_json::Value>());
        let (value, ttl) = match read {
            Ok(version) => (
                Server::parse(version["version"].as_str().unwrap_or_default()),
                3600,
            ),
            Err(_) => (Server { gitea: None }, 60),
        };
        self.servers.lock().unwrap().insert(
            authority.to_owned(),
            Cached {
                value,
                expires: Instant::now() + Duration::from_secs(ttl),
            },
        );
        value
    }

    pub(super) fn send(
        &self,
        authority: &str,
        request: Request<'_>,
    ) -> Result<Response, ForgeError> {
        let credential = self.credential(authority)?;
        let fingerprint = credential.as_ref().map_or_else(
            || "anonymous".to_owned(),
            |credential| crate::settings::credential_fingerprint(&credential.token, None),
        );
        let lease = self
            .rate_limits
            .lock()
            .unwrap()
            .admit(authority, &fingerprint, false)
            .map_err(|retry_at| {
                error(
                    ForgeErrorKind::Paused { retry_at },
                    "Forgejo request paused",
                )
            })?;
        if request.write && credential.is_none() {
            return Err(error(
                ForgeErrorKind::NoCredential,
                "Forgejo credential unavailable",
            ));
        }
        let url = format!("https://{authority}/api/v1{}", request.path);
        let started = Instant::now();
        let mut call = self
            .agent
            .request(request.method, &url)
            .timeout(DEADLINE)
            .set("Accept", request.accept)
            .set("User-Agent", "tcode");
        if let Some(credential) = &credential {
            call = call.set("Authorization", &format!("token {}", credential.token));
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
                    error(ForgeErrorKind::Deadline, "Forgejo request deadline")
                } else {
                    error(
                        ForgeErrorKind::Uncertain,
                        "Forgejo request failed (network)",
                    )
                });
            }
        };
        let status = response.status();
        log::debug!(
            "forgejo host={} path={} operation={} status={}",
            authority,
            request.path.split('?').next().unwrap_or("/"),
            request.operation,
            status
        );
        let headers: BTreeMap<_, _> = response
            .headers_names()
            .into_iter()
            .filter_map(|name| {
                response
                    .header(&name)
                    .map(|value| (name.to_ascii_lowercase(), value.to_owned()))
            })
            .collect();
        let limit = request.limit.min(BODY_LIMIT);
        let mut body = Vec::new();
        response
            .into_reader()
            .take(limit as u64 + 1)
            .read_to_end(&mut body)
            .map_err(|_| error(ForgeErrorKind::Uncertain, "Forgejo response cut off"))?;
        let truncated = body.len() > limit;
        body.truncate(limit);
        if status == 429 {
            let named = headers
                .get("retry-after")
                .and_then(|value| value.trim().parse().ok())
                .map(|seconds| SystemTime::now() + Duration::from_secs(seconds));
            let retry_at =
                self.rate_limits
                    .lock()
                    .unwrap()
                    .refused(authority, &fingerprint, lease, named);
            return Err(error(
                ForgeErrorKind::RateLimited { retry_at },
                "Forgejo request failed (rate limited)",
            ));
        }
        if (200..300).contains(&status) {
            self.rate_limits
                .lock()
                .unwrap()
                .succeeded(authority, &fingerprint, lease);
            return Ok(Response { body, truncated });
        }
        let message = serde_json::from_slice::<serde_json::Value>(&body)
            .ok()
            .and_then(|value| value["message"].as_str().map(str::to_owned));
        Err(match status {
            401 => {
                self.forget(authority);
                error(
                    ForgeErrorKind::Unauthorized,
                    "Forgejo request failed (unauthorized)",
                )
            }
            404 => error(
                ForgeErrorKind::NotFound,
                "Forgejo request failed (not found)",
            ),
            500.. => error(
                ForgeErrorKind::Uncertain,
                format!("Forgejo response failed ({status})"),
            ),
            _ => error(
                ForgeErrorKind::Refused {
                    messages: message.into_iter().collect(),
                },
                format!("Forgejo response failed ({status})"),
            ),
        })
    }

    /// A list read page by page while each page is full, up to `pages`; `false` when more
    /// remained. A server may cap a page below the size asked for, so a page is full at the
    /// size the first one came back with.
    pub(super) fn list(
        &self,
        authority: &str,
        path: &str,
        operation: &'static str,
        pages: usize,
    ) -> Result<(Vec<serde_json::Value>, bool), ForgeError> {
        let separator = if path.contains('?') { '&' } else { '?' };
        let mut rows = Vec::new();
        let mut size = None;
        for page in 1..=pages {
            let batch: Vec<serde_json::Value> = self
                .send(
                    authority,
                    Request::get(
                        format!("{path}{separator}limit={PAGE_LIMIT}&page={page}"),
                        operation,
                    ),
                )?
                .json()?;
            let full = page_full(batch.len(), size.get_or_insert(batch.len()));
            rows.extend(batch);
            if !full {
                return Ok((rows, true));
            }
        }
        Ok((rows, false))
    }
}

impl Api {
    /// An upload or avatar on the server, read with its token. `authority` is the server the
    /// pull request is on, mount path included, whose credential the URL's host is sent.
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
            .map(|credential| format!("token {}", credential.token));
        crate::forge::media(
            &self.agent,
            url,
            authorization.as_deref(),
            validator,
            "Forgejo",
        )
    }
}

/// The page size asked for; a server may answer fewer.
pub(super) const PAGE_LIMIT: usize = 50;

/// Whether a page of `rows` was full, so another may follow: a server may cap a page below
/// [`PAGE_LIMIT`], so a page is full at the size the first page came back with.
pub(super) fn page_full(rows: usize, first: &usize) -> bool {
    rows > 0 && rows >= *first
}

fn nonempty(value: String) -> Option<String> {
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_owned())
}

#[cfg(test)]
mod tests {
    use super::Server;

    /// `GITEA_TOKEN` goes only to the server `GITEA_INSTANCE_URL` names, and tea's token for one
    /// server never to another: tea is asked for each server by its own host.
    #[cfg(unix)]
    #[test]
    fn a_token_goes_only_to_the_server_it_is_for() {
        use super::Api;
        use std::os::unix::fs::PermissionsExt as _;
        use tcode_core::settings::CredentialSource;
        let root =
            std::env::temp_dir().join(format!("tcode-forgejo-token-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let tea = root.join("tea");
        std::fs::write(
            &tea,
            "#!/bin/sh\nwhile read -r line; do\n  [ \"$line\" = host=tea.test ] && echo password=tea-secret\ndone\nexit 0\n",
        )
        .unwrap();
        std::fs::set_permissions(&tea, std::fs::Permissions::from_mode(0o700)).unwrap();
        let api = Api::new(
            crate::settings::SettingsStore::new(root.clone()),
            [
                ("PATH".to_owned(), root.to_string_lossy().into_owned()),
                (
                    "GITEA_INSTANCE_URL".to_owned(),
                    "https://env.test/".to_owned(),
                ),
                ("GITEA_TOKEN".to_owned(), "env-secret".to_owned()),
            ],
        );
        let source = |authority: &str| {
            api.credential(authority)
                .unwrap()
                .map(|credential| (credential.source, credential.token))
        };
        assert_eq!(
            source("env.test"),
            Some((
                CredentialSource::Env {
                    name: "GITEA_TOKEN".into()
                },
                "env-secret".into()
            ))
        );
        assert_eq!(
            source("tea.test"),
            Some((
                CredentialSource::Cli { tool: "tea".into() },
                "tea-secret".into()
            ))
        );
        assert_eq!(source("other.test"), None);
        assert_eq!(source("tea.test.evil"), None);
        let _ = std::fs::remove_dir_all(root);
    }

    /// Forgejo reports its own numbering with the Gitea it forked from, and has neither of
    /// Gitea's newer review endpoints; Gitea gains resolve in 1.26 and replies in 1.27.
    #[test]
    fn a_server_version_says_which_api_it_speaks() {
        let forgejo = Server::parse("16.0.0-dev-753-6bcc6da0+gitea-1.22.0");
        assert!(!forgejo.resolves() && !forgejo.replies() && !forgejo.snake_case_merge());
        let gitea_125 = Server::parse("1.25.5");
        assert!(!gitea_125.resolves() && !gitea_125.snake_case_merge());
        let gitea_126 = Server::parse("1.26.0");
        assert!(gitea_126.resolves() && gitea_126.snake_case_merge() && !gitea_126.replies());
        let gitea_dev = Server::parse("1.27.0+dev-1118-ge629c4fdc2");
        assert!(gitea_dev.resolves() && gitea_dev.replies());
    }
}
