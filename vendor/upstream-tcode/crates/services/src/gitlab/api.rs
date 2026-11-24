//! GitLab REST v4 and GraphQL, each server reached directly with its own token. Reads of a
//! public project need none; writes do.

use crate::{
    forge::{ForgeError, ForgeErrorKind},
    settings::SettingsStore,
};
use serde_json::{Value, json};
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

const BODY_LIMIT: usize = 8 * 1024 * 1024;
const DEADLINE: Duration = Duration::from_secs(60);
/// The environment token glab itself reads, for the server its host variables name, else
/// gitlab.com.
pub(super) const ENV_TOKEN: &str = "GITLAB_TOKEN";
/// The variables glab reads its server from, first set first (glab's `internal/config/schema.go`).
const ENV_HOSTS: [&str; 3] = ["GITLAB_HOST", "GITLAB_URI", "GL_HOST"];
/// Every variable glab would prefer over its stored login.
const GLAB_ENVIRONMENT: &[&str] = &[
    ENV_TOKEN,
    "GITLAB_ACCESS_TOKEN",
    "OAUTH_TOKEN",
    "GITLAB_HOST",
    "GITLAB_URI",
    "GL_HOST",
];

pub(super) fn error(kind: ForgeErrorKind, description: impl Into<String>) -> ForgeError {
    ForgeError {
        kind,
        description: description.into(),
    }
}

#[derive(Clone)]
pub(super) struct Credential {
    token: String,
    verification: String,
    pub(super) source: CredentialSource,
}

#[derive(Debug)]
pub(super) struct Response {
    pub(super) body: Vec<u8>,
    pub(super) truncated: bool,
    /// The page after this one, as `X-Next-Page` names it.
    pub(super) next_page: Option<u32>,
}

impl Response {
    pub(super) fn json<T: serde::de::DeserializeOwned>(&self) -> Result<T, ForgeError> {
        if self.truncated {
            return Err(error(ForgeErrorKind::TooLarge, "GitLab response too large"));
        }
        serde_json::from_slice(&self.body)
            .map_err(|_| error(ForgeErrorKind::Uncertain, "GitLab response unreadable"))
    }
}

pub(super) struct Request<'a> {
    pub(super) method: &'a str,
    /// Below `/api/v4`, query included.
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

struct Cached<V> {
    value: V,
    expires: Instant,
}

pub(super) struct Api {
    store: SettingsStore,
    environment: BTreeMap<String, String>,
    hosts: RwLock<BTreeMap<String, HostSettings>>,
    pub(super) agent: ureq::Agent,
    tokens: Mutex<HashMap<String, Cached<Option<Credential>>>>,
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

