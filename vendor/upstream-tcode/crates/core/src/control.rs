//! Grants, requests and records of the `tcode_control` surface: what a thread's
//! model may observe and operate on in Tcode, and what it did.

use std::collections::{BTreeMap, BTreeSet};

pub use agent::ControlAction;
use serde::{Deserialize, Serialize};

#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Serialize,
    Deserialize,
    schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum ControlTier {
    Observe,
    Organize,
    Operate,
}

impl ControlTier {
    pub const ALL: [ControlTier; 3] = [Self::Observe, Self::Organize, Self::Operate];

    pub fn name(self) -> &'static str {
        match self {
            Self::Observe => "observe",
            Self::Organize => "organize",
            Self::Operate => "operate",
        }
    }
}

/// Operations off by default, each enabled on its own.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Serialize,
    Deserialize,
    schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum ControlCategory {
    AnswerInput,
    Terminal,
    Delete,
    GitRemote,
    PrWrite,
    HistoryEdit,
    SettingsWrite,
}

impl ControlCategory {
    pub const ALL: [ControlCategory; 7] = [
        Self::AnswerInput,
        Self::Terminal,
        Self::Delete,
        Self::GitRemote,
        Self::PrWrite,
        Self::HistoryEdit,
        Self::SettingsWrite,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Self::AnswerInput => "answer_input",
            Self::Terminal => "terminal",
            Self::Delete => "delete",
            Self::GitRemote => "git_remote",
            Self::PrWrite => "pr_write",
            Self::HistoryEdit => "history_edit",
            Self::SettingsWrite => "settings_write",
        }
    }
}

/// Which projects a grant reaches. `Project` is the calling thread's own.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ControlScope {
    Project,
    Projects(Vec<String>),
    Host,
}

impl ControlScope {
    /// Whether a target in `target_project` is reachable from a caller in `caller_project`.
    /// Every scope includes the caller's own project; a thread without a project is
    /// reachable only from a host-wide scope, or from a caller that also has none.
    pub fn reaches(&self, caller_project: Option<&str>, target_project: Option<&str>) -> bool {
        match self {
            Self::Host => true,
            Self::Project => caller_project == target_project,
            Self::Projects(ids) => {
                caller_project == target_project
                    || target_project.is_some_and(|id| ids.iter().any(|p| p == id))
            }
        }
    }

    /// Whether a call may ask for this scope under `granted`, from a caller in
    /// `caller_project`: everything it reaches, `granted` reaches too.
    pub fn within(&self, granted: &ControlScope, caller_project: Option<&str>) -> bool {
        match (self, granted) {
            (_, Self::Host) | (Self::Project, _) => true,
            (Self::Projects(ids), _) => ids
                .iter()
                .all(|id| granted.reaches(caller_project, Some(id))),
            (Self::Host, _) => false,
        }
    }

    pub fn describe(&self) -> String {
        match self {
            Self::Project => "project".into(),
            Self::Projects(ids) => format!("projects {}", ids.join(", ")),
            Self::Host => "host".into(),
        }
    }

    /// The scope covering both, for a caller in `caller_project`: `Host` dominates, and
    /// project lists join, naming the caller's own project when one side meant it.
    fn widen(&mut self, other: &ControlScope, caller_project: Option<&str>) {
        let ids = |scope: &ControlScope| match scope {
            Self::Projects(ids) => ids.clone(),
            _ => caller_project.map(str::to_owned).into_iter().collect(),
        };
        *self = match (&*self, other) {
            (Self::Host, _) | (_, Self::Host) => Self::Host,
            (Self::Project, Self::Project) => Self::Project,
            (a, b) => {
                let mut joined = ids(a);
                for id in ids(b) {
                    if !joined.contains(&id) {
                        joined.push(id);
                    }
                }
                Self::Projects(joined)
            }
        };
    }
}

/// Scope × tiers, plus individually enabled privileged categories.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ControlGrant {
    pub scope: ControlScope,
    #[serde(default)]
    pub tiers: BTreeSet<ControlTier>,
    #[serde(default)]
    pub categories: BTreeSet<ControlCategory>,
}

impl Default for ControlGrant {
    fn default() -> Self {
        Self::default_grant()
    }
}

impl ControlGrant {
    /// The caller's own project, every tier, no privileged category.
    pub fn default_grant() -> Self {
        Self {
            scope: ControlScope::Project,
            tiers: ControlTier::ALL.into_iter().collect(),
            categories: BTreeSet::new(),
        }
    }

