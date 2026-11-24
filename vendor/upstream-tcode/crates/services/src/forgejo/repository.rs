//! Where a Forgejo or Gitea repository lives: its server's authority, which may carry a port and
//! a mount path, and `owner/name` below it.

use crate::forge::Repository;
use tcode_core::pull_request::{HostKind, PullRequestKey};

fn component(value: &str) -> bool {
    !value.is_empty()
        && value != "."
        && value != ".."
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
}

/// `owner/name` below `authority`, lowercased as keys hold it.
pub(super) fn repository(authority: &str, owner: &str, name: &str) -> Option<Repository> {
    let name = name.strip_suffix(".git").unwrap_or(name);
    (component(owner) && component(name)).then(|| Repository {
        host: authority.to_owned(),
        locator: format!("{owner}/{name}").to_ascii_lowercase(),
    })
}

pub(super) fn url(key: &PullRequestKey) -> String {
    format!(
        "https://{}/{}/pulls/{}",
        key.host, key.repository, key.number
    )
}

/// The authority among `known` that a URL's host, port and path start with, longest mount
/// first; else the host and port alone when its name says Forgejo or Gitea. A host and port
/// that is not one, such as one with userinfo, names no server.
fn authority_of(known: &[String], host_port: &str, path: &[&str]) -> Option<(String, usize)> {
    if HostKind::Gitea.authority(host_port).ok().as_deref() != Some(host_port) {
        return None;
    }
    let mut best: Option<(String, usize)> = None;
    for authority in known {
        let (known_host, mount) = authority.split_once('/').unwrap_or((authority, ""));
        if known_host != host_port {
            continue;
        }
        let mount: Vec<_> = mount.split('/').filter(|part| !part.is_empty()).collect();
        if path.len() >= mount.len()
            && path[..mount.len()]
                .iter()
                .zip(&mount)
                .all(|(part, mount)| part.eq_ignore_ascii_case(mount))
            && best.as_ref().is_none_or(|(_, depth)| mount.len() > *depth)
        {
            best = Some((authority.clone(), mount.len()));
        }
    }
    best.or_else(|| {
        HostKind::detect(host_port)
            .filter(|kind| *kind != HostKind::Github)
            .map(|_| (host_port.to_owned(), 0))
    })
}

/// A pull request's web URL, `https://host[:port][/mount]/owner/name/pulls/N[/…]`, on a server
/// in `known` or one whose name says Forgejo or Gitea.
pub(super) fn pull_request_url(known: &[String], value: &str) -> Option<(PullRequestKey, String)> {
    let value = value.trim().split(['?', '#']).next()?;
    let rest = value
        .strip_prefix("https://")
        .or_else(|| value.strip_prefix("http://"))?
        .to_ascii_lowercase();
    let mut parts = rest.split('/');
    let host_port = parts.next()?.to_owned();
    let path: Vec<_> = parts.collect();
    let (authority, depth) = authority_of(known, &host_port, &path)?;
    let [owner, name, "pulls", number, ..] = &path[depth..] else {
        return None;
    };
    let number: u64 = number.parse().ok().filter(|number| *number > 0)?;
    let key = repository(&authority, owner, name)?.key(number);
    let url = url(&key);
    Some((key, url))
}

/// A repository named `owner/name`, `authority/owner/name` or by its web URL.
pub(super) fn selector(known: &[String], value: &str, host: &str) -> Option<Repository> {
    let value = value.trim().trim_end_matches('/');
    let value = value
        .strip_prefix("https://")
        .or_else(|| value.strip_prefix("http://"))
        .unwrap_or(value);
    let parts: Vec<_> = value.split('/').collect();
    match parts.as_slice() {
        [owner, name] => repository(&HostKind::Gitea.authority(host).ok()?, owner, name),
        [host_port, path @ ..] if path.len() >= 2 => {
            let (authority, depth) = authority_of(known, &host_port.to_ascii_lowercase(), path)?;
            let [owner, name] = &path[depth..] else {
                return None;
            };
            repository(&authority, owner, name)
        }
        _ => None,
    }
}

