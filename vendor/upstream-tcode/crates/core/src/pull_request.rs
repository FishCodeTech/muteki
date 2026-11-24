use serde::{Deserialize, Serialize};

/// GitHub's documentation of native stacks, which the instructions and the stack map both link.
pub const STACKS_DOCS_URL: &str = "https://docs.github.com/en/pull-requests/collaborating-with-pull-requests/working-with-stacked-pull-requests";

/// What Tcode's own text says about a pull request host: everything host-specific that the
/// model reads, and the link menu's hint. Clients see these without asking the host, so each
/// host Tcode implements has its terms here, in [`HOSTS`].
#[derive(Debug)]
pub struct HostTerms {
    /// As the host names itself.
    pub name: &'static str,
    /// The host's CLIs, which create pull requests without linking them to a thread.
    pub clis: &'static str,
    /// The host's native stacks as the instructions name them, and their documentation.
    pub stacks: Option<(&'static str, &'static str)>,
    /// Each CLI's tool and noun before `merge` or `close` in a command that merges or closes a
    /// pull request.
    pub merge_commands: &'static [(&'static str, &'static str)],
    /// Whether a web URL reads as one of the host's pull requests: a menu hint only, since the
    /// host validates and canonicalizes the target.
    pub pull_request_url: fn(&str) -> bool,
    /// Whether a person reads the host by its own name rather than the product's: one product
    /// name would mislabel a server that runs a sibling of it.
    pub named_by_host: bool,
    /// The host's monochrome mark among the UI's icon assets.
    pub mark: &'static str,
    /// The pull request's own pages for its conflicts and its checks, below its URL, where the
    /// host has them.
    pub conflicts_page: Option<&'static str>,
    pub checks_page: Option<&'static str>,
}

impl HostTerms {
    /// The host as a person reads it, for a host at `authority`.
    pub fn display_name(&self, authority: &str) -> String {
        if self.named_by_host {
            host_name(authority).to_owned()
        } else {
            self.name.to_owned()
        }
    }
}

/// The host name of an authority, without its port or mount path.
pub fn host_name(authority: &str) -> &str {
    let host = authority.split('/').next().unwrap_or(authority);
    host.split(':').next().unwrap_or(host)
}

pub const GITHUB: HostTerms = HostTerms {
    name: "GitHub",
    clis: "gh, gh stack",
    stacks: Some(("GitHub native stacks", STACKS_DOCS_URL)),
    merge_commands: &[("gh", "pr")],
    pull_request_url: |value| {
        let value = value.split(['?', '#']).next().unwrap_or_default();
        let Some(rest) = value
            .strip_prefix("https://")
            .or_else(|| value.strip_prefix("http://"))
        else {
            return false;
        };
        let parts: Vec<_> = rest.split('/').collect();
        let [host, owner, repository, "pull", number, ..] = parts.as_slice() else {
            return false;
        };
        !host.is_empty()
            && !owner.is_empty()
            && !repository.is_empty()
            && number.parse::<u64>().is_ok_and(|number| number > 0)
    },
    named_by_host: false,
    mark: "icons/github.svg",
    conflicts_page: Some("/conflicts"),
    checks_page: Some("/checks"),
};

pub const FORGEJO: HostTerms = HostTerms {
    name: "Forgejo",
    clis: "tea",
    stacks: None,
    merge_commands: &[("tea", "pr")],
    pull_request_url: forgejo_pull_request_url,
    named_by_host: true,
    mark: "icons/forgejo.svg",
    conflicts_page: None,
    checks_page: None,
};

/// Gitea and Forgejo share an API and a CLI, so they differ only in name.
pub const GITEA: HostTerms = HostTerms {
    name: "Gitea",
    ..FORGEJO
};

/// `https://host[:port][/mount]/owner/repository/pulls/N`, on any host.
fn forgejo_pull_request_url(value: &str) -> bool {
    let value = value.split(['?', '#']).next().unwrap_or_default();
    let Some(rest) = value
        .strip_prefix("https://")
        .or_else(|| value.strip_prefix("http://"))
    else {
        return false;
    };
    let parts: Vec<_> = rest.split('/').collect();
    parts.iter().enumerate().any(|(index, part)| {
        *part == "pulls"
            && index >= 3
            && !parts[0].is_empty()
            && !parts[index - 2].is_empty()
            && !parts[index - 1].is_empty()
            && parts
                .get(index + 1)
                .and_then(|number| number.parse::<u64>().ok())
                .is_some_and(|number| number > 0)
    })
}

pub const GITLAB: HostTerms = HostTerms {
    name: "GitLab",
    clis: "glab",
    stacks: None,
    merge_commands: &[("glab", "mr")],
    pull_request_url: gitlab_pull_request_url,
    named_by_host: false,
    mark: "icons/gitlab.svg",
    conflicts_page: None,
    checks_page: Some("/pipelines"),
};

/// `https://host[:port]/group[/subgroup…]/project[/-]/merge_requests/N`, on any host.
fn gitlab_pull_request_url(value: &str) -> bool {
    let value = value.split(['?', '#']).next().unwrap_or_default();
    let Some(rest) = value
        .strip_prefix("https://")
        .or_else(|| value.strip_prefix("http://"))
    else {
        return false;
    };
    let parts: Vec<_> = rest.split('/').collect();
    parts.iter().enumerate().any(|(index, part)| {
        let project = if index >= 1 && parts[index - 1] == "-" {
            index - 1
        } else {
            index
        };
        *part == "merge_requests"
            && project >= 3
            && !parts[0].is_empty()
            && parts[1..project].iter().all(|part| !part.is_empty())
            && parts
                .get(index + 1)
                .and_then(|number| number.parse::<u64>().ok())
                .is_some_and(|number| number > 0)
    })
}

pub const BITBUCKET: HostTerms = HostTerms {
    name: "Bitbucket",
    clis: "",
    stacks: None,
    merge_commands: &[],
    pull_request_url: bitbucket_pull_request_url,
    named_by_host: false,
    mark: "icons/bitbucket.svg",
    conflicts_page: None,
    checks_page: None,
};

/// `https://bitbucket.org/workspace/repository/pull-requests/N`.
fn bitbucket_pull_request_url(value: &str) -> bool {
    let value = value.split(['?', '#']).next().unwrap_or_default();
    let Some(rest) = value
        .strip_prefix("https://")
        .or_else(|| value.strip_prefix("http://"))
    else {
        return false;
    };
    let parts: Vec<_> = rest.split('/').collect();
    let [host, workspace, repository, "pull-requests", number, ..] = parts.as_slice() else {
        return false;
    };
    host.eq_ignore_ascii_case(HostKind::Bitbucket.public_host())
        && !workspace.is_empty()
        && !repository.is_empty()
        && number.parse::<u64>().is_ok_and(|number| number > 0)
}

pub const AZURE: HostTerms = HostTerms {
    name: "Azure DevOps",
    clis: "az repos",
    stacks: None,
    // `az repos pr` completes and abandons through `update --status`, with no merge or close
    // verb to match.
    merge_commands: &[],
    pull_request_url: |value| azure_pull_request(value).is_some(),
    named_by_host: false,
    mark: "icons/azure-devops.svg",
    conflicts_page: None,
    checks_page: None,
};

/// The organization, project, repository and number of an Azure DevOps pull request's web URL:
/// `https://dev.azure.com/org/project/_git/repository/pullrequest/N`, or the older
/// `https://org.visualstudio.com[/DefaultCollection]/project/_git/repository/pullrequest/N`. A
/// repository named like its project may leave the project out. Names stay percent-encoded.
pub fn azure_pull_request(value: &str) -> Option<(String, String, String, u64)> {
    let value = value.trim().split(['?', '#']).next()?;
    let rest = value
        .strip_prefix("https://")
        .or_else(|| value.strip_prefix("http://"))?;
    let parts: Vec<_> = rest.split('/').collect();
    let (organization, path) = match parts.as_slice() {
        [host, organization, path @ ..] if host.eq_ignore_ascii_case(AZURE_HOST) => {
            (*organization, path)
        }
        [host, path @ ..] => {
            let host = host.to_ascii_lowercase();
            let organization = host.strip_suffix(".visualstudio.com")?;
            let path = match path {
                [collection, path @ ..] if collection.eq_ignore_ascii_case("defaultcollection") => {
                    path
                }
                path => path,
            };
            (&rest[..organization.len()], path)
        }
        _ => return None,
    };
    let (project, repository, number) = match path {
        [project, "_git", repository, "pullrequest", number, ..] => (*project, *repository, number),
        ["_git", repository, "pullrequest", number, ..] => (*repository, *repository, number),
        _ => return None,
    };
    let number = number.parse::<u64>().ok().filter(|number| *number > 0)?;
    (azure_organization(organization).is_some() && !project.is_empty() && !repository.is_empty())
        .then(|| {
            (
                organization.to_ascii_lowercase(),
                project.to_owned(),
                repository.to_owned(),
                number,
            )
        })
}

