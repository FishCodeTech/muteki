use std::path::Path;
use tcode_core::pull_request::PullRequestKey;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Repository {
    pub host: String,
    pub owner: String,
    pub name: String,
}

impl Repository {
    pub fn key(&self, number: u64) -> PullRequestKey {
        PullRequestKey::new(&self.host, &format!("{}/{}", self.owner, self.name), number)
    }
    pub fn from_key(key: &PullRequestKey) -> Option<Self> {
        let repository = selector(&key.repository, &key.host)?;
        (repository.host == key.host).then_some(repository)
    }
    pub fn url(&self, number: u64) -> String {
        format!(
            "https://{}/{}/{}/pull/{number}",
            self.host, self.owner, self.name
        )
    }
}

fn component(value: &str) -> bool {
    !value.is_empty()
        && value != "."
        && value != ".."
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
}

pub fn selector(value: &str, default_host: &str) -> Option<Repository> {
    let value = value.trim().trim_end_matches('/');
    let value = value
        .strip_prefix("https://")
        .or_else(|| value.strip_prefix("http://"))
        .unwrap_or(value);
    let value = value.strip_suffix(".git").unwrap_or(value);
    let parts: Vec<_> = value.split('/').collect();
    let (host, owner, name) = match parts.as_slice() {
        [owner, name] => (default_host, *owner, *name),
        [host, owner, name] => (*host, *owner, *name),
        _ => return None,
    };
    Some(Repository {
        host: super::normalize_host(host).ok()?,
        owner: component(owner).then(|| owner.to_ascii_lowercase())?,
        name: component(name).then(|| name.to_ascii_lowercase())?,
    })
}

pub fn pull_request_url(value: &str) -> Option<(PullRequestKey, String)> {
    let value = value.trim().split(['?', '#']).next()?;
    let rest = value
        .strip_prefix("https://")
        .or_else(|| value.strip_prefix("http://"))?;
    let parts: Vec<_> = rest.split('/').collect();
    let [host, owner, name, "pull", number, ..] = parts.as_slice() else {
        return None;
    };
    let number: u64 = number.parse().ok()?;
    if number == 0 {
        return None;
    }
    let repository = selector(&format!("{host}/{owner}/{name}"), host)?;
    Some((repository.key(number), repository.url(number)))
}

fn remote(value: &str) -> Option<(Repository, String)> {
    let ssh = value.starts_with("ssh://") || value.starts_with("git@");
    let value = value
        .strip_prefix("ssh://")
        .or_else(|| value.strip_prefix("https://"))
        .or_else(|| value.strip_prefix("http://"))
        .unwrap_or(value);
    let value = value.rsplit_once('@').map_or(value, |(_, rest)| rest);
    let value = if value
        .split_once('/')
        .is_some_and(|(host, _)| host.ends_with(":443") || host.ends_with(":22"))
    {
        value.replacen(":443", "", 1).replacen(":22", "", 1)
    } else {
        value.replacen(':', "/", 1)
    };
    let (host, path) = value.split_once('/')?;
    let literal_host = host.to_ascii_lowercase();
    let host = if ssh && !host.contains('.') && host.to_ascii_lowercase().contains("github") {
        "github.com"
    } else {
        host
    };
    if !host
        .to_ascii_lowercase()
        .split('.')
        .any(|label| label == "github")
    {
        return None;
    }
    Some((selector(&format!("{host}/{path}"), host)?, literal_host))
}

pub fn git_read(cwd: &Path, args: &[&str]) -> Option<String> {
    let output = crate::process::command("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn remotes(checkout: &crate::forge::Checkout<'_>) -> Vec<(String, Repository)> {
    crate::forge::checkout::remotes(checkout, |url| Some(remote(url)?.0))
}

pub fn resolve(checkout: &crate::forge::Checkout<'_>) -> Option<Repository> {
    let default_host = std::env::var("GH_HOST").unwrap_or_else(|_| "github.com".into());
    if let Ok(value) = std::env::var("GH_REPO")
        && let Some(repository) = selector(&value, &default_host)
    {
        return Some(repository);
    }
    let fetch = checkout.remotes();
    let remotes = remotes(checkout);
    if remotes.is_empty() {
        return None;
    }
    let marks = git_read(
        checkout.cwd(),
        &["config", "--get-regexp", "^remote\\..*\\.gh-resolved$"],
    )
    .unwrap_or_default();
    let marked: Vec<_> = marks
        .lines()
        .filter_map(|line| {
            let (key, value) = line.split_once(char::is_whitespace)?;
            let name = key.strip_prefix("remote.")?.strip_suffix(".gh-resolved")?;
            let (_, repository) = remotes.iter().find(|(n, _)| n == name)?;
            if value.trim() == "base" {
                Some(repository.clone())
            } else {
                selector(value, &repository.host)
            }
        })
        .collect();
    let host = remotes
        .iter()
        .find(|(name, _)| name == "origin")
        .or_else(|| remotes.first())
        .map(|(_, repository)| repository.host.as_str())
        .unwrap_or(&default_host);
    if marked.len() == 1
        && fetch
            .iter()
            .all(|(_, url)| remote(url).is_some_and(|(_, literal_host)| literal_host == host))
    {
        return marked.into_iter().next();
    }
    remotes
        .iter()
        .filter(|(_, repository)| repository.host == host)
        .min_by_key(|(name, _)| {
            ["upstream", "github", "origin"]
                .iter()
                .position(|rank| name.eq_ignore_ascii_case(rank))
                .unwrap_or(99)
        })
        .map(|(_, repository)| repository.clone())
}