/// The repository a remote URL names: HTTP(S) with any port and mount, or SSH, whose port is
/// the SSH server's and so says nothing of the web authority.
pub(super) fn remote(known: &[String], value: &str) -> Option<Repository> {
    let value = value.trim();
    if let Some(rest) = value
        .strip_prefix("https://")
        .or_else(|| value.strip_prefix("http://"))
    {
        let rest = rest.rsplit_once('@').map_or(rest, |(_, rest)| rest);
        let mut parts = rest.split('/');
        let host_port = parts.next()?.to_ascii_lowercase();
        let path: Vec<_> = parts.filter(|part| !part.is_empty()).collect();
        let (authority, depth) = authority_of(known, &host_port, &path)?;
        let [owner, name] = &path[depth..] else {
            return None;
        };
        return repository(&authority, owner, name);
    }
    let rest = value.strip_prefix("ssh://").unwrap_or(value);
    let rest = rest.rsplit_once('@').map_or(rest, |(_, rest)| rest);
    let (host, path) = if value.starts_with("ssh://") {
        let (host_port, path) = rest.split_once('/')?;
        (host_port.split(':').next()?, path)
    } else {
        rest.split_once(':')?
    };
    let host = host.to_ascii_lowercase();
    if !tcode_core::pull_request::dns_name(&host) {
        return None;
    }
    let segments: Vec<_> = path.split('/').filter(|part| !part.is_empty()).collect();
    let [.., owner, name] = segments.as_slice() else {
        return None;
    };
    let named: Vec<_> = known
        .iter()
        .filter(|authority| tcode_core::pull_request::host_name(authority) == host)
        .collect();
    let authority = match named.as_slice() {
        [authority] => (*authority).clone(),
        [] => HostKind::detect(&host)
            .filter(|kind| *kind != HostKind::Github)
            .map(|_| host.clone())?,
        _ => return None,
    };
    repository(&authority, owner, name)
}

/// The checkout's repository: `upstream`, then `origin`, then the first remote here.
pub(super) fn resolve(
    known: &[String],
    checkout: &crate::forge::Checkout<'_>,
) -> Option<Repository> {
    crate::forge::checkout::resolve(checkout, |url| remote(known, url))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A server with a port and a mount keeps both in its authority, and the mount is not
    /// read as the owner; an SSH remote of that server finds it by name alone.
    #[test]
    fn urls_and_remotes_find_the_server_by_port_and_mount() {
        let known = vec!["git.acme.test:3000/forge".to_owned()];
        let (key, url) = pull_request_url(
            &known,
            "https://git.acme.test:3000/forge/Team/App/pulls/7/files?style=split",
        )
        .unwrap();
        assert_eq!(
            key,
            PullRequestKey::new("git.acme.test:3000/forge", "team/app", 7)
        );
        assert_eq!(url, "https://git.acme.test:3000/forge/team/app/pulls/7");
        assert_eq!(
            remote(&known, "https://git.acme.test:3000/forge/team/app.git"),
            repository("git.acme.test:3000/forge", "team", "app")
        );
        assert_eq!(
            remote(&known, "ssh://git@git.acme.test:2222/team/app.git"),
            repository("git.acme.test:3000/forge", "team", "app")
        );
        assert_eq!(
            pull_request_url(&[], "https://codeberg.org/forgejo/forgejo/pulls/9")
                .unwrap()
                .0,
            PullRequestKey::new("codeberg.org", "forgejo/forgejo", 9)
        );
        assert!(pull_request_url(&[], "https://git.example.com/a/b/pulls/9").is_none());
        assert!(pull_request_url(&known, "https://git.acme.test:3000/forge/a/b/pull/9").is_none());
    }

    /// A URL's userinfo or a port that is not one must not turn another server into a Forgejo
    /// or Gitea host Tcode sends requests to.
    #[test]
    fn a_url_naming_another_server_after_userinfo_is_refused() {
        for url in [
            "https://gitea.x.com@127.0.0.1:8080/a/b/pulls/1",
            "https://codeberg.org:443@evil.com/a/b/pulls/1",
        ] {
            assert!(pull_request_url(&[], url).is_none(), "{url}");
            let rest = url.trim_start_matches("https://");
            assert!(
                selector(&[], rest.trim_end_matches("/pulls/1"), "").is_none(),
                "{url}"
            );
        }
    }
}
