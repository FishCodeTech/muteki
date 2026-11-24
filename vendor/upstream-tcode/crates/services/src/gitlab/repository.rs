//! Where a GitLab project lives: its server's authority, which may carry a port, and the
//! project's full path below it, subgroups included.

use crate::forge::Repository;
use tcode_core::pull_request::{HostKind, PullRequestKey, dns_name, host_name};

fn component(value: &str) -> bool {
    !value.is_empty()
        && value != "."
        && value != ".."
        && !value.starts_with('-')
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
}

/// A project's full path below `authority`, lowercased as keys hold it: a group, any
/// subgroups, then the project.
pub(super) fn repository(authority: &str, path: &[&str]) -> Option<Repository> {
    let (name, groups) = path.split_last()?;
    let name = name.strip_suffix(".git").unwrap_or(name);
    (!groups.is_empty() && groups.iter().all(|group| component(group)) && component(name)).then(
        || Repository {
            host: authority.to_owned(),
            locator: format!("{}/{name}", groups.join("/")).to_ascii_lowercase(),
        },
    )
}

pub(super) fn url(key: &PullRequestKey) -> String {
    format!(
        "https://{}/{}/-/merge_requests/{}",
        key.host, key.repository, key.number
    )
}

/// The project's path as the API names a project: URL-encoded whole.
pub(super) fn project_id(locator: &str) -> String {
    locator.replace('/', "%2F")
}

/// `host_port` when it is a GitLab server: one in `known`, or one whose name says GitLab. A
/// host and port that is not one, such as one with userinfo, names no server.
fn authority_of(known: &[String], host_port: &str) -> Option<String> {
    if HostKind::Gitlab.authority(host_port).ok().as_deref() != Some(host_port) {
        return None;
    }
    (known.iter().any(|authority| authority == host_port)
        || HostKind::detect(host_port) == Some(HostKind::Gitlab))
    .then(|| host_port.to_owned())
}

/// A merge request's web URL, `https://host[:port]/group[/sub…]/project[/-]/merge_requests/N
/// [/…]`, on a server in `known` or one whose name says GitLab.
pub(super) fn pull_request_url(known: &[String], value: &str) -> Option<(PullRequestKey, String)> {
    let value = value.trim().split(['?', '#']).next()?;
    let rest = value
        .strip_prefix("https://")
        .or_else(|| value.strip_prefix("http://"))?
        .to_ascii_lowercase();
    let mut parts = rest.split('/');
    let host_port = parts.next()?.to_owned();
    let path: Vec<_> = parts.collect();
    let authority = authority_of(known, &host_port)?;
    let at = path.iter().position(|part| *part == "merge_requests")?;
    let project = match path[..at].split_last() {
        Some((&"-", project)) => project,
        _ => &path[..at],
    };
    let number: u64 = path
        .get(at + 1)?
        .parse()
        .ok()
        .filter(|number| *number > 0)?;
    let key = repository(&authority, project)?.key(number);
    let url = url(&key);
    Some((key, url))
}

/// A project named by its full path on `host`, `authority/full/path`, or its web URL.
pub(super) fn selector(known: &[String], value: &str, host: &str) -> Option<Repository> {
    let value = value.trim().trim_end_matches('/');
    let value = value
        .strip_prefix("https://")
        .or_else(|| value.strip_prefix("http://"))
        .unwrap_or(value);
    let parts: Vec<_> = value.split('/').collect();
    let (first, rest) = parts.split_first()?;
    let first = first.to_ascii_lowercase();
    if rest.len() >= 2
        && let Some(authority) = authority_of(known, &first)
            .or_else(|| (first == host.trim().to_ascii_lowercase()).then(|| first.clone()))
    {
        return repository(&authority, rest);
    }
    repository(&HostKind::Gitlab.authority(host).ok()?, &parts)
}

/// The project a remote URL names: HTTP(S) with any port, or SSH, whose port is the SSH
/// server's and so says nothing of the web authority.
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
        return repository(&authority_of(known, &host_port)?, &path);
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
    if !dns_name(&host) {
        return None;
    }
    let segments: Vec<_> = path.split('/').filter(|part| !part.is_empty()).collect();
    let named: Vec<_> = known
        .iter()
        .filter(|authority| host_name(authority) == host)
        .collect();
    let authority = match named.as_slice() {
        [authority] => (*authority).clone(),
        [] => (HostKind::detect(&host) == Some(HostKind::Gitlab)).then(|| host.clone())?,
        _ => return None,
    };
    repository(&authority, &segments)
}

/// The checkout's project: `upstream`, then `origin`, then the first remote here.
pub(super) fn resolve(
    known: &[String],
    checkout: &crate::forge::Checkout<'_>,
) -> Option<Repository> {
    crate::forge::checkout::resolve(checkout, |url| remote(known, url))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A merge request's URL keeps its server's port and the project's subgroups, with or
    /// without GitLab's `/-/` separator; a remote names the same project over HTTPS or SSH,
    /// whose port is the SSH server's; an unnamed server is GitLab's only when settings say.
    #[test]
    fn urls_and_remotes_find_the_project_by_port_and_subgroups() {
        let known = vec!["code.acme.test:8443".to_owned()];
        let project = repository("code.acme.test:8443", &["team", "apps", "web"]);
        let (key, url) = pull_request_url(
            &known,
            "https://code.acme.test:8443/Team/Apps/Web/-/merge_requests/7/diffs?commit_id=1",
        )
        .unwrap();
        assert_eq!(
            key,
            PullRequestKey::new("code.acme.test:8443", "team/apps/web", 7)
        );
        assert_eq!(
            url,
            "https://code.acme.test:8443/team/apps/web/-/merge_requests/7"
        );
        assert_eq!(
            pull_request_url(
                &known,
                "https://code.acme.test:8443/team/apps/web/merge_requests/7"
            )
            .map(|(key, _)| key),
            Some(key.clone())
        );
        assert_eq!(
            remote(&known, "https://code.acme.test:8443/team/apps/web.git"),
            project
        );
        assert_eq!(
            remote(&known, "ssh://git@code.acme.test:2222/team/apps/web.git"),
            project
        );
        assert_eq!(
            remote(&known, "git@code.acme.test:team/apps/web.git"),
            project
        );
        assert_eq!(
            pull_request_url(
                &[],
                "https://gitlab.com/gitlab-org/cli/-/merge_requests/4049"
            )
            .unwrap()
            .0,
            PullRequestKey::new("gitlab.com", "gitlab-org/cli", 4049)
        );
        assert!(pull_request_url(&[], "https://code.acme.test/a/b/-/merge_requests/1").is_none());
        assert!(
            pull_request_url(
                &[],
                "https://gitlab.com@127.0.0.1:8080/a/b/-/merge_requests/1"
            )
            .is_none()
        );
        assert!(pull_request_url(&[], "https://gitlab.com/b/-/merge_requests/1").is_none());
        assert_eq!(project_id("team/apps/web"), "team%2Fapps%2Fweb");
    }
}
