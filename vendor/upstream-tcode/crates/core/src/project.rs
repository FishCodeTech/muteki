//! Projects and session-index domain data.

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    path::{Path, PathBuf},
};

use agent::{OptionSelection, ProviderKind, ResumeCursor};
use serde::{Deserialize, Serialize};

use crate::settings::{ProjectSort, Settings, acp_color_key};
use crate::settlement::AgentReport;

/// A project groups sessions (threads) that share a working-directory root.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Project {
    pub id: String,
    pub name: String,
    pub root: PathBuf,
    /// Host-owned scratch group, independent of the client filesystem.
    #[serde(default)]
    pub quick_chat: bool,
    /// Host-owned image selected by the user; absent uses the project config's iconPath.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon_path: Option<PathBuf>,
    #[serde(default)]
    pub permission_defaults: BTreeMap<String, String>,
    pub created_at: u64,
}

impl Project {
    /// Create a project rooted at `root`, deriving a display name from its
    /// last path component (falling back to the full path).
    #[cfg(feature = "process")]
    pub fn from_root(root: PathBuf) -> Self {
        let name = project_name_from_root(&root);
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            name,
            root,
            quick_chat: false,
            icon_path: None,
            permission_defaults: BTreeMap::new(),
            created_at: now_secs(),
        }
    }
}

/// Derive a project display name from a directory path.
pub fn project_name_from_root(root: &Path) -> String {
    root.file_name()
        .map(|n| n.to_string_lossy().to_string())
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| root.display().to_string())
}

/// `path` with its `old_root` prefix replaced by `new_root`; `None` when
/// `path` is not under `old_root`.
pub fn rebase_path(path: &Path, old_root: &Path, new_root: &Path) -> Option<PathBuf> {
    let rest = path.strip_prefix(old_root).ok()?;
    Some(if rest.as_os_str().is_empty() {
        new_root.to_path_buf()
    } else {
        new_root.join(rest)
    })
}

/// Source checkout and branch of a session-owned worktree, used for cleanup.
/// The session's `cwd` holds the worktree path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorktreeInfo {
    /// The main project checkout the worktree was created from (its git root).
    pub root_project_path: PathBuf,
    /// The base branch/ref the worktree was branched from.
    pub base: String,
    /// The branch created for this worktree (`tcode/<session-id>`).
    pub branch: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SettledOverride {
    Settled,
    Active,
}

