//! In-process MCP server through which a thread's model observes and operates Tcode,
//! within the grant the user gave that thread.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tcode_core::control::{ControlCategory, ControlGrant, ControlTier};

pub mod params;
mod tools;

pub use params::*;
pub use tools::{INSTRUCTIONS, tool_descriptions};

/// One call of a `tcode_control` tool, with its parameters.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ControlOp {
    ListProjects(ListProjectsParams),
    ListThreads(ListThreadsParams),
    GetThread(GetThreadParams),
    Search(SearchParams),
    Outline(OutlineParams),
    Read(ReadParams),
    SearchInItem(SearchInItemParams),
    FileHistory(FileHistoryParams),
    GetGit(GetGitParams),
    GetPullRequest(GetPullRequestParams),
    ListProviders(ListProvidersParams),
    GetSettings(GetSettingsParams),
    ReadTerminal(ReadTerminalParams),
    ReviewCandidates(ReviewCandidatesParams),
    UpdateThreads(UpdateThreadsParams),
    RecordReview(RecordReviewParams),
    SettleSelf(SettleSelfParams),
    StartThread(StartThreadParams),
    SendMessage(SendMessageParams),
    ControlThread(ControlThreadParams),
    PullRequest(PullRequestParams),
    Git(GitParams),
    CreateProject(CreateProjectParams),
    MoveThread(MoveThreadParams),
    NotifyWhen(NotifyWhenParams),
    CancelNotify(CancelNotifyParams),
    Reference(ReferenceParams),
    RespondInput(RespondInputParams),
    Terminal(TerminalParams),
    DeleteThreads(DeleteThreadsParams),
    Rewind(RewindParams),
    PatchSettings(PatchSettingsParams),
    RequestGrant(RequestGrantParams),
}

/// What a grant must hold for a tool to be offered and its calls accepted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolRequirement {
    Tier(ControlTier),
    Category(ControlCategory),
    /// `request_grant`: offered whatever the grant, it only asks the user.
    Always,
}

impl ToolRequirement {
    pub fn allowed_by(self, grant: &ControlGrant) -> bool {
        match self {
            Self::Tier(tier) => grant.allows_tier(tier),
            Self::Category(category) => grant.allows_category(category),
            Self::Always => true,
        }
    }
}

/// Tool name → requirement.
pub const TOOLS: [(&str, ToolRequirement); 33] = [
    ("list_projects", ToolRequirement::Tier(ControlTier::Observe)),
    ("list_threads", ToolRequirement::Tier(ControlTier::Observe)),
    ("get_thread", ToolRequirement::Tier(ControlTier::Observe)),
    ("search", ToolRequirement::Tier(ControlTier::Observe)),
    ("outline", ToolRequirement::Tier(ControlTier::Observe)),
    ("read", ToolRequirement::Tier(ControlTier::Observe)),
    (
        "search_in_item",
        ToolRequirement::Tier(ControlTier::Observe),
    ),
    ("file_history", ToolRequirement::Tier(ControlTier::Observe)),
    ("get_git", ToolRequirement::Tier(ControlTier::Observe)),
    (
        "get_pull_request",
        ToolRequirement::Tier(ControlTier::Observe),
    ),
    (
        "list_providers",
        ToolRequirement::Tier(ControlTier::Observe),
    ),
    ("get_settings", ToolRequirement::Tier(ControlTier::Observe)),
    ("read_terminal", ToolRequirement::Tier(ControlTier::Observe)),
    (
        "review_candidates",
        ToolRequirement::Tier(ControlTier::Observe),
    ),
    // Referencing only makes a thread in the observe scope readable later; it writes nothing.
    ("reference", ToolRequirement::Tier(ControlTier::Observe)),
    (
        "update_threads",
        ToolRequirement::Tier(ControlTier::Organize),
    ),
    (
        "record_review",
        ToolRequirement::Tier(ControlTier::Organize),
    ),
    ("settle_self", ToolRequirement::Tier(ControlTier::Organize)),
    ("start_thread", ToolRequirement::Tier(ControlTier::Operate)),
    ("send_message", ToolRequirement::Tier(ControlTier::Operate)),
    (
        "control_thread",
        ToolRequirement::Tier(ControlTier::Operate),
    ),
    ("pull_request", ToolRequirement::Tier(ControlTier::Operate)),
    ("git", ToolRequirement::Tier(ControlTier::Operate)),
    (
        "create_project",
        ToolRequirement::Tier(ControlTier::Operate),
    ),
    ("move_thread", ToolRequirement::Tier(ControlTier::Operate)),
    ("notify_when", ToolRequirement::Tier(ControlTier::Operate)),
    ("cancel_notify", ToolRequirement::Tier(ControlTier::Operate)),
    (
        "respond_input",
        ToolRequirement::Category(ControlCategory::AnswerInput),
    ),
    (
        "terminal",
        ToolRequirement::Category(ControlCategory::Terminal),
    ),
    (
        "delete_threads",
        ToolRequirement::Category(ControlCategory::Delete),
    ),
    (
        "rewind",
        ToolRequirement::Category(ControlCategory::HistoryEdit),
    ),
    (
        "patch_settings",
        ToolRequirement::Category(ControlCategory::SettingsWrite),
    ),
    ("request_grant", ToolRequirement::Always),
];