    /// Nothing at all: the thread gets `request_grant` and nothing else.
    pub fn none() -> Self {
        Self {
            scope: ControlScope::Project,
            tiers: BTreeSet::new(),
            categories: BTreeSet::new(),
        }
    }

    pub fn allows_tier(&self, tier: ControlTier) -> bool {
        self.tiers.contains(&tier)
    }

    pub fn allows_category(&self, category: ControlCategory) -> bool {
        self.categories.contains(&category)
    }

    pub fn is_empty(&self) -> bool {
        self.tiers.is_empty() && self.categories.is_empty()
    }

    /// Widen this grant, held by a thread in `caller_project`, by everything `other` allows.
    pub fn merge(&mut self, other: &ControlGrant, caller_project: Option<&str>) {
        self.scope.widen(&other.scope, caller_project);
        self.tiers.extend(other.tiers.iter().copied());
        self.categories.extend(other.categories.iter().copied());
    }

    /// "observe, organize (project)" — how refusals and callbacks name a grant.
    pub fn describe(&self) -> String {
        let mut parts: Vec<&str> = self.tiers.iter().map(|tier| tier.name()).collect();
        parts.extend(self.categories.iter().map(|category| category.name()));
        if parts.is_empty() {
            format!("nothing ({})", self.scope.describe())
        } else {
            format!("{} ({})", parts.join(", "), self.scope.describe())
        }
    }
}

/// Where a thread's effective grant came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ControlGrantSource {
    Default,
    Project,
    Thread,
}

/// A grant the model asked for, awaiting the user's answer in Tcode's UI.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ControlGrantRequest {
    pub id: String,
    pub grant: ControlGrant,
    pub reason: String,
    /// Unix seconds.
    pub requested_at: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ControlSettings {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub default_grant: ControlGrant,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub project_overrides: BTreeMap<String, ControlGrant>,
}

const fn default_true() -> bool {
    true
}

impl Default for ControlSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            default_grant: ControlGrant::default_grant(),
            project_overrides: BTreeMap::new(),
        }
    }
}

impl ControlSettings {
    pub fn is_default(&self) -> bool {
        self == &Self::default()
    }

    /// A thread's effective grant: its own override, else its project's, else the default.
    pub fn resolve(
        &self,
        thread_override: Option<&ControlGrant>,
        project_id: Option<&str>,
    ) -> (ControlGrant, ControlGrantSource) {
        if let Some(grant) = thread_override {
            return (grant.clone(), ControlGrantSource::Thread);
        }
        if let Some(grant) = project_id.and_then(|id| self.project_overrides.get(id)) {
            return (grant.clone(), ControlGrantSource::Project);
        }
        (self.default_grant.clone(), ControlGrantSource::Default)
    }
}

/// How far Tcode goes on its own with settlement.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SettlementAutomation {
    Off,
    Suggest,
    #[default]
    ExplicitSignals,
}

impl SettlementAutomation {
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReferenceOrigin {
    User,
    Model,
}

/// Another thread this one may read, named by a chip or the `reference` tool.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThreadReference {
    pub thread_id: String,
    pub by: ReferenceOrigin,
    /// Unix seconds.
    pub created_at: u64,
    /// The reference crosses a project or provider boundary: the referenced thread's content
    /// may reach another model vendor. The user is told once when the reference is made,
    /// at the mention for a chip and by a runtime notice for the `reference` tool.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub boundary_notice: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FollowUp {
    /// Unix seconds.
    pub since: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SelfSettleWhen {
    EndOfTurn,
    PrMerged { url: String },
    PrClosed { url: String },
}

/// The user's consent, quoted, for the thread to settle itself.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SelfSettleGrant {
    pub when: SelfSettleWhen,
    pub consent_quote: String,
    /// Unix seconds.
    pub granted_at: u64,
}

/// The thread and idempotency key that started this thread through `start_thread`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StartedBy {
    pub thread_id: String,
    pub request_id: String,
}

#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Serialize,
    Deserialize,
    schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum WatchEvent {
    Idle,
    TurnCompleted,
    NeedsInput,
    NeedsApproval,
    Settled,
    PrChanged,
}

/// A wake the caller asked for on other threads' events.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ControlWatch {
    pub id: String,
    pub thread_ids: Vec<String>,
    pub events: BTreeSet<WatchEvent>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub once: bool,
}