/// Index entry describing one persisted session.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionMeta {
    #[serde(
        default,
        deserialize_with = "crate::pull_request::drop_discovered_links"
    )]
    pub pull_requests: Vec<crate::pull_request::ThreadPullRequestLink>,
    /// Interrupt pauses monitoring for the thread, including watches registered afterwards.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub pull_request_watches_paused: bool,
    /// Reviews being written in this thread, one per pull request it shows.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pull_request_reviews: Vec<crate::pull_request::PullRequestReviewDraft>,
    /// Writes to the native stacks this thread shows that the host is running or could not
    /// confirm.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pull_request_operations: Vec<crate::pull_request::PullRequestStackOperation>,
    pub id: String,
    pub title: String,
    pub provider: ProviderKind,
    /// Which provider *profile* this session runs against. `None` (and absent in
    /// legacy index files) means the built-in profile for `provider`; `Some(id)`
    /// selects a user-created profile (e.g. a third-party Anthropic endpoint).
    /// `provider` stays the protocol discriminant — the profile only changes the
    /// env / binary / home the same protocol is spawned with.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile_id: Option<String>,
    pub cwd: PathBuf,
    #[serde(default)]
    pub project_id: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    /// Set when the thread is archived (unix secs). Archived threads vanish from
    /// the sidebar and are reversible from Settings → Archived Threads. Absent in
    /// legacy files (defaults to "not archived").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub archived_at: Option<u64>,
    /// Lifecycle timestamps are Unix seconds; activity in the event stream is Unix milliseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub settled_at: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub settled_override: Option<SettledOverride>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unsettled_at: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_settle_disabled_at: Option<u64>,
    /// A pinned thread is exempt from automatic settlement.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pinned_at: Option<u64>,
    /// Fractional order keys ([`crate::thread_sort::order_key_between`]); a
    /// row without one sorts by time within its section.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pin_order: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_order: Option<String>,
    /// Dedicated-worktree mode metadata, when the session runs in its own git
    /// worktree instead of the project checkout. Absent = local checkout.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree: Option<WorktreeInfo>,
    #[serde(default)]
    pub resume_cursor: Option<ResumeCursor>,
    /// Whether the next provider start must fork `resume_cursor` rather than
    /// resume it in place.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub pending_fork: bool,
    /// The directory a move to another project took this thread from, until the next turn
    /// hands the conversation to a fresh provider session in `cwd`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub moved_from: Option<PathBuf>,
    /// Set when this thread was imported from another tool's local history
    /// ("claude:<id>" / "codex:<id>"). Used to keep re-imports idempotent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub imported_from: Option<String>,
    /// Requested provider option values; absent choices use the descriptor default.
    #[serde(default)]
    pub option_selections: Vec<OptionSelection>,
    /// Which ACP agent this session runs (its registry id), when
    /// `provider == ProviderKind::Acp`. `None` for the native providers, and
    /// absent in every index file written before ACP existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub acp_agent_id: Option<String>,
    /// Parent orchestrator thread for native child sessions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_session_id: Option<String>,
    /// Set when this session mirrors a provider-native subagent transcript.
    /// Value is the parent-timeline Subagent item id (reattach key). Mirror
    /// sessions are read-only: disabled composer, no provider process.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native_subagent: Option<String>,
    /// Whether this session receives the tcode_orchestrate MCP registration.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub orchestrate_enabled: bool,
    /// Set (unix secs) when orchestrate `cancel` stopped this dispatched
    /// child: its result is not delivered. A message admitted to it clears it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cancelled_at: Option<u64>,
    /// Maximum inline result characters for this child's terminal callback.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result_max_chars: Option<u32>,
    /// The report this dispatched child owes its lead, opened by the lead's
    /// latest message. Absent until the first dispatch or `send`, and after
    /// `cancel`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expectation: Option<ChildExpectation>,
    /// This thread's own `tcode_control` grant, set by an approved request; absent means its project's or the default applies.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub control_grant: Option<crate::control::ControlGrant>,
    /// Grants the model asked for that the user has not answered.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub control_grant_requests: Vec<crate::control::ControlGrantRequest>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub references: Vec<crate::control::ThreadReference>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub follow_up: Option<crate::control::FollowUp>,
    /// Parked (unix secs): set aside on purpose, exempt from automatic settlement.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parked_at: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub settle_suggestion_dismissed_at: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub self_settle: Option<crate::control::SelfSettleGrant>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_by: Option<crate::control::StartedBy>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub control_watches: Vec<crate::control::ControlWatch>,
    pub created_at: u64,
    pub updated_at: u64,
}

/// One report a dispatched child owes its lead (see [`AgentReport`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChildExpectation {
    pub state: AgentReport,
    /// Unix seconds when the lead's message opened it.
    pub opened_at: u64,
    /// The `report_result` tool-call item whose text was delivered for it, so
    /// a later report in the same thread is recognised as an update.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delivered_item: Option<String>,
    /// Whether the child was already asked once to report after going quiet.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub reminded: bool,
}

impl ChildExpectation {
    pub fn opened(now_secs: u64) -> Self {
        Self {
            state: AgentReport::Expected,
            opened_at: now_secs,
            delivered_item: None,
            reminded: false,
        }
    }
}

impl SessionMeta {
    /// What this thread owes its lead; `None` for threads that are not
    /// dispatched children and for children without an open expectation.
    pub fn report(&self) -> AgentReport {
        self.expectation
            .as_ref()
            .map_or(AgentReport::None, |expectation| expectation.state)
    }
}

/// An ephemeral index of the stored and live sessions using each path or branch.
/// Two distinct owners suffice to answer sharing queries excluding the owner itself.
pub struct WorktreeSharing<'a> {
    paths: HashMap<PathBuf, Vec<&'a SessionMeta>>,
    branches: HashMap<&'a str, Vec<&'a SessionMeta>>,
}