pub fn tool_requirement(name: &str) -> Option<ToolRequirement> {
    TOOLS
        .iter()
        .find(|(tool, _)| *tool == name)
        .map(|(_, requirement)| *requirement)
}

impl ControlOp {
    pub fn tool_name(&self) -> &'static str {
        match self {
            Self::ListProjects(_) => "list_projects",
            Self::ListThreads(_) => "list_threads",
            Self::GetThread(_) => "get_thread",
            Self::Search(_) => "search",
            Self::Outline(_) => "outline",
            Self::Read(_) => "read",
            Self::SearchInItem(_) => "search_in_item",
            Self::FileHistory(_) => "file_history",
            Self::GetGit(_) => "get_git",
            Self::GetPullRequest(_) => "get_pull_request",
            Self::ListProviders(_) => "list_providers",
            Self::GetSettings(_) => "get_settings",
            Self::ReadTerminal(_) => "read_terminal",
            Self::ReviewCandidates(_) => "review_candidates",
            Self::UpdateThreads(_) => "update_threads",
            Self::RecordReview(_) => "record_review",
            Self::SettleSelf(_) => "settle_self",
            Self::StartThread(_) => "start_thread",
            Self::SendMessage(_) => "send_message",
            Self::ControlThread(_) => "control_thread",
            Self::PullRequest(_) => "pull_request",
            Self::Git(_) => "git",
            Self::CreateProject(_) => "create_project",
            Self::MoveThread(_) => "move_thread",
            Self::NotifyWhen(_) => "notify_when",
            Self::CancelNotify(_) => "cancel_notify",
            Self::Reference(_) => "reference",
            Self::RespondInput(_) => "respond_input",
            Self::Terminal(_) => "terminal",
            Self::DeleteThreads(_) => "delete_threads",
            Self::Rewind(_) => "rewind",
            Self::PatchSettings(_) => "patch_settings",
            Self::RequestGrant(_) => "request_grant",
        }
    }

    pub fn requirement(&self) -> ToolRequirement {
        tool_requirement(self.tool_name()).expect("every op is a listed tool")
    }

    /// Categories the call's data needs beyond the tool's own requirement.
    pub fn extra_categories(&self) -> Vec<ControlCategory> {
        match self {
            Self::PullRequest(p) if p.op.writes() => vec![ControlCategory::PrWrite],
            Self::Git(p) if p.op.remote() => vec![ControlCategory::GitRemote],
            _ => Vec::new(),
        }
    }

    /// The scope the call asks for, where a tool takes one.
    pub fn requested_scope(&self) -> Option<&tcode_core::control::ControlScope> {
        match self {
            Self::ListThreads(p) => p.scope.as_ref(),
            Self::Search(p) => p.scope.as_ref(),
            Self::ReviewCandidates(p) => p.scope.as_ref(),
            _ => None,
        }
    }

    /// Whether the call answers per target, so an unknown or out-of-scope target skips
    /// that item instead of refusing the whole call.
    pub fn answers_per_target(&self) -> bool {
        matches!(
            self,
            Self::UpdateThreads(_)
                | Self::SendMessage(_)
                | Self::ControlThread(_)
                | Self::DeleteThreads(_)
                | Self::NotifyWhen(_)
        )
    }

    /// Threads the call acts on or reads, as named in its parameters.
    pub fn target_thread_ids(&self) -> Vec<&str> {
        match self {
            Self::GetThread(p) => vec![p.thread_id.as_str()],
            Self::Search(p) => p.thread_ids.iter().flatten().map(String::as_str).collect(),
            Self::Outline(p) => vec![p.thread_id.as_str()],
            Self::Read(p) => p
                .anchors
                .iter()
                .flatten()
                .map(String::as_str)
                .chain(p.from.as_deref())
                .chain(p.to.as_deref())
                .filter_map(|anchor| parse_anchor(anchor).map(|(thread, _)| thread))
                .chain(
                    p.cursor
                        .as_deref()
                        .and_then(|cursor| cursor.rsplit_once('#'))
                        .map(|(thread, _)| thread),
                )
                .collect(),
            Self::SearchInItem(p) => parse_anchor(&p.anchor)
                .map(|(thread, _)| vec![thread])
                .unwrap_or_default(),
            Self::FileHistory(p) => vec![p.thread_id.as_str()],
            Self::GetGit(p) => p.thread_id.iter().map(String::as_str).collect(),
            Self::GetPullRequest(p) => vec![p.thread_id.as_str()],
            Self::UpdateThreads(p) => p.changes.iter().map(|c| c.thread_id.as_str()).collect(),
            Self::SendMessage(p) => p.thread_ids.iter().map(String::as_str).collect(),
            Self::ControlThread(p) => p.changes.iter().map(|c| c.thread_id.as_str()).collect(),
            Self::PullRequest(p) => vec![p.thread_id.as_str()],
            Self::Git(p) => vec![p.thread_id.as_str()],
            Self::MoveThread(p) => vec![p.thread_id.as_str()],
            Self::NotifyWhen(p) => p.thread_ids.iter().map(String::as_str).collect(),
            Self::Reference(p) => vec![p.thread_id.as_str()],
            Self::RespondInput(p) => vec![p.thread_id.as_str()],
            Self::Terminal(p) => vec![p.thread_id.as_str()],
            Self::DeleteThreads(p) => p.thread_ids.iter().map(String::as_str).collect(),
            Self::Rewind(p) => vec![p.thread_id.as_str()],
            Self::ListProjects(_)
            | Self::ListThreads(_)
            | Self::ListProviders(_)
            | Self::GetSettings(_)
            | Self::ReadTerminal(_)
            | Self::ReviewCandidates(_)
            | Self::RecordReview(_)
            | Self::SettleSelf(_)
            | Self::StartThread(_)
            | Self::CreateProject(_)
            | Self::CancelNotify(_)
            | Self::PatchSettings(_)
            | Self::RequestGrant(_) => Vec::new(),
        }
    }

    /// Reads of a thread's content or state, allowed also on the caller itself, its
    /// references and its orchestrated children.
    pub fn reads_content(&self) -> bool {
        matches!(
            self,
            Self::NotifyWhen(_)
                | Self::GetThread(_)
                | Self::Search(_)
                | Self::Outline(_)
                | Self::Read(_)
                | Self::SearchInItem(_)
                | Self::FileHistory(_)
        )
    }
}