/// The semantic keys a [`ControlAction`] records, shared by the host that writes them and the
/// clients that put them in words. A [`ControlAction::detail`] is data only (a title, an id,
/// a name, a count, the user's or the model's own words), never a sentence.
pub mod action {
    pub const SETTLE: &str = "settle";
    /// The thread settled itself under the user's consent; detail: the consent quote.
    pub const SETTLED_ITSELF: &str = "settled_itself";
    pub const UNSETTLE: &str = "unsettle";
    pub const ARCHIVE: &str = "archive";
    pub const UNARCHIVE: &str = "unarchive";
    pub const PIN: &str = "pin";
    pub const UNPIN: &str = "unpin";
    /// Detail: the new title.
    pub const RENAME: &str = "rename";
    pub const MARK_READ: &str = "mark_read";
    pub const MARK_UNREAD: &str = "mark_unread";
    /// Detail: the note, if any.
    pub const FOLLOW_UP: &str = "follow_up";
    pub const FOLLOW_UP_OFF: &str = "follow_up_off";
    pub const PARK: &str = "park";
    pub const PARK_OFF: &str = "park_off";
    pub const KEEP: &str = "keep";
    pub const KEEP_OFF: &str = "keep_off";
    /// Detail: the consent quote.
    pub const SETTLE_SELF: &str = "settle_self";
    /// Detail: the number of items.
    pub const RECORD_REVIEW: &str = "record_review";
    /// Detail: the model's reason.
    pub const REQUEST_GRANT: &str = "request_grant";
    /// Detail: the watch id.
    pub const NOTIFY_WHEN: &str = "notify_when";
    /// Detail: the removed watch ids.
    pub const CANCEL_NOTIFY: &str = "cancel_notify";
    /// Detail: the request id.
    pub const RESPOND_INPUT: &str = "respond_input";
    pub const TERMINAL_OPEN: &str = "terminal_open";
    /// Detail: the terminal id.
    pub const TERMINAL_INPUT: &str = "terminal_input";
    /// Detail: the terminal id.
    pub const TERMINAL_CLOSE: &str = "terminal_close";
    /// Detail: the deleted thread's title.
    pub const DELETE_THREADS: &str = "delete_threads";
    /// Detail: the turn id.
    pub const REWIND: &str = "rewind";
    /// Detail: the setting's key.
    pub const PATCH_SETTINGS: &str = "patch_settings";
    pub const START_THREAD: &str = "start_thread";
    /// Detail: the target project's name and root.
    pub const MOVE: &str = "move";
    pub const SEND_QUEUED: &str = "send_queued";
    pub const SEND_STEERED: &str = "send_steered";
    pub const SEND_SCHEDULED: &str = "send_scheduled";
    pub const INTERRUPT: &str = "interrupt";
    /// Detail: the turn id.
    pub const RETRY: &str = "retry";
    pub const COMPACT: &str = "compact";
    /// Detail: the model id.
    pub const SET_MODEL: &str = "set_model";
    /// Detail: the option id.
    pub const SET_OPTION: &str = "set_option";
    pub const FORK: &str = "fork";
    /// Detail: the queued message id.
    pub const STEER_QUEUED: &str = "steer_queued";
    /// Detail: the queued message id.
    pub const DROP_QUEUED: &str = "drop_queued";
    /// The pull request operations; detail: the pull request.
    pub const PR_LINK: &str = "pr_link";
    pub const PR_UNLINK: &str = "pr_unlink";
    pub const PR_WATCH: &str = "pr_watch";
    pub const PR_UNWATCH: &str = "pr_unwatch";
    pub const PR_REFRESH: &str = "pr_refresh";
    pub const PR_COMMENT: &str = "pr_comment";
    pub const PR_REPLY: &str = "pr_reply";
    pub const PR_RESOLVE: &str = "pr_resolve";
    /// Detail: the branch.
    pub const GIT_CHECKOUT: &str = "git_checkout";
    pub const GIT_MERGE_WORKTREE: &str = "git_merge_worktree";
    pub const GIT_COMMIT: &str = "git_commit";
    pub const GIT_COMMIT_PUSH: &str = "git_commit_push";
    pub const GIT_PUSH: &str = "git_push";
    pub const GIT_PULL: &str = "git_pull";
    pub const GIT_PUBLISH_BRANCH: &str = "git_publish_branch";
    pub const GIT_INITIALIZE: &str = "git_initialize";
    /// Detail: the project root.
    pub const CREATE_PROJECT: &str = "create_project";
    /// Detail: the referenced thread's title.
    pub const REFERENCE: &str = "reference";
}