impl<'a> WorktreeSharing<'a> {
    pub fn new(sessions: impl IntoIterator<Item = &'a SessionMeta>) -> Self {
        let mut sharing = Self {
            paths: HashMap::new(),
            branches: HashMap::new(),
        };
        for meta in sessions {
            for ancestor in meta.cwd.ancestors() {
                Self::add_owner(sharing.paths.entry(ancestor.to_owned()).or_default(), meta);
            }
            if let Some(worktree) = &meta.worktree {
                Self::add_owner(sharing.branches.entry(&worktree.branch).or_default(), meta);
            }
        }
        sharing
    }

    fn add_owner(owners: &mut Vec<&'a SessionMeta>, meta: &'a SessionMeta) {
        if owners.len() < 2 && owners.iter().all(|other| other.id != meta.id) {
            owners.push(meta);
        }
    }

    pub fn is_shared(&self, meta: &SessionMeta) -> bool {
        let Some(worktree) = &meta.worktree else {
            return false;
        };
        self.paths
            .get(&meta.cwd)
            .into_iter()
            .chain(self.branches.get(worktree.branch.as_str()))
            .flatten()
            .any(|other| meta.shares_worktree_with(other))
    }
}

impl SessionMeta {
    pub fn is_settled(&self) -> bool {
        self.settled_override == Some(SettledOverride::Settled)
            || (self.settled_override.is_none() && self.settled_at.is_some())
    }

    /// A dispatched orchestrate child or a provider-native subagent mirror:
    /// reached from its parent, never listed on its own.
    pub fn is_subagent(&self) -> bool {
        self.parent_session_id.is_some() || self.native_subagent.is_some()
    }

    /// A child its lead dispatched through orchestrate, as opposed to a
    /// provider-native subagent mirror.
    pub fn is_dispatched(&self) -> bool {
        self.parent_session_id.is_some() && self.native_subagent.is_none()
    }

    pub fn migrate_lifecycle(&mut self) {
        if self.settled_override.is_none() && self.settled_at.is_some() {
            self.settled_override = Some(SettledOverride::Settled);
        }
    }

    /// Whether `other` works in the worktree this session owns. A fork keeps
    /// the source's cwd without the `worktree` ownership marker, so the cwd
    /// decides as well as the branch.
    pub fn shares_worktree_with(&self, other: &SessionMeta) -> bool {
        let Some(worktree) = &self.worktree else {
            return false;
        };
        other.id != self.id
            && (other.cwd.starts_with(&self.cwd)
                || other
                    .worktree
                    .as_ref()
                    .is_some_and(|other| other.branch == worktree.branch))
    }

    /// The key [`Settings::provider_color`] resolves this thread's color from:
    /// a user profile id, an ACP agent (`acp:<id>`), or the built-in profile
    /// id. A user profile is its own provider to the user even
    /// when it drives a built-in protocol, so it never inherits the brand color.
    pub fn provider_color_key(&self) -> String {
        if let Some(profile_id) = self
            .profile_id
            .as_deref()
            .filter(|id| !Settings::is_builtin_profile_id(id))
        {
            return profile_id.to_string();
        }
        if self.provider == ProviderKind::Acp
            && let Some(agent_id) = self.acp_agent_id.as_deref()
        {
            return acp_color_key(agent_id);
        }
        Settings::builtin_profile_id(self.provider).to_string()
    }

    #[cfg(feature = "process")]
    pub fn new(provider: ProviderKind, cwd: PathBuf, model: Option<String>) -> Self {
        let now = now_secs();
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            title: format!("New {} session", provider.display_name()),
            pull_requests: Vec::new(),
            pull_request_watches_paused: false,
            pull_request_reviews: Vec::new(),
            pull_request_operations: Vec::new(),
            provider,
            profile_id: None,
            cwd,
            project_id: None,
            model,
            archived_at: None,
            settled_at: None,
            settled_override: None,
            unsettled_at: None,
            auto_settle_disabled_at: None,
            pinned_at: None,
            pin_order: None,
            active_order: None,
            worktree: None,
            resume_cursor: None,
            pending_fork: false,
            moved_from: None,
            imported_from: None,
            option_selections: Vec::new(),
            acp_agent_id: None,
            parent_session_id: None,
            native_subagent: None,
            orchestrate_enabled: false,
            cancelled_at: None,
            result_max_chars: None,
            expectation: None,
            control_grant: None,
            control_grant_requests: Vec::new(),
            references: Vec::new(),
            follow_up: None,
            parked_at: None,
            settle_suggestion_dismissed_at: None,
            self_settle: None,
            started_by: None,
            control_watches: Vec::new(),
            created_at: now,
            updated_at: now,
        }
    }
}