/// Azure DevOps Services' one web host; each organization is a host below it.
pub const AZURE_HOST: &str = "dev.azure.com";

/// An Azure DevOps organization's name, lowercased: letters, digits and inner hyphens.
fn azure_organization(name: &str) -> Option<String> {
    (!name.is_empty() && name.len() <= 50 && dns_name(name) && !name.contains('.'))
        .then(|| name.to_ascii_lowercase())
}

/// Every host Tcode reads pull requests from.
pub const HOSTS: &[&HostTerms] = &[&GITHUB, &FORGEJO, &GITEA, &GITLAB, &BITBUCKET, &AZURE];

/// The software a source-control host runs, which decides how Tcode talks to it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HostKind {
    Github,
    Forgejo,
    Gitea,
    Gitlab,
    /// Bitbucket Cloud, which is bitbucket.org alone.
    Bitbucket,
    /// Azure DevOps Services, where each organization on dev.azure.com is a host.
    Azure,
}

/// Why an authority cannot be a host of a kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostRefusal {
    Blank,
    Invalid,
    /// A port or a path on a kind whose hosts are named by host name alone.
    PortOrPath,
    /// A path on a kind whose hosts take a port but no path.
    Path,
    /// Another host than the one a kind is served from.
    Fixed,
}

impl HostKind {
    pub const ALL: [Self; 6] = [
        Self::Github,
        Self::Forgejo,
        Self::Gitea,
        Self::Gitlab,
        Self::Bitbucket,
        Self::Azure,
    ];

    pub fn terms(self) -> &'static HostTerms {
        match self {
            Self::Github => &GITHUB,
            Self::Forgejo => &FORGEJO,
            Self::Gitea => &GITEA,
            Self::Gitlab => &GITLAB,
            Self::Bitbucket => &BITBUCKET,
            Self::Azure => &AZURE,
        }
    }

    /// The kind's public host, which needs no adding.
    pub fn public_host(self) -> &'static str {
        match self {
            Self::Github => "github.com",
            Self::Forgejo => "codeberg.org",
            Self::Gitea => "gitea.com",
            Self::Gitlab => "gitlab.com",
            Self::Bitbucket => "bitbucket.org",
            Self::Azure => AZURE_HOST,
        }
    }

    /// The one host a kind is served from, which is never typed.
    pub fn fixed_host(self) -> Option<&'static str> {
        (self == Self::Bitbucket).then(|| self.public_host())
    }

    /// Whether a saved token of the kind may come with the account's email, which it then
    /// authenticates with, or alone, as a bearer.
    pub fn takes_email(self) -> bool {
        self == Self::Bitbucket
    }

    /// Whether a host of the kind is one organization on the kind's public host, named by the
    /// organization alone.
    pub fn organizations(self) -> bool {
        self == Self::Azure
    }

    /// The kind a host's name says it runs: a public host, or a whole DNS label naming the
    /// software. A `bitbucket` label names Bitbucket Data Center, which Tcode does not read.
    pub fn detect(authority: &str) -> Option<Self> {
        let host = host_name(authority).to_ascii_lowercase();
        Self::ALL.into_iter().find(|kind| {
            host == kind.public_host()
                || host.split('.').any(|label| {
                    Some(label)
                        == match kind {
                            Self::Github => Some("github"),
                            Self::Forgejo => Some("forgejo"),
                            Self::Gitea => Some("gitea"),
                            Self::Gitlab => Some("gitlab"),
                            Self::Bitbucket | Self::Azure => None,
                        }
                })
        })
    }

    /// The kind a host is read as: the one `known` from settings, else the one its name says,
    /// else GitHub, as every host was before hosts had kinds.
    pub fn of(known: Option<Self>, host: &str) -> Self {
        known.or_else(|| Self::detect(host)).unwrap_or(Self::Github)
    }

    /// What an authority of the kind looks like, for one [`Self::authority`] refused.
    pub fn rule(self) -> String {
        match self {
            Self::Github => "Use a GitHub hostname without a URL or path.".into(),
            Self::Forgejo | Self::Gitea => format!(
                "Use a {} host name in lowercase, with its port and path if the server has them, without a URL scheme.",
                self.terms().name
            ),
            Self::Gitlab => "Use a GitLab host name in lowercase, with its port if the server has one, without a URL scheme or path.".into(),
            Self::Bitbucket => "Use bitbucket.org: Tcode reads Bitbucket Cloud only.".into(),
            Self::Azure => "Use dev.azure.com/ and the organization's name, in lowercase.".into(),
        }
    }

    /// An authority as settings keep it: lowercase, without a scheme or trailing slashes.
    /// Forgejo and Gitea servers take a port and a mount path, GitLab servers a port alone,
    /// Bitbucket is only its public host, and an Azure DevOps organization is
    /// `dev.azure.com/{organization}`, given as that, its name alone, or its older
    /// `{organization}.visualstudio.com`.
    pub fn authority(self, raw: &str) -> Result<String, HostRefusal> {
        let value = raw.trim().to_ascii_lowercase();
        let value = value
            .strip_prefix("https://")
            .or_else(|| value.strip_prefix("http://"))
            .unwrap_or(&value)
            .trim_end_matches('/');
        if value.is_empty() {
            return Err(HostRefusal::Blank);
        }
        if self == Self::Azure {
            let host = value.split('/').next().unwrap_or(value);
            let organization = if host == AZURE_HOST {
                value.split('/').nth(1).ok_or(HostRefusal::Blank)?
            } else if let Some(organization) = host.strip_suffix(".visualstudio.com") {
                organization
            } else {
                value
            };
            return azure_organization(organization)
                .map(|organization| format!("{AZURE_HOST}/{organization}"))
                .ok_or(HostRefusal::Invalid);
        }
        let (host_port, path) = value.split_once('/').unwrap_or((value, ""));
        let (host, port) = match host_port.split_once(':') {
            Some((host, port)) => (host, Some(port)),
            None => (host_port, None),
        };
        if !dns_name(host) {
            return Err(HostRefusal::Invalid);
        }
        if let Some(fixed) = self.fixed_host() {
            return if value == fixed {
                Ok(value.to_owned())
            } else {
                Err(HostRefusal::Fixed)
            };
        }
        if port.is_none() && path.is_empty() {
            return Ok(value.to_owned());
        }
        if self == Self::Github {
            return Err(HostRefusal::PortOrPath);
        }
        if self == Self::Gitlab && !path.is_empty() {
            return Err(HostRefusal::Path);
        }
        if port.is_some_and(|port| port.parse::<u16>().map_or(true, |port| port == 0))
            || (!path.is_empty()
                && !path.split('/').all(|segment| {
                    !segment.is_empty()
                        && segment != "."
                        && segment != ".."
                        && segment
                            .bytes()
                            .all(|b| b.is_ascii_alphanumeric() || b"-_.~".contains(&b))
                }))
        {
            return Err(HostRefusal::Invalid);
        }
        Ok(value.to_owned())
    }
}

/// A DNS host name: labels of ASCII letters, digits and inner hyphens.
pub fn dns_name(host: &str) -> bool {
    !host.is_empty()
        && host.len() <= 253
        && host.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        })
}

/// The hosts whose terms Tcode's text names: GitHub's, and those of every other kind in
/// `kinds`, in [`HostKind::ALL`] order.
pub fn hosts_in(kinds: impl IntoIterator<Item = HostKind>) -> Vec<&'static HostTerms> {
    let kinds: Vec<_> = kinds.into_iter().collect();
    HostKind::ALL
        .into_iter()
        .filter(|kind| *kind == HostKind::Github || kinds.contains(kind))
        .map(HostKind::terms)
        .collect()
}