/// One `tcode_control` write, as Settings → Control lists it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ControlActivityEntry {
    pub target_thread_id: String,
    pub target_title: String,
    /// Unix milliseconds.
    pub at: u64,
    pub action: ControlAction,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewItem {
    pub thread_id: String,
    pub group: String,
    /// In words: clear evidence, inference, or unsure.
    pub confidence: String,
    /// Anchors and quoted facts the group rests on.
    pub evidence: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub suggestion: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewRecord {
    pub id: String,
    /// The thread whose model recorded it.
    pub thread_id: String,
    /// Unix seconds.
    pub recorded_at: u64,
    /// What the review looked at, in its own words.
    pub coverage: String,
    pub items: Vec<ReviewItem>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_thread_override_beats_its_project_which_beats_the_default() {
        let mut settings = ControlSettings::default();
        let project = ControlGrant {
            scope: ControlScope::Host,
            ..ControlGrant::none()
        };
        settings
            .project_overrides
            .insert("p".into(), project.clone());
        let thread = ControlGrant {
            categories: [ControlCategory::Delete].into_iter().collect(),
            ..ControlGrant::default_grant()
        };
        assert_eq!(
            settings.resolve(None, None),
            (ControlGrant::default_grant(), ControlGrantSource::Default)
        );
        assert_eq!(
            settings.resolve(None, Some("p")),
            (project, ControlGrantSource::Project)
        );
        assert_eq!(
            settings.resolve(Some(&thread), Some("p")),
            (thread, ControlGrantSource::Thread)
        );
    }

    #[test]
    fn merging_an_approved_request_only_widens() {
        let mut grant = ControlGrant {
            scope: ControlScope::Projects(vec!["a".into()]),
            tiers: [ControlTier::Observe].into_iter().collect(),
            categories: BTreeSet::new(),
        };
        grant.merge(
            &ControlGrant {
                scope: ControlScope::Projects(vec!["b".into(), "a".into()]),
                tiers: [ControlTier::Operate].into_iter().collect(),
                categories: [ControlCategory::Terminal].into_iter().collect(),
            },
            Some("own"),
        );
        assert_eq!(
            grant.scope,
            ControlScope::Projects(vec!["a".into(), "b".into()])
        );
        assert!(grant.allows_tier(ControlTier::Observe) && grant.allows_tier(ControlTier::Operate));
        assert!(grant.allows_category(ControlCategory::Terminal));
        grant.merge(&ControlGrant::none(), Some("own"));
        assert_eq!(
            grant.scope,
            ControlScope::Projects(vec!["a".into(), "b".into(), "own".into()])
        );
        grant.merge(
            &ControlGrant {
                scope: ControlScope::Host,
                ..ControlGrant::none()
            },
            Some("own"),
        );
        assert_eq!(grant.scope, ControlScope::Host);

        // The default grant's own project survives approving another project.
        let mut grant = ControlGrant::default_grant();
        grant.merge(
            &ControlGrant {
                scope: ControlScope::Projects(vec!["b".into()]),
                ..ControlGrant::none()
            },
            Some("own"),
        );
        assert_eq!(
            grant.scope,
            ControlScope::Projects(vec!["own".into(), "b".into()])
        );
    }

    #[test]
    fn scope_reaches_targets_by_project() {
        assert!(ControlScope::Project.reaches(Some("a"), Some("a")));
        assert!(!ControlScope::Project.reaches(Some("a"), Some("b")));
        assert!(!ControlScope::Project.reaches(Some("a"), None));
        assert!(ControlScope::Projects(vec!["b".into()]).reaches(Some("a"), Some("b")));
        assert!(ControlScope::Projects(vec!["b".into()]).reaches(Some("a"), Some("a")));
        assert!(!ControlScope::Projects(vec!["b".into()]).reaches(Some("a"), Some("c")));
        assert!(!ControlScope::Projects(vec!["b".into()]).reaches(Some("a"), None));
        assert!(ControlScope::Host.reaches(Some("a"), None));
    }

    #[test]
    fn a_requested_scope_is_within_the_grant_only_when_the_grant_reaches_all_of_it() {
        let b = ControlScope::Projects(vec!["b".into()]);
        assert!(ControlScope::Project.within(&b, Some("a")));
        assert!(ControlScope::Projects(vec!["a".into(), "b".into()]).within(&b, Some("a")));
        assert!(!ControlScope::Projects(vec!["c".into()]).within(&b, Some("a")));
        assert!(ControlScope::Projects(vec!["a".into()]).within(&ControlScope::Project, Some("a")));
        assert!(!b.within(&ControlScope::Project, Some("a")));
        assert!(!ControlScope::Host.within(&b, Some("a")));
        assert!(ControlScope::Host.within(&ControlScope::Host, Some("a")));
    }
}