/// `root_id` and every thread under it, each parent before its children:
/// the threads archive, unarchive and delete act on together. Empty when
/// `root_id` is not among `sessions`.
pub fn descendant_session_ids<'a>(
    sessions: impl IntoIterator<Item = &'a SessionMeta>,
    root_id: &str,
) -> Vec<String> {
    let mut children: HashMap<&str, Vec<&str>> = HashMap::new();
    let mut found = false;
    for meta in sessions {
        found |= meta.id == root_id;
        if let Some(parent) = meta.parent_session_id.as_deref() {
            children.entry(parent).or_default().push(&meta.id);
        }
    }
    if !found {
        return Vec::new();
    }
    let mut visited = HashSet::from([root_id]);
    let mut output = vec![root_id.to_owned()];
    let mut next = 0;
    while let Some(id) = output.get(next).cloned() {
        for &child in children.get(id.as_str()).into_iter().flatten() {
            if visited.insert(child) {
                output.push(child.to_owned());
            }
        }
        next += 1;
    }
    output
}

/// A project and its sessions, ready for the sidebar (newest activity first).
#[derive(Debug, Clone)]
pub struct ProjectGroup {
    pub project: Project,
    pub sessions: Vec<SessionMeta>,
}

/// Group `sessions` under their `projects` and order projects using `sort`.
/// Callers choose the row order for their lifecycle or archive surface.
pub fn group_sessions(
    projects: &[Project],
    sessions: &[SessionMeta],
    sort: ProjectSort,
) -> Vec<ProjectGroup> {
    let mut groups: Vec<ProjectGroup> = projects
        .iter()
        .map(|project| {
            let sessions: Vec<SessionMeta> = sessions
                .iter()
                .filter(|s| s.project_id.as_deref() == Some(project.id.as_str()))
                .cloned()
                .collect();
            ProjectGroup {
                project: project.clone(),
                sessions,
            }
        })
        .collect();

    match sort {
        // Groups ordered by newest activity (falling back to project creation).
        ProjectSort::RecentActivity => groups.sort_by(|a, b| {
            let activity = |g: &ProjectGroup| {
                g.sessions
                    .iter()
                    .map(|s| s.updated_at)
                    .max()
                    .unwrap_or(g.project.created_at)
            };
            activity(b).cmp(&activity(a))
        }),
        // Groups ordered by project name, case-insensitive A-Z.
        ProjectSort::NameAsc => {
            groups.sort_by(|a, b| {
                a.project
                    .name
                    .to_lowercase()
                    .cmp(&b.project.name.to_lowercase())
            });
        }
    }
    groups
}

/// Every project and session in the store. Also the shape of the legacy
/// `sessions.json` the store migrates from; older files were a bare
/// `Vec<SessionMeta>`, and the migration tolerates both.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct IndexFile {
    #[serde(default)]
    pub projects: Vec<Project>,
    #[serde(default)]
    pub sessions: Vec<SessionMeta>,
}

/// Ensure every session belongs to a project, deriving implicit projects from
/// each orphan session's cwd (deduped by root). Idempotent.
#[cfg(feature = "process")]
pub fn migrate_index(mut file: IndexFile) -> IndexFile {
    // Map existing project roots to their ids so derived projects dedupe.
    let mut root_to_id: std::collections::HashMap<PathBuf, String> = file
        .projects
        .iter()
        .map(|p| (p.root.clone(), p.id.clone()))
        .collect();

    for session in &mut file.sessions {
        if session
            .project_id
            .as_ref()
            .is_some_and(|id| file.projects.iter().any(|p| &p.id == id))
        {
            continue;
        }
        let root = session.cwd.clone();
        let project_id = if let Some(id) = root_to_id.get(&root) {
            id.clone()
        } else {
            let project = Project::from_root(root.clone());
            let id = project.id.clone();
            root_to_id.insert(root, id.clone());
            file.projects.push(project);
            id
        };
        session.project_id = Some(project_id);
    }
    file
}