    /// The token saved for the server, then `GITLAB_TOKEN` when the server is the one
    /// `GITLAB_HOST` names (gitlab.com without it), then the one glab stored for it. `None`
    /// reads anonymously.
    pub(super) fn credential(&self, authority: &str) -> Result<Option<Credential>, ForgeError> {
        if !self.enabled(authority) {
            return Err(error(
                ForgeErrorKind::HostDisabled,
                "GitLab host turned off",
            ));
        }
        if let Some(token) = self
            .store
            .token(HostKind::Gitlab, authority)
            .and_then(nonempty)
        {
            return Ok(Some(Credential {
                verification: crate::settings::credential_fingerprint(&token, None),
                token,
                source: CredentialSource::Saved,
            }));
        }
        if let Some(token) = self.environment_token(authority) {
            return Ok(Some(Credential {
                verification: crate::settings::credential_fingerprint(&token, None),
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
        let found = self.glab_token(authority);
        let ttl = if found.is_some() { 300 } else { 10 };
        let value = found.map(|token| Credential {
            verification: crate::settings::credential_fingerprint(&token, None),
            token,
            source: CredentialSource::Cli {
                tool: "glab".into(),
            },
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

    pub(super) fn token_saved(&self, authority: &str) -> bool {
        self.store
            .token(HostKind::Gitlab, authority)
            .and_then(nonempty)
            .is_some()
    }

    /// Whether the server has answered a request sent with `credential`, a saved token.
    pub(super) fn verified(&self, authority: &str, credential: &Credential) -> bool {
        self.store
            .credential_verified(HostKind::Gitlab, authority, &credential.verification)
    }

    fn environment_token(&self, authority: &str) -> Option<String> {
        (self.environment_host()? == authority)
            .then(|| self.environment.get(ENV_TOKEN).cloned().and_then(nonempty))?
    }

    /// The server `GITLAB_TOKEN` is for, when the variable is set: the one the first of glab's
    /// host variables names, else gitlab.com. A host variable that names no server sends the
    /// token nowhere.
    pub(super) fn environment_host(&self) -> Option<String> {
        self.environment.get(ENV_TOKEN)?;
        match ENV_HOSTS
            .iter()
            .find_map(|name| self.environment.get(*name))
        {
            Some(named) => HostKind::Gitlab.authority(named).ok(),
            None => Some(HostKind::Gitlab.public_host().to_owned()),
        }
    }

    pub(super) fn glab_program(&self) -> Option<PathBuf> {
        let path = self.environment.get("PATH")?;
        std::env::split_paths(path)
            .map(|dir| dir.join(if cfg!(windows) { "glab.exe" } else { "glab" }))
            .find(|program| program.is_file())
    }

    /// glab's git credential helper answers with the stored token of the server named by
    /// host; `glab auth status` prints one only to a terminal reader.
    fn glab_token(&self, authority: &str) -> Option<String> {
        let input = format!("protocol=https\nhost={authority}\n\n");
        let output = self.run_glab(&["auth", "git-credential", "get"], Some(&input), true)?;
        output
            .lines()
            .find_map(|line| line.strip_prefix("password="))
            .map(str::to_owned)
            .and_then(nonempty)
    }

    /// The servers glab has logins for. glab exits with a failure when any of them refuses its
    /// token, and still lists them all.
    pub(super) fn glab_logins(&self) -> Vec<String> {
        let Some(output) = self.run_glab(
            &["auth", "status", "--all", "--output", "json"],
            None,
            false,
        ) else {
            return Vec::new();
        };
        let status: Value = serde_json::from_str(&output).unwrap_or_default();
        status["hosts"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|host| host["host"].as_str())
            .filter_map(|host| HostKind::Gitlab.authority(host).ok())
            .collect()
    }

    fn run_glab(&self, args: &[&str], input: Option<&str>, success: bool) -> Option<String> {
        crate::forge::run_cli(
            self.glab_program()?,
            args,
            input,
            &self.environment,
            GLAB_ENVIRONMENT,
            success,
        )
    }

    pub(super) fn send(
        &self,
        authority: &str,
        request: Request<'_>,
    ) -> Result<Response, ForgeError> {
        self.call(authority, &format!("/api/v4{}", request.path), request)
    }

    /// A GraphQL query's `data`. GitLab answers a query it could not run in full with errors
    /// beside what it could; any error fails the read.
    pub(super) fn graphql(
        &self,
        authority: &str,
        query: &str,
        variables: Value,
        operation: &'static str,
    ) -> Result<Value, ForgeError> {
        self.graphql_request(authority, query, variables, operation, false)
    }

    /// A GraphQL mutation, which goes only with a credential.
    pub(super) fn graphql_write(
        &self,
        authority: &str,
        query: &str,
        variables: Value,
        operation: &'static str,
    ) -> Result<Value, ForgeError> {
        self.graphql_request(authority, query, variables, operation, true)
    }

    fn graphql_request(
        &self,
        authority: &str,
        query: &str,
        variables: Value,
        operation: &'static str,
        write: bool,
    ) -> Result<Value, ForgeError> {
        let response: Value = self
            .call(
                authority,
                "/api/graphql",
                Request {
                    method: "POST",
                    body: Some(json!({ "query": query, "variables": variables })),
                    write,
                    ..Request::get("/graphql", operation)
                },
            )?
            .json()?;
        if let Some(errors) = response["errors"]
            .as_array()
            .filter(|errors| !errors.is_empty())
        {
            let messages: Vec<_> = errors
                .iter()
                .filter_map(|error| error["message"].as_str().map(str::to_owned))
                .collect();
            return Err(error(
                ForgeErrorKind::Refused { messages },
                "GitLab query failed",
            ));
        }
        Ok(response["data"].clone())
    }

    fn call(
        &self,
        authority: &str,
        path: &str,
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
                error(ForgeErrorKind::Paused { retry_at }, "GitLab request paused")
            })?;
        if request.write && credential.is_none() {
            return Err(error(
                ForgeErrorKind::NoCredential,
                "GitLab credential unavailable",
            ));
        }
        let url = format!("https://{authority}{path}");
        let started = Instant::now();
        let mut call = self
            .agent
            .request(request.method, &url)
            .timeout(DEADLINE)
            .set("Accept", request.accept)
            .set("User-Agent", "tcode");
        if let Some(credential) = &credential {
            // GitLab takes a personal access token and glab's OAuth token alike as a bearer.
            call = call.set("Authorization", &format!("Bearer {}", credential.token));
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
                    error(ForgeErrorKind::Deadline, "GitLab request deadline")
                } else {
                    error(ForgeErrorKind::Uncertain, "GitLab request failed (network)")
                });
            }
        };
        let status = response.status();
        log::debug!(
            "gitlab host={} path={} operation={} status={}",
            authority,
            request.path.split('?').next().unwrap_or("/"),
            request.operation,
            status
        );
        let header = |name: &str| response.header(name).map(str::to_owned);
        let next_page = header("x-next-page").and_then(|page| page.trim().parse().ok());
        let retry_after = header("retry-after");
        let reset = header("ratelimit-reset");
        let limit = request.limit.min(BODY_LIMIT);
        let mut body = Vec::new();
        response
            .into_reader()
            .take(limit as u64 + 1)
            .read_to_end(&mut body)
            .map_err(|_| error(ForgeErrorKind::Uncertain, "GitLab response cut off"))?;
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
                "GitLab request failed (rate limited)",
            ));
        }
        if (200..300).contains(&status) {
            self.rate_limits
                .lock()
                .unwrap()
                .succeeded(authority, fingerprint, lease);
            if let Some(credential) = credential
                .as_ref()
                .filter(|credential| credential.source == CredentialSource::Saved)
                && let Err(error) = self.store.record_verification(
                    HostKind::Gitlab,
                    authority,
                    &credential.verification,
                    true,
                )
            {
                log::warn!("failed to save credential verification: {error}");
            }
            return Ok(Response {
                body,
                truncated,
                next_page,
            });
        }
        Err(match status {
            401 => {
                if let Some(credential) = credential.as_ref()
                    && let Err(error) = self.store.record_verification(
                        HostKind::Gitlab,
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
                    "GitLab request failed (unauthorized)",
                )
            }
            404 => error(
                ForgeErrorKind::NotFound,
                "GitLab request failed (not found)",
            ),
            500.. => error(
                ForgeErrorKind::Uncertain,
                format!("GitLab response failed ({status})"),
            ),
            _ => error(
                ForgeErrorKind::Refused {
                    messages: messages(&body),
                },
                format!("GitLab response failed ({status})"),
            ),
        })
    }

    /// A list read page by page while GitLab names a next page, up to `pages`; `false` when
    /// more remained.
    pub(super) fn list(
        &self,
        authority: &str,
        path: &str,
        operation: &'static str,
        pages: usize,
    ) -> Result<(Vec<Value>, bool), ForgeError> {
        let separator = if path.contains('?') { '&' } else { '?' };
        let mut rows = Vec::new();
        let mut page = 1;
        for _ in 0..pages {
            let response = self.send(
                authority,
                Request::get(
                    format!("{path}{separator}per_page={PAGE_LIMIT}&page={page}"),
                    operation,
                ),
            )?;
            let batch: Vec<Value> = response.json()?;
            rows.extend(batch);
            match response.next_page {
                Some(next) if next > page => page = next,
                _ => return Ok((rows, true)),
            }
        }
        Ok((rows, false))
    }

    /// An upload or avatar on the server, read with its token.
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
            .map(|credential| format!("Bearer {}", credential.token));
        crate::forge::media(
            &self.agent,
            url,
            authorization.as_deref(),
            validator,
            "GitLab",
        )
    }
}

