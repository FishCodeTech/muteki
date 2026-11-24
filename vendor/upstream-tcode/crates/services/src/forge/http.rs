//! What every host reached over its own HTTP API does alike: how a sent write's answer reads,
//! a write made of several requests, media read with the host's credential, and a host CLI
//! asked for a stored login.

use super::{ForgeError, ForgeErrorKind};
use std::{
    collections::BTreeMap,
    io::Read as _,
    path::PathBuf,
    time::{Duration, Instant, SystemTime},
};
use tcode_protocol::{
    MAX_PULL_REQUEST_MEDIA_BYTES, PullRequestActionResult as Outcome, PullRequestMedia,
};

const DEADLINE: Duration = Duration::from_secs(60);

/// The answer to a write that was sent. Without an answer, or with a server failure, the host
/// may have applied it; a refusal or a rate limit says it did not.
pub(crate) fn answered<T>(result: Result<T, ForgeError>) -> Outcome {
    match result {
        Ok(_) => Outcome::Applied,
        Err(ForgeError {
            kind: ForgeErrorKind::Uncertain | ForgeErrorKind::Deadline | ForgeErrorKind::TooLarge,
            ..
        }) => Outcome::Uncertain,
        Err(error) => Outcome::Rejected(error.rejection()),
    }
}

/// One request of a write made of several, and the names it covers.
pub(crate) type Step<'a> = (Vec<String>, Box<dyn FnOnce() -> Outcome + 'a>);

/// Sends the steps in order until one is not applied; when some went through before it, the
/// answer is [`Outcome::Partial`] by the names each covers.
pub(crate) fn in_order(steps: Vec<Step<'_>>) -> Outcome {
    let mut applied = Vec::new();
    let mut steps = steps.into_iter();
    while let Some((names, send)) = steps.next() {
        let outcome = send();
        if outcome == Outcome::Applied {
            applied.extend(names);
            continue;
        }
        if applied.is_empty() {
            return outcome;
        }
        return Outcome::Partial {
            applied,
            unapplied: names
                .into_iter()
                .chain(steps.flat_map(|(names, _)| names))
                .collect(),
            failure: Box::new(outcome),
        };
    }
    Outcome::Applied
}

/// Whether two URLs are one origin: scheme, host and port. A token goes nowhere else, so a
/// redirect to another port of the same host never carries it.
pub(crate) fn same_origin(left: &url::Url, right: &url::Url) -> bool {
    left.scheme() == right.scheme()
        && left.host_str() == right.host_str()
        && left.port_or_known_default() == right.port_or_known_default()
}