pub fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

#[cfg(all(test, feature = "process"))]
mod tests {
    use super::*;

    fn session_in(project_id: &str, updated_at: u64) -> SessionMeta {
        let mut meta = SessionMeta::new(ProviderKind::Codex, PathBuf::from("/x"), None);
        meta.project_id = Some(project_id.to_string());
        meta.updated_at = updated_at;
        meta
    }

    #[test]
    fn provider_color_key_prefers_user_profile_then_acp_agent_then_builtin() {
        let mut meta = SessionMeta::new(ProviderKind::ClaudeCode, PathBuf::from("/x"), None);
        assert_eq!(meta.provider_color_key(), "claude");
        // The built-in profile spelled out explicitly is still the built-in.
        meta.profile_id = Some("claude".into());
        assert_eq!(meta.provider_color_key(), "claude");
        // A third-party endpoint is a different provider to the user.
        meta.profile_id = Some("work-claude".into());
        assert_eq!(meta.provider_color_key(), "work-claude");

        let mut acp = SessionMeta::new(ProviderKind::Acp, PathBuf::from("/x"), None);
        acp.acp_agent_id = Some("gemini".into());
        assert_eq!(acp.provider_color_key(), "acp:gemini");
    }

    #[test]
    fn worktree_is_shared_by_a_fork_in_its_cwd_but_not_by_a_sibling_directory() {
        let mut owner = SessionMeta::new(ProviderKind::Codex, PathBuf::from("/wt/source"), None);
        owner.worktree = Some(WorktreeInfo {
            root_project_path: PathBuf::from("/repo"),
            base: "main".into(),
            branch: "tcode/source".into(),
        });
        let fork = SessionMeta::new(ProviderKind::Codex, owner.cwd.clone(), None);
        let nested = SessionMeta::new(ProviderKind::Codex, owner.cwd.join("crates/core"), None);
        let sibling = SessionMeta::new(ProviderKind::Codex, PathBuf::from("/wt/source-2"), None);
        let checkout = SessionMeta::new(ProviderKind::Codex, PathBuf::from("/repo"), None);

        assert!(WorktreeSharing::new([&owner, &fork]).is_shared(&owner));
        assert!(WorktreeSharing::new([&owner, &nested]).is_shared(&owner));
        assert!(!WorktreeSharing::new([&owner, &sibling]).is_shared(&owner));
        assert!(!WorktreeSharing::new([&owner, &checkout]).is_shared(&owner));
        assert!(!WorktreeSharing::new([&owner, &owner]).is_shared(&owner));
        let mut same_branch = sibling.clone();
        same_branch.worktree = owner.worktree.clone();
        assert!(WorktreeSharing::new([&owner, &same_branch]).is_shared(&owner));
        assert!(
            !WorktreeSharing::new([&owner, &fork]).is_shared(&fork),
            "a fork owns no worktree"
        );
    }

    #[test]
    fn group_sessions_orders_projects_by_activity_or_name() {
        let projects = vec![
            Project {
                id: "p-old".into(),
                name: "Old".into(),
                root: PathBuf::from("/old"),
                quick_chat: false,
                icon_path: None,
                permission_defaults: Default::default(),
                created_at: 1,
            },
            Project {
                id: "p-new".into(),
                name: "New".into(),
                root: PathBuf::from("/new"),
                quick_chat: false,
                icon_path: None,
                permission_defaults: Default::default(),
                created_at: 2,
            },
            Project {
                id: "p-empty".into(),
                name: "Empty".into(),
                root: PathBuf::from("/empty"),
                quick_chat: false,
                icon_path: None,
                permission_defaults: Default::default(),
                created_at: 15,
            },
        ];
        let sessions = vec![
            session_in("p-old", 10),
            session_in("p-new", 100),
            session_in("p-new", 50),
            session_in("p-old", 20),
        ];

        let groups = group_sessions(&projects, &sessions, ProjectSort::RecentActivity);
        // p-new (activity 100), p-old (activity 20), p-empty (created_at 15, no sessions).
        assert_eq!(groups[0].project.id, "p-new");
        assert_eq!(groups[1].project.id, "p-old");
        assert_eq!(groups[2].project.id, "p-empty");
        assert!(groups[2].sessions.is_empty());

        // Name A-Z ordering ignores activity: Empty, New, Old (case-insensitive).
        let by_name = group_sessions(&projects, &sessions, ProjectSort::NameAsc);
        assert_eq!(by_name[0].project.name, "Empty");
        assert_eq!(by_name[1].project.name, "New");
        assert_eq!(by_name[2].project.name, "Old");
    }