const LINKING_OPEN: &str = "<pull_request_linking>\n";
const LINKING_CLOSE: &str = "\n</pull_request_linking>\n\n";

/// The hosts by name, as Tcode's text to the model names them together.
pub fn host_names(hosts: &[&HostTerms]) -> String {
    let names: Vec<_> = hosts.iter().map(|terms| terms.name).collect();
    names.join(" or ")
}

/// Given to the model with the pull request tools, through the provider's instruction channel.
pub fn linking_instructions(hosts: &[&HostTerms]) -> String {
    let mut clis: Vec<_> = Vec::new();
    for terms in hosts {
        if !terms.clis.is_empty() && !clis.contains(&terms.clis) {
            clis.push(terms.clis);
        }
    }
    let mut text = format!(
        "{LINKING_OPEN}When the tcode_pull_requests MCP server exposes link_pull_request, use it to register every pull request you create or work on for this thread. Call link_pull_request with the full PR URL immediately after creating a PR or starting work on an existing PR. For a stack, link every layer, not just the current branch or top PR. This applies to {}, other CLIs and host APIs: they do not register PRs with this thread. Linking an already-linked PR is safe. Before finishing PR work, call list_thread_pull_requests and link anything missing. Do not link unrelated PRs mentioned only as background. If linking fails, report that failure instead of claiming the PR is linked.\nWhen asked to monitor, watch, or babysit a PR and watch_pull_request is available, call it and end your turn: Tcode wakes you when checks finish, someone else comments, or the branch conflicts, so do not poll or run your own watcher. Tcode also wakes you once when the pull request merges or closes, then the watch ends. A wake is news, not a merge decision: check readiness yourself before merging. When you hand the work back to the user, call unwatch_pull_request first.",
        clis.join(", ")
    );
    for (stacks, docs) in hosts.iter().filter_map(|terms| terms.stacks) {
        text.push_str(&format!(
            "\nFor dependent changes, {stacks} preserve the full bottom-to-top topology and merge scope; see {docs} ."
        ));
    }
    text.push_str(LINKING_CLOSE);
    text
}

/// The rest of a stored turn's injected context when it leads with the linking instructions,
/// whichever host's they were: turns recorded before the instructions moved to the provider's
/// instruction channel carry them.
pub fn strip_linking_instructions(context: &str) -> Option<&str> {
    let block = context.strip_prefix(LINKING_OPEN)?;
    let end = block.find(LINKING_CLOSE)?;
    Some(&block[end + LINKING_CLOSE.len()..])
}

/// Whether an agent's command merges or closes a pull request through any host's CLI:
/// upstream's `\b(?:gh\s+pr|glab\s+mr)\s+(?:merge|close)\b` over the raw text.
pub fn merges_or_closes(command: &str) -> bool {
    let word = |c: char| c.is_alphanumeric() || c == '_';
    let spaced = |text: &str, rest: &str| -> Option<usize> {
        let trimmed = rest.trim_start();
        (trimmed.len() < rest.len() && trimmed.starts_with(text))
            .then(|| rest.len() - trimmed.len() + text.len())
    };
    command.char_indices().any(|(start, _)| {
        if command[..start].ends_with(word) {
            return false;
        }
        let rest = &command[start..];
        let commands = HOSTS.iter().flat_map(|terms| terms.merge_commands);
        let Some(rest) = commands.into_iter().find_map(|(tool, noun)| {
            let after = rest.strip_prefix(tool)?;
            Some(&after[spaced(noun, after)?..])
        }) else {
            return false;
        };
        ["merge", "close"]
            .into_iter()
            .any(|verb| spaced(verb, rest).is_some_and(|end| !rest[end..].starts_with(word)))
    })
}

/// `repository` is a canonical locator supplied by the forge adapter, opaque to core.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct PullRequestKey {
    #[serde(deserialize_with = "canonical_locator")]
    pub host: String,
    #[serde(deserialize_with = "canonical_locator")]
    pub repository: String,
    pub number: u64,
}

fn canonical_locator<'de, D: serde::Deserializer<'de>>(decoder: D) -> Result<String, D::Error> {
    Ok(String::deserialize(decoder)?.trim().to_ascii_lowercase())
}