/// Media on a host's own server, read with `authorization` as its Authorization header. A
/// redirect may lead to object storage that authorizes with its own signature, which never
/// sees the credential. `host` names the host in error descriptions.
pub(crate) fn media(
    agent: &ureq::Agent,
    url: &url::Url,
    authorization: Option<&str>,
    validator: Option<&str>,
    host: &str,
) -> Result<PullRequestMedia, ForgeError> {
    use crate::github::media;
    let error = |kind, what: &str| ForgeError {
        kind,
        description: format!("{host} media {what}"),
    };
    let failed = || error(ForgeErrorKind::Uncertain, "unreadable");
    let started = Instant::now();
    let mut target = url.clone();
    let mut response = None;
    for _ in 0..=3 {
        let remaining = DEADLINE.saturating_sub(started.elapsed());
        let mut request = agent
            .get(target.as_str())
            .timeout(remaining)
            .set("User-Agent", "tcode")
            .set("Accept-Encoding", "identity");
        if let Some(authorization) = authorization.filter(|_| same_origin(&target, url)) {
            request = request.set("Authorization", authorization);
        }
        if let Some(validator) = validator {
            let header = if validator.starts_with('"') || validator.starts_with("W/") {
                "If-None-Match"
            } else {
                "If-Modified-Since"
            };
            request = request.set(header, validator);
        }
        let answer = match request.call() {
            Ok(answer) => answer,
            Err(ureq::Error::Status(404, _)) => {
                return Err(error(ForgeErrorKind::NotFound, "not found"));
            }
            Err(_) => return Err(failed()),
        };
        if (300..400).contains(&answer.status()) && answer.status() != 304 {
            target = answer
                .header("location")
                .and_then(|location| target.join(location).ok())
                .filter(|next| next.scheme() == "https")
                .ok_or_else(failed)?;
            continue;
        }
        response = Some(answer);
        break;
    }
    let response = response.ok_or_else(failed)?;
    let expires_at = (SystemTime::now() + Duration::from_secs(3600))
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    if response.status() == 304 {
        return Ok(PullRequestMedia::NotModified { expires_at });
    }
    let mime = response
        .header("content-type")
        .and_then(media::media_type)
        .or_else(|| media::type_from_name(&target).map(str::to_owned))
        .or_else(|| media::type_from_name(url).map(str::to_owned))
        .ok_or_else(|| error(ForgeErrorKind::UnsupportedMedia, "is not media"))?;
    if !mime.starts_with("image/") {
        return Ok(PullRequestMedia::External { mime });
    }
    let validator = response
        .header("etag")
        .or_else(|| response.header("last-modified"))
        .map(str::to_owned);
    let mut bytes = Vec::new();
    response
        .into_reader()
        .take(MAX_PULL_REQUEST_MEDIA_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| failed())?;
    if bytes.len() > MAX_PULL_REQUEST_MEDIA_BYTES {
        return Err(error(ForgeErrorKind::TooLarge, "too large"));
    }
    media::bounded(&mime, &bytes)?;
    Ok(PullRequestMedia::Image {
        bytes,
        mime,
        validator,
        expires_at,
    })
}

/// A host CLI's output, given `input` on stdin, in the launch environment without `remove`,
/// which the CLI would prefer over its stored login. `None` when it does not answer within ten
/// seconds, or, with `success`, when it fails.
pub(crate) fn run_cli(
    program: PathBuf,
    args: &[&str],
    input: Option<&str>,
    environment: &BTreeMap<String, String>,
    remove: &[&str],
    success: bool,
) -> Option<String> {
    use smol::io::{AsyncReadExt as _, AsyncWriteExt as _};
    let mut command = crate::process::async_command(program);
    command.args(args).env_clear().envs(environment);
    for name in remove {
        command.env_remove(name);
    }
    command
        .stdin(if input.is_some() {
            std::process::Stdio::piped()
        } else {
            std::process::Stdio::null()
        })
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);
    let mut child = command.spawn().ok()?;
    let stdin = child.stdin.take();
    let stdout = child.stdout.take()?;
    futures_lite::future::block_on(smol::future::race(
        async {
            if let (Some(mut stdin), Some(input)) = (stdin, input) {
                stdin.write_all(input.as_bytes()).await.ok()?;
                drop(stdin);
            }
            let mut bytes = Vec::new();
            stdout
                .take(1024 * 1024)
                .read_to_end(&mut bytes)
                .await
                .ok()?;
            let status = child.status().await.ok()?;
            (status.success() || !success).then_some(())?;
            String::from_utf8(bytes).ok()
        },
        async {
            smol::Timer::after(Duration::from_secs(10)).await;
            None
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::same_origin;

    /// An upload's redirect keeps the token only on the server's own origin.
    #[test]
    fn a_redirect_off_the_servers_origin_drops_the_token() {
        let url = |value: &str| url::Url::parse(value).unwrap();
        let upload = url("https://git.acme.test/attachments/1");
        assert!(same_origin(
            &upload,
            &url("https://git.acme.test:443/attachments/2")
        ));
        assert!(!same_origin(
            &upload,
            &url("https://git.acme.test:8443/attachments/1")
        ));
        assert!(!same_origin(
            &upload,
            &url("http://git.acme.test/attachments/1")
        ));
        assert!(!same_origin(
            &upload,
            &url("https://storage.acme.test/attachments/1")
        ));
    }
}
