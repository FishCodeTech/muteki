//! Where an Azure DevOps repository lives: a project and a repository of an organization, which
//! is the host, `dev.azure.com/{organization}`. Keys hold the names decoded, as `project/repository`.

use crate::forge::Repository;
use tcode_core::pull_request::{AZURE_HOST, HostKind, PullRequestKey, azure_pull_request};

/// A percent-encoded name as Azure names it, decoded; `None` for one that is no name.
fn name(raw: &str) -> Option<String> {
    let bytes = raw.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let hex = std::str::from_utf8(bytes.get(index + 1..index + 3)?).ok()?;
            decoded.push(u8::from_str_radix(hex, 16).ok()?);
            index += 3;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }
    let name = String::from_utf8(decoded).ok()?;
    (!name.trim().is_empty()
        && name != "."
        && name != ".."
        && !name.contains(['/', '\\'])
        && !name.chars().any(char::is_control))
    .then_some(name)
}

/// The repository of `organization`, named by its project and its own name.
pub(super) fn repository(
    organization: &str,
    project: &str,
    repository: &str,
) -> Option<Repository> {
    let host = HostKind::Azure.authority(organization).ok()?;
    Some(Repository {
        host,
        locator: format!("{}/{}", name(project)?, name(repository)?).to_ascii_lowercase(),
    })
}

/// The organization's name in a key's host.
pub(super) fn organization(key: &PullRequestKey) -> &str {
    key.host
        .strip_prefix(AZURE_HOST)
        .and_then(|rest| rest.strip_prefix('/'))
        .unwrap_or_default()
}

/// The project and the repository a key locates.
pub(super) fn names(key: &PullRequestKey) -> (&str, &str) {
    key.repository.split_once('/').unwrap_or_default()
}

pub(super) fn url(key: &PullRequestKey) -> String {
    let (project, repository) = names(key);
    let encode = crate::github::pull_request_reads::percent_encode;
    format!(
        "https://{}/{}/_git/{}/pullrequest/{}",
        key.host,
        encode(project),
        encode(repository),
        key.number
    )
}

/// A pull request's web URL on dev.azure.com, or on an organization's older visualstudio.com
/// host, which reads as the same organization.
pub(super) fn pull_request_url(value: &str) -> Option<(PullRequestKey, String)> {
    let (organization, project, name, number) = azure_pull_request(value)?;
    let key = repository(&organization, &project, &name)?.key(number);
    let url = url(&key);
    Some((key, url))
}

/// The repository a remote URL names: HTTPS on dev.azure.com or visualstudio.com, with or
/// without a user, or SSH in either form on either host.
pub(super) fn remote(value: &str) -> Option<Repository> {
    let value = value.trim();
    let (host, path) = match value.split_once("://") {
        Some((_, rest)) => {
            let (authority, path) = rest.split_once('/')?;
            let host = authority
                .rsplit_once('@')
                .map_or(authority, |(_, host)| host);
            (host.split(':').next()?, path)
        }
        None => {
            let rest = value.rsplit_once('@').map_or(value, |(_, rest)| rest);
            rest.split_once(':')?
        }
    };
    let host = host.to_ascii_lowercase();
    let parts: Vec<_> = path.split('/').filter(|part| !part.is_empty()).collect();
    if host == "ssh.dev.azure.com" || host == "vs-ssh.visualstudio.com" {
        let [version, organization, project, name] = parts.as_slice() else {
            return None;
        };
        return version
            .eq_ignore_ascii_case("v3")
            .then(|| repository(organization, project, name))?;
    }
    let (organization, path) = if host == AZURE_HOST {
        let (organization, path) = parts.split_first()?;
        (*organization, path)
    } else {
        let organization = host.strip_suffix(".visualstudio.com")?;
        let path = match parts.as_slice() {
            [collection, path @ ..] if collection.eq_ignore_ascii_case("defaultcollection") => path,
            path => path,
        };
        (organization, path)
    };
    match path {
        [project, "_git", name] => repository(organization, project, name),
        ["_git", name] => repository(organization, name, name),
        _ => None,
    }
}

/// A repository named as `project/repository` on `host`, or by its web or remote URL.
pub(super) fn selector(value: &str, host: &str) -> Option<Repository> {
    let value = value.trim().trim_end_matches('/');
    let url = if value.contains("://") {
        value.to_owned()
    } else {
        format!("https://{value}")
    };
    if let Some(found) = remote(&url) {
        return Some(found);
    }
    let (project, name) = value.split_once('/')?;
    let organization = HostKind::Azure.authority(host).ok()?;
    repository(
        organization.strip_prefix(&format!("{AZURE_HOST}/"))?,
        project,
        name,
    )
}

pub(super) fn resolve(checkout: &crate::forge::Checkout<'_>) -> Option<Repository> {
    crate::forge::checkout::resolve(checkout, remote)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A pull request's URL names its organization, project and repository on dev.azure.com,
    /// whatever page of it is open and on the older visualstudio.com host alike; a remote names
    /// the same repository over HTTPS with Azure's user prefix, or SSH, on either host.
    #[test]
    fn urls_and_remotes_name_the_repository_of_the_organization() {
        let (key, url) = pull_request_url(
            "https://dev.azure.com/Fabrikam/Fiber%20Optics/_git/Web/pullrequest/42?_a=files",
        )
        .unwrap();
        assert_eq!(
            key,
            PullRequestKey::new("dev.azure.com/fabrikam", "fiber optics/web", 42)
        );
        assert_eq!(
            url,
            "https://dev.azure.com/fabrikam/fiber%20optics/_git/web/pullrequest/42"
        );
        assert_eq!(
            pull_request_url(
                "https://fabrikam.visualstudio.com/DefaultCollection/Fiber%20Optics/_git/Web/pullrequest/42"
            )
            .map(|(key, _)| key),
            Some(key.clone())
        );
        let web = repository("fabrikam", "Fiber%20Optics", "Web");
        for remote_url in [
            "https://fabrikam@dev.azure.com/fabrikam/Fiber%20Optics/_git/Web",
            "https://dev.azure.com/fabrikam/Fiber%20Optics/_git/Web",
            "https://fabrikam.visualstudio.com/Fiber%20Optics/_git/Web",
            "git@ssh.dev.azure.com:v3/fabrikam/Fiber%20Optics/Web",
            "ssh://git@ssh.dev.azure.com/v3/fabrikam/Fiber%20Optics/Web",
            "fabrikam@vs-ssh.visualstudio.com:v3/fabrikam/Fiber%20Optics/Web",
        ] {
            assert_eq!(remote(remote_url), web, "{remote_url}");
        }
        // A repository named like its project may leave the project out.
        assert_eq!(
            remote("https://dev.azure.com/fabrikam/_git/Web"),
            repository("fabrikam", "web", "web")
        );
        assert_eq!(selector("Fiber Optics/Web", "dev.azure.com/fabrikam"), web);
        assert!(remote("git@github.com:fabrikam/web.git").is_none());
        assert!(
            pull_request_url("https://dev.azure.com/fabrikam/p/_git/w/pullrequest/0").is_none()
        );
        assert!(pull_request_url("https://dev.azure.com/fabrikam/p/_git/w/commit/1").is_none());
    }
}