impl PullRequestKey {
    pub fn new(host: &str, repository: &str, number: u64) -> Self {
        Self {
            host: host.trim().to_ascii_lowercase(),
            repository: repository.trim().to_ascii_lowercase(),
            number,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PullRequestState {
    Open,
    Closed,
    Merged,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PullRequestSource {
    Manual,
    /// Opened by an action in this thread. Links stored as `created` came from branch
    /// discovery, which Tcode no longer has; [`drop_discovered_links`] drops them as they are
    /// read.
    Opened,
    Agent,
    Stack,
    /// A key-only tombstone left by unlinking a native stack layer, so the stack's sync never
    /// relinks it.
    Dismissed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PullRequestAuthor {
    pub login: String,
    pub avatar_url: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChecksState {
    Passing,
    Failing,
    Pending,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewDecision {
    Approved,
    ChangesRequested,
    Required,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mergeability {
    Clean,
    Conflicting,
    /// The host says it is still working mergeability out, so an answer is coming.
    Pending,
    /// The host gives no answer it will settle, such as a draft's or one its API cannot state.
    #[default]
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PullRequestSnapshot {
    pub state: PullRequestState,
    pub title: String,
    pub head_branch: String,
    pub base_branch: String,
    pub is_draft: bool,
    pub updated_at: String,
    pub synced_at: u64,
    pub closed_at: Option<String>,
    pub merged_at: Option<String>,
    pub author: Option<PullRequestAuthor>,
    /// Lines added and removed; `None` where the host gave no count.
    #[serde(default)]
    pub additions: Option<u64>,
    #[serde(default)]
    pub deletions: Option<u64>,
    /// `None` where the host gave no count of changed files.
    #[serde(default)]
    pub changed_files: Option<u64>,
    pub review_decision: Option<ReviewDecision>,
    pub checks_state: Option<ChecksState>,
    #[serde(default)]
    pub mergeability: Mergeability,
}

impl PullRequestSnapshot {
    pub fn same_observation(&self, other: &Self) -> bool {
        let mut observation = other.clone();
        observation.synced_at = self.synced_at;
        self == &observation
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PullRequestStackLayer {
    pub url: String,
    pub number: u64,
    pub head_branch: String,
    pub state: PullRequestState,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PullRequestStack {
    pub id: String,
    pub number: u64,
    pub url: String,
    pub base: String,
    /// The forge supplies the full topology, bottom to top, independently of links.
    pub layers: Vec<PullRequestStackLayer>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", content = "stack", rename_all = "snake_case")]
pub enum PullRequestStackState {
    #[default]
    Unknown,
    None,
    Native(PullRequestStack),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "reason", rename_all = "snake_case")]
pub enum PullRequestSyncError {
    NoCredential,
    HostDisabled,
    RateLimited { retry_at: u64 },
    NotFound,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ThreadPullRequestLink {
    pub key: PullRequestKey,
    pub source: PullRequestSource,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub linked_at: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot: Option<PullRequestSnapshot>,
    #[serde(default, skip_serializing_if = "is_unknown")]
    pub stack: PullRequestStackState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sync_error: Option<PullRequestSyncError>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub watch: Option<crate::pull_request_watch::PullRequestWatch>,
}

fn is_unknown(stack: &PullRequestStackState) -> bool {
    *stack == PullRequestStackState::Unknown
}

/// Reads a thread's links without those stored with source `created`: branch discovery made
/// them, and a link a user or agent adopted afterwards cannot be told apart from one it never
/// touched.
pub(crate) fn drop_discovered_links<'de, D: serde::Deserializer<'de>>(
    decoder: D,
) -> Result<Vec<ThreadPullRequestLink>, D::Error> {
    #[derive(Deserialize)]
    #[serde(rename_all = "snake_case")]
    enum Discovered {
        Created,
    }
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Stored {
        Link(Box<ThreadPullRequestLink>),
        Discovered { source: Discovered },
    }
    Ok(Vec::<Stored>::deserialize(decoder)?
        .into_iter()
        .filter_map(|stored| match stored {
            Stored::Link(link) => Some(*link),
            Stored::Discovered {
                source: Discovered::Created,
            } => None,
        })
        .collect())
}

impl ThreadPullRequestLink {
    pub fn visible(&self) -> bool {
        self.source != PullRequestSource::Dismissed
    }
}

/// Whether the thread shows the pull request: a visible link, or a layer of the native stack
/// a visible link carries.
pub fn shown(links: &[ThreadPullRequestLink], key: &PullRequestKey) -> bool {
    links.iter().filter(|link| link.visible()).any(|link| {
        link.key == *key
            || matches!(&link.stack, PullRequestStackState::Native(stack)
            if link.key.host == key.host
                && link.key.repository == key.repository
                && stack.layers.iter().any(|layer| layer.number == key.number))
    })
}

/// GitHub's merge methods, in the order its merge button lists them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PullRequestMergeMethod {
    Merge,
    Squash,
    Rebase,
}

/// How a merge, a branch update or auto-merge may reach a pull request the thread shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PullRequestStackRoute {
    /// In no native stack: it merges as one pull request.
    Single,
    /// Layer `index` (from 1, bottom first) of a native stack of `layers`, which GitHub merges
    /// with the layers below it.
    Layer { index: u32, layers: u32 },
    /// Whether it is in a native stack is not known yet.
    Unknown,
}

/// The route, and the native stack layer the pull request is when it is one.
pub fn stack_route<'a>(
    links: &'a [ThreadPullRequestLink],
    key: &PullRequestKey,
) -> (PullRequestStackRoute, Option<&'a PullRequestStackLayer>) {
    let visible = || links.iter().filter(|link| link.visible());
    let layer = visible().find_map(|link| match &link.stack {
        PullRequestStackState::Native(stack)
            if link.key.host == key.host && link.key.repository == key.repository =>
        {
            let index = stack
                .layers
                .iter()
                .position(|layer| layer.number == key.number)?;
            Some((
                PullRequestStackRoute::Layer {
                    index: index as u32 + 1,
                    layers: stack.layers.len() as u32,
                },
                &stack.layers[index],
            ))
        }
        _ => None,
    });
    match (layer, visible().find(|link| link.key == *key)) {
        (Some((route, layer)), _) => (route, Some(layer)),
        (None, Some(link)) if link.stack == PullRequestStackState::None => {
            (PullRequestStackRoute::Single, None)
        }
        _ => (PullRequestStackRoute::Unknown, None),
    }
}

/// The native stack a visible link of the thread carries with `key` among its layers.
pub fn native_stack<'a>(
    links: &'a [ThreadPullRequestLink],
    key: &PullRequestKey,
) -> Option<&'a PullRequestStack> {
    links
        .iter()
        .filter(|link| link.visible())
        .find_map(|link| match &link.stack {
            PullRequestStackState::Native(stack)
                if link.key.host == key.host
                    && link.key.repository == key.repository
                    && stack.layers.iter().any(|layer| layer.number == key.number) =>
            {
                Some(stack)
            }
            _ => None,
        })
}

/// Why GitHub will not merge a layer the merge scope holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StackBlocker {
    Draft,
    Closed,
}

/// How the thread holds a layer of its stack.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StackLayerCondition {
    Linked,
    Dismissed,
    NotLinked,
}

/// What a layer is to a merge of the selected one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StackLayerRole {
    Selected,
    /// Merged already, below the selected layer: not part of the merge.
    BelowMerged,
    /// Lands together with the selected layer.
    InScope,
    /// In the scope, and GitHub refuses to merge it.
    Blocks(StackBlocker),
    Above,
}

#[derive(Debug, Clone)]
pub struct StackMapRow<'a> {
    pub layer: &'a PullRequestStackLayer,
    /// The thread's visible link of the layer, whose snapshot titles it.
    pub link: Option<&'a ThreadPullRequestLink>,
    /// `None` where no snapshot of the layer says.
    pub draft: Option<bool>,
    pub condition: StackLayerCondition,
    pub role: StackLayerRole,
}

/// A native stack from the selected layer's point of view: every layer bottom to top, the
/// layers a merge of the selected one lands, and those GitHub refuses among them. Read from the
/// stored topology and links only; a write reads GitHub again.
#[derive(Debug, Clone)]
pub struct StackMap<'a> {
    pub stack: &'a PullRequestStack,
    pub rows: Vec<StackMapRow<'a>>,
    pub selected: usize,
}

impl StackMap<'_> {
    pub fn selected(&self) -> &StackMapRow<'_> {
        &self.rows[self.selected]
    }

    /// The unmerged layers from the bottom through the selected one, bottom first: what Merge
    /// stack lands.
    pub fn scope(&self) -> Vec<u64> {
        self.rows[..=self.selected]
            .iter()
            .filter(|row| row.layer.state != PullRequestState::Merged)
            .map(|row| row.layer.number)
            .collect()
    }

    /// The scope's layers GitHub refuses, below the selected one, bottom first.
    pub fn blockers(&self) -> Vec<(u64, StackBlocker)> {
        self.rows
            .iter()
            .filter_map(|row| match row.role {
                StackLayerRole::Blocks(blocker) => Some((row.layer.number, blocker)),
                _ => None,
            })
            .collect()
    }

    /// The open layers a rebase moves, bottom first.
    pub fn unmerged(&self) -> Vec<u64> {
        self.rows
            .iter()
            .filter(|row| row.layer.state != PullRequestState::Merged)
            .map(|row| row.layer.number)
            .collect()
    }
}

/// The selected layer's map of the native stack the thread shows it in. Every layer being
/// merged must be open and ready for review: a closed or draft layer below the selected one
/// blocks it, and one whose draft flag is unknown does not until a fresh read says.
pub fn stack_map<'a>(
    links: &'a [ThreadPullRequestLink],
    key: &PullRequestKey,
) -> Option<StackMap<'a>> {
    let stack = native_stack(links, key)?;
    let selected = stack
        .layers
        .iter()
        .position(|layer| layer.number == key.number)?;
    let rows = stack
        .layers
        .iter()
        .enumerate()
        .map(|(index, layer)| {
            let held = links.iter().find(|link| {
                link.key.host == key.host
                    && link.key.repository == key.repository
                    && link.key.number == layer.number
            });
            let link = held.filter(|link| link.visible());
            let condition = match held {
                Some(link) if link.visible() => StackLayerCondition::Linked,
                Some(_) => StackLayerCondition::Dismissed,
                None => StackLayerCondition::NotLinked,
            };
            let draft = link
                .and_then(|link| link.snapshot.as_ref())
                .map(|snapshot| snapshot.is_draft);
            let role = if index == selected {
                StackLayerRole::Selected
            } else if index > selected {
                StackLayerRole::Above
            } else if layer.state == PullRequestState::Merged {
                StackLayerRole::BelowMerged
            } else if layer.state == PullRequestState::Closed {
                StackLayerRole::Blocks(StackBlocker::Closed)
            } else if draft == Some(true) {
                StackLayerRole::Blocks(StackBlocker::Draft)
            } else {
                StackLayerRole::InScope
            };
            StackMapRow {
                layer,
                link,
                draft,
                condition,
                role,
            }
        })
        .collect();
    Some(StackMap {
        stack,
        rows,
        selected,
    })
}

/// A write to a native stack that the host is running, or whose end it could not confirm. The
/// host publishes it with every thread that shows the stack, so each device and a reconnecting
/// one show the same state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PullRequestStackOperation {
    #[serde(deserialize_with = "canonical_locator")]
    pub host: String,
    #[serde(deserialize_with = "canonical_locator")]
    pub repository: String,
    pub stack: u64,
    pub started_at: u64,
    pub kind: StackOperationKind,
}

impl PullRequestStackOperation {
    pub fn is_for(&self, host: &str, repository: &str, stack: u64) -> bool {
        self.host == host && self.repository == repository && self.stack == stack
    }

