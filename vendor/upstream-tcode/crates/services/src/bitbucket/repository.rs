//! Where a Bitbucket Cloud repository lives: a workspace and a repository slug on
//! bitbucket.org.

use super::api::HOST;
use crate::forge::Repository;
use tcode_core::pull_request::PullRequestKey;

fn component(value: &str) -> bool {
    !value.is_empty()
        && value != "."
        && value != ".."
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
}

/// `workspace/slug`, lowercased as keys hold it.
pub(super) fn repository(path: &[&str]) -> Option<Repository> {
    let [workspace, slug] = path else {
        return None;
    };
    let slug = slug.strip_suffix(".git").unwrap_or(slug);
    (component(workspace) && component(slug)).then(|| Repository {
        host: HOST.to_owned(),
        locator: format!("{workspace}/{slug}").to_ascii_lowercase(),
    })
}

pub(super) fn url(key: &PullRequestKey) -> String {
    format!(
        "https://{HOST}/{}/pull-requests/{}",
        key.repository, key.number
    )
}

/// A pull request's web URL, `https://bitbucket.org/workspace/slug/pull-requests/N[/…]`.
pub(super) fn pull_request_url(value: &str) -> Option<(PullRequestKey, String)> {
    let value = value.trim().split(['?', '#']).next()?;
    let rest = value
        .strip_prefix("https://")
        .or_else(|| value.strip_prefix("http://"))?
        .to_ascii_lowercase();
    let parts: Vec<_> = rest.split('/').collect();
    let [host, workspace, slug, "pull-requests", number, ..] = parts.as_slice() else {
        return None;
    };
    if *host != HOST {
        return None;
    }
    let number: u64 = number.parse().ok().filter(|number| *number > 0)?;
    let key = repository(&[workspace, slug])?.key(number);
    let url = url(&key);
    Some((key, url))
}

/// A repository named as `workspace/slug`, `bitbucket.org/workspace/slug`, or its web URL.
pub(super) fn selector(value: &str) -> Option<Repository> {
    let value = value.trim().trim_end_matches('/');
    let value = value
        .strip_prefix("https://")
        .or_else(|| value.strip_prefix("http://"))
        .unwrap_or(value);
    let parts: Vec<_> = value.split('/').collect();
    match parts.as_slice() {
        [host, rest @ ..] if host.eq_ignore_ascii_case(HOST) => repository(rest),
        _ => repository(&parts),
    }
}

/// The repository a remote URL names on bitbucket.org: HTTPS, with or without a user, or SSH
/// in either form.
pub(super) fn remote(value: &str) -> Option<Repository> {
    let value = value.trim();
    let (host, path) = if let Some(rest) = value
        .strip_prefix("https://")
        .or_else(|| value.strip_prefix("http://"))
        .or_else(|| value.strip_prefix("ssh://"))
    {
        let rest = rest.rsplit_once('@').map_or(rest, |(_, rest)| rest);
        let (host_port, path) = rest.split_once('/')?;
        (host_port.split(':').next()?, path)
    } else {
        let rest = value.rsplit_once('@').map_or(value, |(_, rest)| rest);
        rest.split_once(':')?
    };
    if !host.eq_ignore_ascii_case(HOST) {
        return None;
    }
    let segments: Vec<_> = path.split('/').filter(|part| !part.is_empty()).collect();
    repository(&segments)
}

pub(super) fn resolve(checkout: &crate::forge::Checkout<'_>) -> Option<Repository> {
    crate::forge::checkout::resolve(checkout, remote)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A pull request's URL names its repository on bitbucket.org whatever page of it is open;
    /// a remote names the same repository over HTTPS, with Bitbucket's user prefix, or SSH; no
    /// other host is Bitbucket's.
    #[test]
    fn urls_and_remotes_name_the_repository_on_bitbucket_org() {
        let (key, url) = pull_request_url(
            "https://bitbucket.org/Atlassian/ACE.js/pull-requests/547/diff#comment-1",
        )
        .unwrap();
        assert_eq!(key, PullRequestKey::new(HOST, "atlassian/ace.js", 547));
        assert_eq!(
            url,
            "https://bitbucket.org/atlassian/ace.js/pull-requests/547"
        );
        let project = repository(&["atlassian", "ace.js"]);
        for remote_url in [
            "https://tryanks@bitbucket.org/atlassian/ace.js.git",
            "https://bitbucket.org/atlassian/ace.js",
            "git@bitbucket.org:atlassian/ace.js.git",
            "ssh://git@bitbucket.org/atlassian/ace.js.git",
        ] {
            assert_eq!(remote(remote_url), project, "{remote_url}");
        }
        assert_eq!(selector("bitbucket.org/atlassian/ace.js"), project);
        assert_eq!(selector("atlassian/ace.js"), project);
        assert!(remote("git@bitbucket.acme.test:atlassian/ace.js.git").is_none());
        assert!(pull_request_url("https://bitbucket.acme.test/a/b/pull-requests/1").is_none());
        assert!(pull_request_url("https://bitbucket.org/a/b/pull-requests/0").is_none());
        assert!(pull_request_url("https://bitbucket.org/a/b/c/pull-requests/1").is_none());
    }
}