    #[test]
    fn what_branch_discovery_stored_is_dropped_as_the_thread_is_read() {
        let link = |number: u64, source: &str| {
            serde_json::json!({
                "key": {"host": "github.com", "repository": "sample/project", "number": number},
                "source": source,
                "url": format!("https://github.com/sample/project/pull/{number}"),
                "linked_at": 1
            })
        };
        let stored = serde_json::json!({
            "id": "legacy", "title": "Legacy", "provider": "codex",
            "cwd": "/work", "created_at": 1, "updated_at": 2,
            "pull_requests": [
                link(1, "manual"),
                link(2, "created"),
                link(3, "agent"),
                link(4, "stack"),
                {"key": {"host": "github.com", "repository": "sample/project", "number": 5},
                 "source": "dismissed"},
                link(6, "opened")
            ],
            "branch_pull_request": {
                "key": {"host": "github.com", "repository": "sample/project", "number": 7},
                "url": "https://github.com/sample/project/pull/7"
            }
        });
        let meta: SessionMeta = serde_json::from_value(stored).unwrap();
        let kept: Vec<_> = meta
            .pull_requests
            .iter()
            .map(|link| (link.key.number, link.source))
            .collect();
        use crate::pull_request::PullRequestSource::*;
        assert_eq!(
            kept,
            [
                (1, Manual),
                (3, Agent),
                (4, Stack),
                (5, Dismissed),
                (6, Opened)
            ]
        );
        let malformed = serde_json::json!({
            "id": "legacy", "title": "Legacy", "provider": "codex",
            "cwd": "/work", "created_at": 1, "updated_at": 2,
            "pull_requests": [{"source": "manual"}]
        });
        assert!(
            serde_json::from_value::<SessionMeta>(malformed).is_err(),
            "only discovery's links are dropped; an unreadable link still fails the thread"
        );
    }

    #[test]
    fn session_metadata_preserves_legacy_defaults_and_persisted_options() {
        let legacy = serde_json::json!({
            "id": "legacy", "title": "Legacy", "provider": "codex",
            "cwd": "/work", "forked_from": "source", "created_at": 1, "updated_at": 2,
            "checkpoints": [{"turn": 2, "commit": "deadbeef", "event_offset": 7}],
            "approval_mode": "read_only"
        });
        let meta: SessionMeta = serde_json::from_value(legacy).unwrap();
        assert!(!meta.pending_fork);
        assert_eq!(meta.parent_session_id, None);
        assert_eq!(meta.native_subagent, None);
        assert!(!meta.orchestrate_enabled);
        assert_eq!(meta.archived_at, None);
        assert_eq!(meta.worktree, None);
        let json = serde_json::to_value(&meta).unwrap();
        for omitted in [
            "forked_from",
            "checkpoints",
            "approval_mode",
            "pending_fork",
            "parent_session_id",
            "native_subagent",
            "orchestrate_enabled",
            "archived_at",
            "worktree",
        ] {
            assert!(json.get(omitted).is_none(), "unexpected field: {omitted}");
        }

        let mut meta = meta;
        meta.pending_fork = true;
        meta.parent_session_id = Some("parent".into());
        meta.native_subagent = Some("spawn-1".into());
        meta.orchestrate_enabled = true;
        meta.archived_at = Some(1234);
        meta.worktree = Some(WorktreeInfo {
            root_project_path: PathBuf::from("/proj"),
            base: "main".into(),
            branch: "tcode/abc".into(),
        });
        let json = serde_json::to_value(&meta).unwrap();
        assert!(json.get("forked_from").is_none());
        assert!(json.get("checkpoints").is_none());
        assert_eq!(serde_json::from_value::<SessionMeta>(json).unwrap(), meta);
    }
}