    /// Whether the operation is moving layer `number` now, so that layer waits for it: a merge
    /// whose scope holds it, until a sync read what became of an unconfirmed one, or a rebase
    /// still running over it.
    pub fn covers(&self, number: u64) -> bool {
        match &self.kind {
            StackOperationKind::Merging { layers, .. }
            | StackOperationKind::MergeUnconfirmed {
                layers,
                checked: false,
                ..
            } => layers.contains(&number),
            StackOperationKind::Rebasing { layers } => {
                layers.iter().any(|layer| layer.number == number)
            }
            StackOperationKind::MergeUnconfirmed { checked: true, .. }
            | StackOperationKind::RebaseEnded { .. } => false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "content", rename_all = "snake_case")]
pub enum StackOperationKind {
    /// GitHub's asynchronous merge `id`, submitted from `target`, landing `layers` bottom first.
    /// `adopted` when GitHub named an operation already running instead of taking a new one.
    Merging {
        id: String,
        target: u64,
        layers: Vec<u64>,
        adopted: bool,
    },
    /// Still pending when the host stopped following it; never submitted again. `checked` once a
    /// sync after that read the target still open, so the stack's writes are offered again.
    MergeUnconfirmed {
        id: String,
        target: u64,
        layers: Vec<u64>,
        checked: bool,
    },
    Rebasing {
        layers: Vec<StackRebaseLayer>,
    },
    /// A rebase that ended, each layer at its final step, kept until the next full sync so
    /// every device can still read how it ended and what to do about a stop.
    RebaseEnded {
        layers: Vec<StackRebaseLayer>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StackRebaseLayer {
    pub number: u64,
    pub branch: String,
    pub step: StackRebaseStep,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "content", rename_all = "snake_case")]
pub enum StackRebaseStep {
    Waiting,
    Rebasing,
    Pushing,
    Pushed {
        from: String,
        to: String,
    },
    AlreadyCurrent,
    Failed {
        reason: StackRebaseFailure,
    },
    /// Above the layer the rebase stopped at.
    NotStarted,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "content", rename_all = "snake_case")]
pub enum StackRebaseFailure {
    /// The layer's own commits do not apply onto the layer below; the scratch rebase was
    /// aborted and the branch left as it was.
    Conflict,
    /// The branch is no longer at the head that was reviewed, so the lease kept it.
    LeaseRefused,
    /// The push got no answer in time, so it may have landed.
    PushUnconfirmed,
    Git {
        step: StackRebaseGitStep,
        message: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StackRebaseGitStep {
    Preparing,
    Fetching,
    ForkPoint,
    CheckingOut,
    Rebasing,
    Pushing,
}

/// The operation the thread shows for the native stack `key` is a layer of.
pub fn stack_operation<'a>(
    operations: &'a [PullRequestStackOperation],
    links: &[ThreadPullRequestLink],
    key: &PullRequestKey,
) -> Option<&'a PullRequestStackOperation> {
    let stack = native_stack(links, key)?;
    operations
        .iter()
        .find(|operation| operation.is_for(&key.host, &key.repository, stack.number))
}

/// The visible links a watch holds, which keep their thread waiting between wakes.
pub fn watched(links: &[ThreadPullRequestLink]) -> impl Iterator<Item = &ThreadPullRequestLink> {
    links
        .iter()
        .filter(|link| link.visible() && link.watch.is_some())
}

pub fn link_pull_request(
    links: &mut Vec<ThreadPullRequestLink>,
    key: PullRequestKey,
    url: String,
    source: PullRequestSource,
    now: u64,
    explicit: bool,
) -> bool {
    if let Some(existing) = links.iter_mut().find(|link| link.key == key) {
        if existing.visible() || !explicit {
            return false;
        }
        *existing = ThreadPullRequestLink {
            key,
            url,
            source,
            linked_at: Some(now),
            snapshot: None,
            stack: PullRequestStackState::Unknown,
            sync_error: None,
            watch: None,
        };
    } else {
        links.push(ThreadPullRequestLink {
            key,
            url,
            source,
            linked_at: Some(now),
            snapshot: None,
            stack: PullRequestStackState::Unknown,
            sync_error: None,
            watch: None,
        });
    }
    true
}

/// Removes the link; a native stack layer leaves a key-only tombstone instead, which only an
/// explicit link restores.
pub fn unlink_pull_request(links: &mut Vec<ThreadPullRequestLink>, key: &PullRequestKey) -> bool {
    let Some(index) = links
        .iter()
        .position(|link| &link.key == key && link.visible())
    else {
        return false;
    };
    let in_stack = links[index].source == PullRequestSource::Stack
        || matches!(links[index].stack, PullRequestStackState::Native(_))
        || links.iter().any(|link| {
            matches!(&link.stack, PullRequestStackState::Native(stack)
                if link.key.host == key.host
                    && link.key.repository == key.repository
                    && stack.layers.iter().any(|layer| layer.number == key.number))
        });
    if !in_stack {
        links.remove(index);
        return true;
    }
    links[index] = ThreadPullRequestLink {
        key: key.clone(),
        source: PullRequestSource::Dismissed,
        url: String::new(),
        linked_at: None,
        snapshot: None,
        stack: PullRequestStackState::Unknown,
        sync_error: None,
        watch: None,
    };
    true
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PullRequestBadgeState {
    Draft,
    Unknown,
    Open,
    Merged,
    Closed,
}

impl PullRequestBadgeState {
    pub fn of(snapshot: Option<&PullRequestSnapshot>) -> Self {
        match snapshot.map(|snapshot| (snapshot.state, snapshot.is_draft)) {
            Some((PullRequestState::Open, true)) => Self::Draft,
            Some((PullRequestState::Open, false)) => Self::Open,
            Some((PullRequestState::Merged, _)) => Self::Merged,
            Some((PullRequestState::Closed, _)) => Self::Closed,
            None => Self::Unknown,
        }
    }
}

pub fn badge(links: &[ThreadPullRequestLink]) -> Option<(PullRequestBadgeState, usize, bool)> {
    let visible: Vec<_> = links.iter().filter(|link| link.visible()).collect();
    if visible.is_empty() {
        return None;
    }
    let open: Vec<_> = visible
        .iter()
        .filter(|link| {
            link.snapshot
                .as_ref()
                .is_none_or(|s| s.state == PullRequestState::Open)
        })
        .collect();
    let state = if visible.iter().all(|link| link.snapshot.is_none()) {
        PullRequestBadgeState::Unknown
    } else if !open.is_empty() {
        if visible.iter().all(|link| {
            link.snapshot
                .as_ref()
                .is_some_and(|s| s.state == PullRequestState::Open && s.is_draft)
        }) {
            PullRequestBadgeState::Draft
        } else {
            PullRequestBadgeState::Open
        }
    } else if visible.iter().all(|link| {
        link.snapshot
            .as_ref()
            .is_some_and(|s| s.state == PullRequestState::Merged)
    }) {
        PullRequestBadgeState::Merged
    } else {
        PullRequestBadgeState::Closed
    };
    let chains = groups(links);
    if visible.len() > 1 && chains.len() == 1 {
        return Some((state, visible.len(), true));
    }
    Some((state, visible.len(), false))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PullRequestGroupKind {
    Native,
    Derived,
    Single,
}
#[derive(Debug, Clone)]
pub struct PullRequestGroup<'a> {
    pub kind: PullRequestGroupKind,
    pub stack: Option<&'a PullRequestStack>,
    pub links: Vec<&'a ThreadPullRequestLink>,
}

pub fn groups(links: &[ThreadPullRequestLink]) -> Vec<PullRequestGroup<'_>> {
    use std::collections::{HashMap, HashSet};
    let visible: Vec<_> = links.iter().filter(|link| link.visible()).collect();
    let mut groups = Vec::new();
    let mut placed = HashSet::new();
    for link in &visible {
        let PullRequestStackState::Native(stack) = &link.stack else {
            continue;
        };
        if placed.contains(&link.key) {
            continue;
        }
        let mut members: Vec<_> = stack
            .layers
            .iter()
            .filter_map(|layer| {
                visible
                    .iter()
                    .find(|member| {
                        member.key.host == link.key.host
                            && member.key.repository == link.key.repository
                            && member.key.number == layer.number
                    })
                    .copied()
            })
            .collect();
        if !members.iter().any(|member| member.key == link.key) {
            members.push(link);
        }
        placed.extend(members.iter().map(|member| member.key.clone()));
        groups.push(PullRequestGroup {
            kind: PullRequestGroupKind::Native,
            stack: Some(stack),
            links: members,
        });
    }
    let remaining: Vec<_> = visible
        .iter()
        .filter(|link| !placed.contains(&link.key))
        .copied()
        .collect();
    let mut by_head: HashMap<(&str, &str, &str), Option<&ThreadPullRequestLink>> = HashMap::new();
    for link in &remaining {
        if let Some(snapshot) = &link.snapshot {
            let key = (
                link.key.host.as_str(),
                link.key.repository.as_str(),
                snapshot.head_branch.as_str(),
            );
            if let std::collections::hash_map::Entry::Vacant(entry) = by_head.entry(key) {
                entry.insert(Some(*link));
            } else {
                by_head.insert(key, None);
            }
        }
    }
    let parent = |link: &ThreadPullRequestLink| {
        link.snapshot
            .as_ref()
            .and_then(|snapshot| {
                by_head
                    .get(&(
                        link.key.host.as_str(),
                        link.key.repository.as_str(),
                        snapshot.base_branch.as_str(),
                    ))
                    .copied()
                    .flatten()
            })
            .filter(|parent| parent.key != link.key)
    };
    let parents: HashSet<_> = remaining
        .iter()
        .filter_map(|link| parent(link).map(|link| &link.key))
        .collect();
    for top in &remaining {
        if parents.contains(&top.key) {
            continue;
        }
        let mut members = Vec::new();
        let mut cursor = Some(*top);
        while let Some(link) = cursor {
            if !placed.insert(link.key.clone()) {
                break;
            }
            members.insert(0, link);
            cursor = parent(link);
        }
        if !members.is_empty() {
            groups.push(PullRequestGroup {
                kind: if members.len() > 1 {
                    PullRequestGroupKind::Derived
                } else {
                    PullRequestGroupKind::Single
                },
                stack: None,
                links: members,
            });
        }
    }
    for link in remaining {
        if placed.insert(link.key.clone()) {
            groups.push(PullRequestGroup {
                kind: PullRequestGroupKind::Single,
                stack: None,
                links: vec![link],
            });
        }
    }
    groups.sort_by_key(|group| {
        std::cmp::Reverse(
            group
                .links
                .iter()
                .map(|link| {
                    link.snapshot
                        .as_ref()
                        .and_then(|s| chrono::DateTime::parse_from_rfc3339(&s.updated_at).ok())
                        .map(|time| time.timestamp().max(0) as u64)
                        .unwrap_or_else(|| link.linked_at.unwrap_or(0))
                })
                .max(),
        )
    });
    groups
}

/// A review being written on the host. GitHub sees none of it until it is submitted whole, so a
/// draft outlives a client, a device and a moved head alike.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PullRequestReviewDraft {
    pub key: PullRequestKey,
    /// The GitHub account writing it, as the conversation names it: a draft is another
    /// account's work once the host reads as someone else, so it is kept and not shown.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub account: String,
    /// The head commit the comments are anchored at.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub head: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub body: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub comments: Vec<PullRequestReviewDraftComment>,
    /// Never reused, so an edit naming a removed comment cannot reach a later one.
    #[serde(default)]
    pub next_id: u64,
    /// The last submission got no answer, so it may have been posted; set until the pull request
    /// is read again.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub uncertain: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PullRequestReviewDraftComment {
    pub id: u64,
    /// The commit whose text the comment's side shows: the base for the old side, the head for
    /// the new.
    pub revision: String,
    pub path: String,
    pub side: crate::session::ReviewSide,
    pub start_line: u32,
    pub end_line: u32,
    pub body: String,
    /// False once the head moved and its lines no longer read as they did: kept, never sent.
    #[serde(default = "placed", skip_serializing_if = "is_placed")]
    pub placed: bool,
}