/// GitLab's words for a refusal: `message` as text, as a field → messages map, or `error`.
pub(super) fn messages(body: &[u8]) -> Vec<String> {
    let Ok(value) = serde_json::from_slice::<Value>(body) else {
        return Vec::new();
    };
    match &value["message"] {
        Value::String(message) => vec![message.clone()],
        Value::Object(fields) => fields
            .iter()
            .flat_map(|(field, said)| match said {
                Value::Array(items) => items
                    .iter()
                    .filter_map(|item| item.as_str())
                    .map(|item| format!("{field} {item}"))
                    .collect(),
                Value::String(item) => vec![format!("{field} {item}")],
                _ => Vec::new(),
            })
            .collect(),
        Value::Array(items) => items
            .iter()
            .filter_map(|item| item.as_str().map(str::to_owned))
            .collect(),
        _ => value["error"]
            .as_str()
            .map(str::to_owned)
            .into_iter()
            .collect(),
    }
}

/// The page size asked for.
pub(super) const PAGE_LIMIT: usize = 100;

fn nonempty(value: String) -> Option<String> {
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_owned())
}

#[cfg(test)]
mod tests {
    /// `GITLAB_TOKEN` goes only to the server glab's host variables name (`GITLAB_HOST`, else
    /// `GL_HOST`), gitlab.com without them, and glab's token for one server never to another:
    /// glab is asked for each server by its own host, with none of those variables.
    #[cfg(unix)]
    #[test]
    fn a_token_goes_only_to_the_server_it_is_for() {
        use super::Api;
        use std::os::unix::fs::PermissionsExt as _;
        use tcode_core::settings::CredentialSource;
        let root =
            std::env::temp_dir().join(format!("tcode-gitlab-token-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let glab = root.join("glab");
        std::fs::write(
            &glab,
            "#!/bin/sh\n[ -n \"$GITLAB_TOKEN$GITLAB_HOST$GITLAB_URI$GL_HOST\" ] && exit 1\nwhile read -r line; do\n  [ \"$line\" = host=glab.test:8443 ] && echo password=glab-secret\ndone\nexit 0\n",
        )
        .unwrap();
        std::fs::set_permissions(&glab, std::fs::Permissions::from_mode(0o700)).unwrap();
        let path = ("PATH".to_owned(), root.to_string_lossy().into_owned());
        let token = ("GITLAB_TOKEN".to_owned(), "env-secret".to_owned());
        let source = |api: &Api, authority: &str| {
            api.credential(authority)
                .unwrap()
                .map(|credential| (credential.source, credential.token))
        };
        let env = Some((
            CredentialSource::Env {
                name: "GITLAB_TOKEN".into(),
            },
            "env-secret".to_owned(),
        ));
        let store = || crate::settings::SettingsStore::new(root.clone());
        let public = Api::new(store(), [path.clone(), token.clone()]);
        assert_eq!(source(&public, "gitlab.com"), env);
        assert_eq!(source(&public, "gitlab.acme.test"), None);
        let gl_host = (
            "GL_HOST".to_owned(),
            "https://gitlab.corp.example".to_owned(),
        );
        let corp = Api::new(store(), [path.clone(), token.clone(), gl_host.clone()]);
        assert_eq!(source(&corp, "gitlab.corp.example"), env);
        assert_eq!(source(&corp, "gitlab.com"), None);
        let named = Api::new(
            store(),
            [
                path,
                token,
                gl_host,
                (
                    "GITLAB_HOST".to_owned(),
                    "https://gitlab.acme.test/".to_owned(),
                ),
            ],
        );
        assert_eq!(source(&named, "gitlab.acme.test"), env);
        assert_eq!(source(&named, "gitlab.com"), None);
        assert_eq!(source(&named, "gitlab.corp.example"), None);
        assert_eq!(
            source(&named, "glab.test:8443"),
            Some((
                CredentialSource::Cli {
                    tool: "glab".into()
                },
                "glab-secret".into()
            ))
        );
        assert_eq!(source(&named, "glab.test"), None);
        assert_eq!(source(&named, "glab.test.evil"), None);
        let _ = std::fs::remove_dir_all(root);
    }
}