#[derive(Debug)]
pub struct BrokerRequest {
    /// The calling thread, baked into its bearer token.
    pub thread_id: String,
    pub op: ControlOp,
    pub reply: async_channel::Sender<Result<serde_json::Value, String>>,
}

pub type Broker = mcp_host::Broker<BrokerRequest>;
pub type TokenRegistry = mcp_host::TokenRegistry<tools::Service>;

/// Answers a thread id with its current effective grant; `None` for a thread the host
/// does not know, which gets `request_grant` alone.
pub type GrantResolver = Arc<dyn Fn(&str) -> Option<ControlGrant> + Send + Sync>;

/// Tool names with the descriptions a connection listed them with.
pub type ListedTools = Vec<(String, String)>;

/// The resolver the runtime installs when it attaches the server, and the tools each
/// thread's latest connection was actually given. Until a resolver is set, and for a thread
/// it does not know, every connection sees the empty grant.
#[derive(Clone, Default)]
pub struct GrantSource {
    resolver: Arc<Mutex<Option<GrantResolver>>>,
    offered: Arc<Mutex<HashMap<String, ListedTools>>>,
}

impl GrantSource {
    pub fn set(&self, resolver: GrantResolver) {
        *self.resolver.lock().unwrap() = Some(resolver);
    }

    pub fn resolve(&self, thread_id: &str) -> ControlGrant {
        let resolver = self.resolver.lock().unwrap().clone();
        resolver
            .and_then(|resolve| resolve(thread_id))
            .unwrap_or_else(ControlGrant::none)
    }

    /// The tools, with descriptions, the thread's latest connection listed; `None` before
    /// its provider first connected. A grant changed since then shows here only once the
    /// provider connects again.
    pub fn offered(&self, thread_id: &str) -> Option<ListedTools> {
        self.offered.lock().unwrap().get(thread_id).cloned()
    }

    pub fn forget(&self, thread_id: &str) {
        self.offered.lock().unwrap().remove(thread_id);
    }

    fn record_offered(&self, thread_id: &str, tools: ListedTools) {
        self.offered
            .lock()
            .unwrap()
            .insert(thread_id.to_owned(), tools);
    }
}

pub struct ControlMcpServer {
    pub url: String,
    pub tokens: TokenRegistry,
    pub requests: async_channel::Receiver<BrokerRequest>,
    pub grants: GrantSource,
}

pub fn start(host: &mut mcp_host::Host) -> ControlMcpServer {
    let url = host.url("/control");
    let (req_tx, req_rx) = async_channel::unbounded();
    let broker = Broker::new(
        req_tx,
        Duration::from_secs(60),
        mcp_host::BrokerErrors {
            unavailable: "tcode control is not available",
            dropped: "tcode control dropped the request",
            timed_out: "tcode control operation timed out",
        },
    );
    let grants = GrantSource::default();
    let resolver = grants.clone();
    let tokens = TokenRegistry::new(move |thread_id| {
        tools::service(broker.clone(), thread_id, resolver.clone())
    });
    host.mount(mcp_host::route("/control", &tokens));
    log::info!("control-mcp: serving at {url}");
    ControlMcpServer {
        url,
        tokens,
        requests: req_rx,
        grants,
    }
}