fn placed() -> bool {
    true
}

fn is_placed(placed: &bool) -> bool {
    *placed
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "content", rename_all = "snake_case")]
pub enum PullRequestReviewDraftEdit {
    /// Lines of the diff read at `head`; a draft anchored at another head takes no new comments
    /// until it is moved to this one.
    AddComment {
        head: String,
        revision: String,
        path: String,
        side: crate::session::ReviewSide,
        start_line: u32,
        end_line: u32,
        body: String,
    },
    EditComment {
        id: u64,
        body: String,
    },
    RemoveComment {
        id: u64,
    },
    SetBody {
        body: String,
    },
    /// Re-anchors the comments at the pull request's current head. The host applies it with
    /// [`reanchor_review`], since only it can read whether each comment's lines changed.
    MoveToHead,
    Discard,
}

/// The account's draft of the pull request.
pub fn review_draft<'a>(
    drafts: &'a [PullRequestReviewDraft],
    key: &PullRequestKey,
    account: &str,
) -> Option<&'a PullRequestReviewDraft> {
    drafts
        .iter()
        .find(|draft| draft.key == *key && draft.account == account)
}

fn draft_index(
    drafts: &[PullRequestReviewDraft],
    key: &PullRequestKey,
    account: &str,
) -> Option<usize> {
    drafts
        .iter()
        .position(|draft| draft.key == *key && draft.account == account)
}

/// Applies an edit to the account's draft; false when it changed nothing, was malformed, or was
/// [`PullRequestReviewDraftEdit::MoveToHead`].
pub fn edit_review_draft(
    drafts: &mut Vec<PullRequestReviewDraft>,
    key: &PullRequestKey,
    account: &str,
    edit: PullRequestReviewDraftEdit,
) -> bool {
    let index = match draft_index(drafts, key, account) {
        Some(index) => index,
        None if matches!(edit, PullRequestReviewDraftEdit::AddComment { .. })
            || matches!(&edit, PullRequestReviewDraftEdit::SetBody { body } if !body.is_empty()) =>
        {
            drafts.push(PullRequestReviewDraft {
                key: key.clone(),
                account: account.to_owned(),
                head: String::new(),
                body: String::new(),
                comments: Vec::new(),
                next_id: 1,
                uncertain: false,
            });
            drafts.len() - 1
        }
        None => return false,
    };
    let draft = &mut drafts[index];
    let changed = match edit {
        PullRequestReviewDraftEdit::AddComment {
            head,
            revision,
            path,
            side,
            start_line,
            end_line,
            body,
        } => {
            let anchored = draft.comments.is_empty() || draft.head == head;
            if !anchored
                || head.is_empty()
                || path.is_empty()
                || start_line == 0
                || start_line > end_line
                || body.trim().is_empty()
            {
                false
            } else {
                draft.head = head;
                draft.comments.push(PullRequestReviewDraftComment {
                    id: draft.next_id,
                    revision,
                    path,
                    side,
                    start_line,
                    end_line,
                    body,
                    placed: true,
                });
                draft.next_id += 1;
                true
            }
        }
        PullRequestReviewDraftEdit::EditComment { id, body } => {
            match draft.comments.iter_mut().find(|comment| comment.id == id) {
                Some(comment) if !body.trim().is_empty() && comment.body != body => {
                    comment.body = body;
                    true
                }
                _ => false,
            }
        }
        PullRequestReviewDraftEdit::RemoveComment { id } => {
            let before = draft.comments.len();
            draft.comments.retain(|comment| comment.id != id);
            draft.comments.len() != before
        }
        PullRequestReviewDraftEdit::SetBody { body } => {
            body != std::mem::replace(&mut draft.body, body.clone())
        }
        PullRequestReviewDraftEdit::MoveToHead => false,
        PullRequestReviewDraftEdit::Discard => {
            drafts.remove(index);
            return true;
        }
    };
    if draft.body.is_empty() && draft.comments.is_empty() {
        drafts.remove(index);
    }
    changed
}

/// Anchors the draft at `head`. `moved` names where each comment's lines are now, or `None` when
/// they changed, which leaves the comment unplaced.
pub fn reanchor_review(
    drafts: &mut [PullRequestReviewDraft],
    key: &PullRequestKey,
    account: &str,
    head: &str,
    moved: impl Fn(&PullRequestReviewDraftComment) -> Option<String>,
) -> bool {
    let Some(index) = draft_index(drafts, key, account) else {
        return false;
    };
    let draft = &mut drafts[index];
    if draft.head == head {
        return false;
    }
    draft.head = head.to_owned();
    for comment in &mut draft.comments {
        match moved(comment) {
            Some(revision) if comment.placed => comment.revision = revision,
            _ => comment.placed = false,
        }
    }
    true
}

/// What a submission did to the draft. Once GitHub took it, its comments go by id, and its body
/// only while it still reads as sent, since a body revised meanwhile is new work. An unanswered
/// one marks the draft until the pull request is read again.
pub fn submitted_review(
    drafts: &mut Vec<PullRequestReviewDraft>,
    key: &PullRequestKey,
    account: &str,
    sent: Option<(&[u64], &str)>,
) -> bool {
    let Some(index) = draft_index(drafts, key, account) else {
        return false;
    };
    let draft = &mut drafts[index];
    let Some((comments, body)) = sent else {
        draft.uncertain = true;
        return true;
    };
    draft
        .comments
        .retain(|comment| !comments.contains(&comment.id));
    if draft.body == body {
        draft.body.clear();
    }
    draft.uncertain = false;
    if draft.body.is_empty() && draft.comments.is_empty() {
        drafts.remove(index);
    }
    true
}

/// A menu visibility hint only; the host validates and canonicalizes the target.
pub fn is_pull_request_url(value: &str) -> bool {
    HOSTS.iter().any(|terms| (terms.pull_request_url)(value))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn merge_or_close_detection_matches_words_in_the_raw_command() {
        for command in [
            "gh pr merge 12 --squash",
            "cd repo && gh pr close 3",
            "(gh  pr\tmerge)",
            "glab mr merge 4",
        ] {
            assert!(merges_or_closes(command), "{command}");
        }
        for command in [
            "gh pr view 12",
            "ugh pr merge",
            "gh pr merged",
            "gh prmerge",
            "echo gh pr",
        ] {
            assert!(!merges_or_closes(command), "{command}");
        }
    }

    /// The model reads this text once per registration: with only GitHub hosts configured it is what
    /// it was before other hosts existed, word for word, and a configured Forgejo or Gitea host
    /// adds its CLI and name, once for the two.
    #[test]
    fn the_model_reads_the_hosts_settings_configure() {
        let github = hosts_in([HostKind::Github]);
        assert_eq!(
            linking_instructions(&github),
            concat!(
                "<pull_request_linking>\nWhen the tcode_pull_requests MCP server exposes link_pull_request, use it to register every pull request you create or work on for this thread. Call link_pull_request with the full PR URL immediately after creating a PR or starting work on an existing PR. For a stack, link every layer, not just the current branch or top PR. This applies to gh, gh stack, other CLIs and host APIs: they do not register PRs with this thread. Linking an already-linked PR is safe. Before finishing PR work, call list_thread_pull_requests and link anything missing. Do not link unrelated PRs mentioned only as background. If linking fails, report that failure instead of claiming the PR is linked.\nWhen asked to monitor, watch, or babysit a PR and watch_pull_request is available, call it and end your turn: Tcode wakes you when checks finish, someone else comments, or the branch conflicts, so do not poll or run your own watcher. Tcode also wakes you once when the pull request merges or closes, then the watch ends. A wake is news, not a merge decision: check readiness yourself before merging. When you hand the work back to the user, call unwatch_pull_request first.",
                "\nFor dependent changes, GitHub native stacks preserve the full bottom-to-top topology and merge scope; see https://docs.github.com/en/pull-requests/collaborating-with-pull-requests/working-with-stacked-pull-requests .",
                "\n</pull_request_linking>\n\n"
            )
        );
        assert_eq!(host_names(&github), "GitHub");
        let all = hosts_in([HostKind::Gitea, HostKind::Forgejo]);
        assert_eq!(host_names(&all), "GitHub or Forgejo or Gitea");
        assert!(
            linking_instructions(&all).contains("This applies to gh, gh stack, tea, other CLIs")
        );
        assert!(merges_or_closes("tea pr merge 3"));
        assert!(merges_or_closes("glab mr close 3"));
        let gitlab = hosts_in([HostKind::Gitlab]);
        assert_eq!(host_names(&gitlab), "GitHub or GitLab");
        assert!(
            linking_instructions(&gitlab)
                .contains("This applies to gh, gh stack, glab, other CLIs")
        );
        // Bitbucket has no CLI to name, so the list stays GitHub's.
        let bitbucket = hosts_in([HostKind::Bitbucket]);
        assert_eq!(host_names(&bitbucket), "GitHub or Bitbucket");
        assert!(
            linking_instructions(&bitbucket).contains("This applies to gh, gh stack, other CLIs")
        );
        // `az repos pr` has no merge or close verb, so no command of it is taken for one.
        let azure = hosts_in([HostKind::Azure]);
        assert_eq!(host_names(&azure), "GitHub or Azure DevOps");
        assert!(
            linking_instructions(&azure)
                .contains("This applies to gh, gh stack, az repos, other CLIs")
        );
        assert!(!merges_or_closes(
            "az repos pr update --id 3 --status completed"
        ));
        assert!(
            strip_linking_instructions(&format!("{}typed", linking_instructions(&all)))
                == Some("typed")
        );
    }

    /// What Add host accepts, and what the host's validation of a settings write accepts: a
    /// GitHub host is a host name alone; a Forgejo or Gitea server may carry a port and a mount.
    #[test]
    fn each_kind_names_its_hosts_by_its_own_rule() {
        assert_eq!(
            HostKind::Forgejo.authority(" HTTPS://Git.Acme.test:3000/forge/ "),
            Ok("git.acme.test:3000/forge".to_owned())
        );
        assert_eq!(
            HostKind::Github.authority("github.example.com:8443"),
            Err(HostRefusal::PortOrPath)
        );
        assert_eq!(HostKind::Gitea.authority("  "), Err(HostRefusal::Blank));
        assert_eq!(
            HostKind::Gitlab.authority("https://Code.Acme.test:8443/"),
            Ok("code.acme.test:8443".to_owned())
        );
        assert_eq!(
            HostKind::Gitlab.authority("code.acme.test/gitlab"),
            Err(HostRefusal::Path)
        );
        assert_eq!(HostKind::detect("gitlab.com"), Some(HostKind::Gitlab));
        assert_eq!(
            HostKind::detect("gitlab.acme.test:8443"),
            Some(HostKind::Gitlab)
        );
        for invalid in ["a b", "git.acme.test:0", "git.acme.test/../x", "-x.test"] {
            assert_eq!(
                HostKind::Gitea.authority(invalid),
                Err(HostRefusal::Invalid),
                "{invalid}"
            );
        }
        assert_eq!(HostKind::detect("codeberg.org"), Some(HostKind::Forgejo));
        assert_eq!(
            HostKind::detect("gitea.acme.test:3000/x"),
            Some(HostKind::Gitea)
        );
        assert_eq!(HostKind::detect("ghost.acme.test"), None);
        // Bitbucket Cloud is bitbucket.org alone; a `bitbucket` label is Data Center's.
        assert_eq!(
            HostKind::Bitbucket.authority("https://Bitbucket.org/"),
            Ok("bitbucket.org".to_owned())
        );
        for other in [
            "bitbucket.acme.test",
            "bitbucket.org:8443",
            "bitbucket.org/x",
        ] {
            assert_eq!(
                HostKind::Bitbucket.authority(other),
                Err(HostRefusal::Fixed),
                "{other}"
            );
        }
        assert_eq!(HostKind::detect("bitbucket.org"), Some(HostKind::Bitbucket));
        assert_eq!(HostKind::detect("bitbucket.acme.test"), None);
        // An Azure DevOps host is an organization, however it is written.
        for organization in [
            "Fabrikam",
            "https://dev.azure.com/Fabrikam/",
            "dev.azure.com/fabrikam/Project",
            "fabrikam.visualstudio.com",
        ] {
            assert_eq!(
                HostKind::Azure.authority(organization),
                Ok("dev.azure.com/fabrikam".to_owned()),
                "{organization}"
            );
        }
        assert_eq!(
            HostKind::Azure.authority("https://dev.azure.com/"),
            Err(HostRefusal::Blank)
        );
        for invalid in [
            "git.acme.test",
            "fab rikam",
            "-fabrikam",
            "dev.azure.com/a.b",
        ] {
            assert_eq!(
                HostKind::Azure.authority(invalid),
                Err(HostRefusal::Invalid),
                "{invalid}"
            );
        }
        assert_eq!(
            HostKind::detect("dev.azure.com/fabrikam"),
            Some(HostKind::Azure)
        );
        assert_eq!(AZURE.display_name("dev.azure.com/fabrikam"), "Azure DevOps");
        assert_eq!(
            FORGEJO.display_name("git.acme.test:3000/forge"),
            "git.acme.test"
        );
        assert_eq!(GITHUB.display_name("github.example.com"), "GitHub");
    }

    fn link(number: u64, state: Option<PullRequestState>, draft: bool) -> ThreadPullRequestLink {
        ThreadPullRequestLink {
            key: PullRequestKey::new("github.com", "sample/project", number),
            url: format!("https://github.com/sample/project/pull/{number}"),
            source: PullRequestSource::Manual,
            linked_at: Some(number),
            snapshot: state.map(|state| PullRequestSnapshot {
                state,
                title: "Change".into(),
                head_branch: format!("layer-{number}"),
                base_branch: "main".into(),
                is_draft: draft,
                updated_at: "2026-10-08T00:00:00Z".into(),
                synced_at: 1,
                closed_at: None,
                merged_at: None,
                author: None,
                additions: Some(0),
                deletions: Some(0),
                changed_files: Some(0),
                review_decision: None,
                checks_state: None,
                mergeability: Mergeability::Unknown,
            }),
            stack: PullRequestStackState::Unknown,
            sync_error: None,
            watch: None,
        }
    }
    /// A snapshot thread metadata kept before counts could be absent still reads its counts; a
    /// host that gives none is read as giving none, not as zero lines or files.
    #[test]
    fn a_kept_snapshot_reads_its_counts() {
        let kept = r#"{"state":"open","title":"Change","head_branch":"feature","base_branch":"main","is_draft":false,"updated_at":"2026-10-08T00:00:00Z","synced_at":1,"closed_at":null,"merged_at":null,"author":null,"additions":3,"deletions":1,"changed_files":2,"review_decision":null,"checks_state":null,"mergeability":"clean"}"#;
        let snapshot: PullRequestSnapshot = serde_json::from_str(kept).unwrap();
        assert_eq!(
            (
                snapshot.additions,
                snapshot.deletions,
                snapshot.changed_files
            ),
            (Some(3), Some(1), Some(2))
        );
        let uncounted = kept.replace(r#""additions":3,"deletions":1,"changed_files":2,"#, "");
        let snapshot: PullRequestSnapshot = serde_json::from_str(&uncounted).unwrap();
        assert_eq!(
            (
                snapshot.additions,
                snapshot.deletions,
                snapshot.changed_files
            ),
            (None, None, None)
        );
    }
    #[test]
    fn unlinking_removes_a_link_and_tombstones_only_a_stack_layer() {
        let stack = PullRequestStack {
            id: "stack".into(),
            number: 1,
            url: "https://github.com/sample/project/stacks/1".into(),
            base: "main".into(),
            layers: [2, 3]
                .map(|number| PullRequestStackLayer {
                    url: format!("https://github.com/sample/project/pull/{number}"),
                    number,
                    head_branch: format!("layer-{number}"),
                    state: PullRequestState::Open,
                })
                .to_vec(),
        };
        let mut carrier = link(2, None, false);
        carrier.stack = PullRequestStackState::Native(stack);
        let mut layer = link(3, None, false);
        layer.source = PullRequestSource::Stack;
        let mut links = vec![link(1, None, false), carrier, layer];
        let key = |number| PullRequestKey::new("github.com", "sample/project", number);
        for number in [1, 2, 3] {
            assert!(unlink_pull_request(&mut links, &key(number)));
        }
        assert!(!unlink_pull_request(&mut links, &key(1)), "already gone");
        assert_eq!(
            links
                .iter()
                .map(|link| (link.key.number, link.source))
                .collect::<Vec<_>>(),
            [
                (2, PullRequestSource::Dismissed),
                (3, PullRequestSource::Dismissed)
            ],
            "the stack's layers stay dismissed so its sync does not relink them"
        );
        assert!(link_pull_request(
            &mut links,
            key(1),
            "https://github.com/sample/project/pull/1".into(),
            PullRequestSource::Manual,
            2,
            false,
        ));
        assert!(!link_pull_request(
            &mut links,
            key(3),
            "https://github.com/sample/project/pull/3".into(),
            PullRequestSource::Stack,
            2,
            false,
        ));
    }

    #[test]
    fn badge_distinguishes_unknown_drafts_terminal_and_related_links() {
        use PullRequestBadgeState as Badge;
        use PullRequestState as State;
        let cases = [
            (vec![], None),
            (vec![link(1, None, false)], Some((Badge::Unknown, 1, false))),
            (
                vec![
                    link(1, Some(State::Merged), false),
                    link(2, Some(State::Open), true),
                ],
                Some((Badge::Open, 2, false)),
            ),
            (
                vec![
                    link(1, Some(State::Open), true),
                    link(2, Some(State::Open), true),
                ],
                Some((Badge::Draft, 2, false)),
            ),
            (
                vec![link(1, Some(State::Merged), false), link(2, None, false)],
                Some((Badge::Open, 2, false)),
            ),
            (
                vec![
                    link(1, Some(State::Merged), false),
                    link(2, Some(State::Merged), false),
                ],
                Some((Badge::Merged, 2, false)),
            ),
            (
                vec![
                    link(1, Some(State::Merged), false),
                    link(2, Some(State::Closed), false),
                ],
                Some((Badge::Closed, 2, false)),
            ),
        ];
        for (links, expected) in cases {
            assert_eq!(badge(&links), expected);
        }
        let mut links = vec![
            link(1, Some(State::Open), false),
            link(2, Some(State::Open), false),
            link(3, Some(State::Open), false),
        ];
        links[1].snapshot.as_mut().unwrap().base_branch = "layer-1".into();
        links[2].snapshot.as_mut().unwrap().base_branch = "layer-2".into();
        assert_eq!(badge(&links), Some((Badge::Open, 3, true)));
        assert_eq!(
            groups(&links)[0]
                .links
                .iter()
                .map(|link| link.key.number)
                .collect::<Vec<_>>(),
            vec![1, 2, 3]
        );
        links[1].source = PullRequestSource::Dismissed;
        assert_eq!(badge(&links), Some((Badge::Open, 2, false)));
        assert!(is_pull_request_url(
            "https://github.com/sample/project/pull/123/files"
        ));
        assert!(is_pull_request_url(
            "https://github.com/sample/project/pull/123?view=1#issuecomment-7"
        ));
        assert!(is_pull_request_url(
            "https://git.acme.test:3000/forge/sample/project/pulls/4/files"
        ));
        assert!(is_pull_request_url(
            "https://code.acme.test:8443/group/sub/project/-/merge_requests/4/diffs"
        ));
        assert!(is_pull_request_url(
            "https://bitbucket.org/sample/project/pull-requests/9/diff"
        ));
        for ordinary in [
            "https://github.com/sample/project/issues/123",
            "https://github.com/sample/project/pull/0",
            "https://gitlab.com/project/-/merge_requests/4",
            "https://bitbucket.acme.test/sample/project/pull-requests/9",
            "https://example.test",
            "#123",
        ] {
            assert!(!is_pull_request_url(ordinary));
        }
    }

    #[test]
    fn a_layer_waits_only_for_the_write_moving_it_now() {
        let operation = |kind| PullRequestStackOperation {
            host: "github.com".into(),
            repository: "sample/project".into(),
            stack: 50,
            started_at: 1,
            kind,
        };
        let rebase = |step: StackRebaseStep| {
            vec![StackRebaseLayer {
                number: 2,
                branch: "layer-2".into(),
                step,
            }]
        };
        let unconfirmed = |checked| StackOperationKind::MergeUnconfirmed {
            id: "op".into(),
            target: 3,
            layers: vec![2, 3],
            checked,
        };
        let merging = operation(StackOperationKind::Merging {
            id: "op".into(),
            target: 3,
            layers: vec![2, 3],
            adopted: false,
        });
        assert!(
            merging.covers(3) && !merging.covers(4),
            "the layers above stay free"
        );
        assert!(operation(unconfirmed(false)).covers(2));
        assert!(
            !operation(unconfirmed(true)).covers(2),
            "a sync read the target still open"
        );
        assert!(
            operation(StackOperationKind::Rebasing {
                layers: rebase(StackRebaseStep::Rebasing)
            })
            .covers(2)
        );
        assert!(
            !operation(StackOperationKind::RebaseEnded {
                layers: rebase(StackRebaseStep::Failed {
                    reason: StackRebaseFailure::Conflict
                })
            })
            .covers(2),
            "a stopped rebase is read, not waited for"
        );
    }
}
