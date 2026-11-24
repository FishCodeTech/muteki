use super::*;
use crate::app::snapshots::ActivityProjection;
use control_mcp::{
    GetGitParams, GetPullRequestParams, GetSettingsParams, GetThreadParams, GitWhat, ItemKind,
    ListProjectsParams, ListProvidersParams, ListThreadsParams, ReadTerminalParams,
    ReviewCandidatesParams, SearchParams, ThreadState, settlement_reason as reason,
};
use tcode_core::control::{FollowUp, SettlementAutomation, StartedBy};
use tcode_core::pull_request::{PullRequestKey, PullRequestState, ThreadPullRequestLink};
use tcode_core::settings::provider_key;
use tcode_services::session_search::{SearchFilter, SearchKind};
use tcode_services::thread_index::{fence, redact};

const PAGE_LIMIT: u32 = 50;
const PAGE_CAP: u32 = 100;
const SEARCH_LIMIT: u32 = 20;
const SEARCH_CAP: u32 = 50;
const PER_TURN_CAP: usize = 200;
/// How many collapsed threads, and how many left-out thread ids, a review lists by name.
const REVIEW_LIST_CAP: usize = 100;
/// What one response may carry, about the size of one large file.
const RESPONSE_BUDGET: usize = 60 * 1024;
const TERMINAL_TAIL_BYTES: usize = 32 * 1024;
const ASSISTANT_HEAD_CHARS: usize = 300;
/// A thread active this recently is still in progress for a review, whatever its run state.
const JUST_ACTIVE_MS: u64 = 10 * 60 * 1000;
const REVIEW_WINDOW_MS: u64 = 7 * DAY_MS;
const DAY_MS: u64 = 86_400_000;

#[derive(Debug, Clone, Serialize)]
struct PullRequestRow {
    key: String,
    url: String,
    state: Option<PullRequestState>,
}

#[derive(Debug, Clone, Serialize)]
struct WorktreeFacts {
    cwd: PathBuf,
    /// `dedicated` for a worktree Tcode created for the thread, else `checkout`.
    kind: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    branch: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    base: Option<String>,
    /// Uncommitted changes right now; `None` outside a git repository or when not read.
    dirty: Option<bool>,
}

#[derive(Debug, Clone, Serialize)]
struct ThreadRow {
    id: String,
    title: String,
    project_id: Option<String>,
    provider: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    profile_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    acp_agent_id: Option<String>,
    model: Option<String>,
    state: ThreadState,
    settled_at: Option<u64>,
    archived_at: Option<u64>,
    pinned: bool,
    follow_up: Option<FollowUp>,
    parked: bool,
    last_activity_at: Option<u64>,
    last_human_message_at: Option<u64>,
    parent_session_id: Option<String>,
    pull_requests: Vec<PullRequestRow>,
    started_by: Option<StartedBy>,
    #[serde(skip_serializing_if = "Option::is_none")]
    worktree: Option<WorktreeFacts>,
}

#[derive(Debug, Serialize)]
struct ThreadPage {
    threads: Vec<ThreadRow>,
    total: usize,
    next_cursor: Option<String>,
    omitted: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
struct SettlementRuleFacts {
    after_days: Option<f64>,
    /// `project` when the thread's project overrides the setting, else `settings`.
    after_days_source: &'static str,
    on_merge: bool,
    on_merge_source: &'static str,
}

#[derive(Debug, Clone, Serialize)]
pub(super) struct SettlementExplanation {
    would_auto_settle: bool,
    /// Unix ms at which automatic settlement would settle the thread.
    due_at: Option<u64>,
    /// Every fact that keeps automatic settlement from applying, as `settlement_reason` keys.
    reasons: Vec<&'static str>,
    rule: SettlementRuleFacts,
}

#[derive(Debug, Default, Serialize)]
struct QueueCounts {
    human: usize,
    agent: usize,
    server: usize,
    scheduled: usize,
}

#[derive(Debug, Serialize)]
struct PendingApproval {
    id: String,
    kind: &'static str,
}

#[derive(Debug, Serialize)]
struct PendingInput {
    request_id: String,
    questions: usize,
}

#[derive(Debug, Serialize)]
struct Lineage {
    parent_session_id: Option<String>,
    children: Vec<String>,
    pending_fork: bool,
    imported_from: Option<String>,
    native_subagent: bool,
}

#[derive(Debug, Serialize)]
struct TerminalRow {
    id: u64,
    label: String,
}

#[derive(Debug, Serialize)]
struct ThreadDetail {
    #[serde(flatten)]
    thread: ThreadRow,
    settlement: SettlementExplanation,
    activity: SessionActivity,
    queue: QueueCounts,
    pending_approvals: Vec<PendingApproval>,
    pending_input: Option<PendingInput>,
    control: Option<tcode_protocol::ControlStatus>,
    lineage: Lineage,
    terminals: Vec<TerminalRow>,
    omitted: Vec<String>,
}

#[derive(Debug, Serialize)]
struct SearchHitRow {
    anchor: String,
    thread_id: String,
    title: String,
    turn: usize,
    kind: SearchKind,
    snippet: String,
}

#[derive(Debug, Serialize)]
struct ThreadCount {
    thread_id: String,
    hits: usize,
}

#[derive(Debug, Serialize)]
struct TurnCount {
    thread_id: String,
    turn: usize,
    hits: usize,
}

#[derive(Debug, Serialize)]
struct SearchAnswer {
    query: String,
    hits: Vec<SearchHitRow>,
    per_thread: Vec<ThreadCount>,
    per_turn: Vec<TurnCount>,
    total: usize,
    truncated: bool,
    next_cursor: Option<String>,
    redacted: usize,
    omitted: Vec<String>,
}

#[derive(Debug, Serialize)]
struct DiffFile {
    path: String,
    kind: agent::FileChangeKind,
}

#[derive(Debug, Default, Serialize)]
struct GitAnswer {
    #[serde(skip_serializing_if = "Option::is_none")]
    thread_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    project_id: Option<String>,
    cwd: PathBuf,
    #[serde(skip_serializing_if = "Option::is_none")]
    status: Option<GitStatus>,
    #[serde(skip_serializing_if = "Option::is_none")]
    files: Option<Vec<DiffFile>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    diff: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    branches: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    current_branch: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    redacted: usize,
    omitted: Vec<String>,
}

#[derive(Debug, Serialize)]
struct PullRequestAnswer<'a> {
    key: String,
    pull_request: &'a ThreadPullRequestLink,
    omitted: Vec<String>,
}

#[derive(Debug, Serialize)]
struct GoalFacts {
    pause: bool,
    live: bool,
}

#[derive(Debug, Serialize)]
struct CapsFacts {
    steering: bool,
    fork: bool,
    native_rewind: bool,
    manual_compaction: bool,
    goals: Option<GoalFacts>,
    per_turn_effort: bool,
    /// MCP servers, and so `tcode_control`, reach the provider's sessions.
    mcp_servers: bool,
    native_computer_use: bool,
}

#[derive(Debug, Serialize)]
struct ModelRow {
    id: String,
    display_name: String,
    is_default: bool,
}

#[derive(Debug, Serialize)]
struct ProfileRow {
    id: String,
    display_name: String,
    enabled: bool,
    status: Option<ProviderSnapshot>,
    usage: Option<tcode_core::usage::ProviderUsage>,
    /// Names only: values are never returned.
    env_names: Vec<String>,
    custom_models: Vec<String>,
    hidden_models: Vec<String>,
}

#[derive(Debug, Serialize)]
struct AcpAgentRow {
    id: String,
    name: String,
    version: String,
    enabled: bool,
}

#[derive(Debug, Serialize)]
struct ProviderRow {
    kind: &'static str,
    caps: CapsFacts,
    version: Option<ProtocolProviderVersionStatus>,
    models: Vec<ModelRow>,
    models_loading: bool,
    profiles: Vec<ProfileRow>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    acp_agents: Vec<AcpAgentRow>,
}

#[derive(Debug, Serialize)]
struct TerminalAnswer {
    terminal_id: u64,
    thread_id: String,
    title: String,
    exited: bool,
    exit_code: Option<i32>,
    text: String,
    redacted: usize,
    omitted: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
struct LastTurn {
    status: Option<TurnStatus>,
    /// The head of the latest assistant message, redacted and fenced as data.
    assistant_head: Option<String>,
}

#[derive(Debug, Serialize)]
struct ReviewCandidate {
    #[serde(flatten)]
    thread: ThreadRow,
    settlement: SettlementExplanation,
    last_turn: Option<LastTurn>,
    pending_approval: bool,
    pending_input: bool,
    /// The thread is held active by a manual unsettle (or by pinning it while settled).
    manual_unsettle: bool,
    /// Whole days since the last activity, for an unsettled thread.
    idle_days: Option<u64>,
}

#[derive(Debug, Serialize)]
struct CollapsedRow {
    id: String,
    title: String,
    /// `running`, `just_active` or `orchestrate_child`.
    reason: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    parent_session_id: Option<String>,
}

#[derive(Debug, Serialize)]
struct Coverage {
    projects: usize,
    threads: usize,
    without_enough_data: usize,
    /// Settled threads were included only when active at or after this unix ms.
    window_since: u64,
}

#[derive(Debug, Serialize)]
struct UnattributedWorktree {
    project_id: String,
    root: PathBuf,
}

#[derive(Debug, Serialize)]
struct ReviewAnswer {
    candidates: Vec<ReviewCandidate>,
    collapsed: Vec<CollapsedRow>,
    unattributed_dirty_worktrees: Vec<UnattributedWorktree>,
    coverage: Coverage,
    redacted: usize,
    omitted: Vec<String>,
}

fn to_value(value: &impl Serialize) -> Result<serde_json::Value, String> {
    serde_json::to_value(value).map_err(|error| error.to_string())
}

/// How a pull request is named to the model: `host/repository#number`.
pub(super) fn pull_request_label(key: &PullRequestKey) -> String {
    format!("{}/{}#{}", key.host, key.repository, key.number)
}

/// The visible link a model names by its label or URL.
pub(super) fn find_pull_request<'a>(
    meta: &'a SessionMeta,
    key: &str,
) -> Option<&'a ThreadPullRequestLink> {
    let key = key.trim();
    meta.pull_requests
        .iter()
        .filter(|link| link.visible())
        .find(|link| pull_request_label(&link.key) == key || link.url == key)
}

fn thread_state(meta: &SessionMeta) -> ThreadState {
    if meta.archived_at.is_some() {
        ThreadState::Archived
    } else if meta.is_settled() {
        ThreadState::Settled
    } else {
        ThreadState::Active
    }
}

fn worktree_facts(meta: &SessionMeta) -> WorktreeFacts {
    WorktreeFacts {
        cwd: meta.cwd.clone(),
        kind: if meta.worktree.is_some() {
            "dedicated"
        } else {
            "checkout"
        },
        branch: meta.worktree.as_ref().map(|w| w.branch.clone()),
        base: meta.worktree.as_ref().map(|w| w.base.clone()),
        dirty: None,
    }
}

/// Live uncommitted changes in `cwd`, from the owner of the GitStatus topic.
fn dirty_state(cwd: &Path) -> Option<bool> {
    let status = tcode_services::git::read_status(cwd);
    status.is_repo.then_some(status.has_working_tree_changes)
}

fn dirty_states(cwds: impl IntoIterator<Item = PathBuf>) -> HashMap<PathBuf, Option<bool>> {
    let mut states = HashMap::new();
    for cwd in cwds {
        if let std::collections::hash_map::Entry::Vacant(entry) = states.entry(cwd) {
            let dirty = dirty_state(entry.key());
            entry.insert(dirty);
        }
    }
    states
}

/// `"<activity>:<id>"`: the last row of a page in `list_threads` order.
fn parse_cursor(cursor: &str) -> Result<(u64, &str), String> {
    cursor
        .split_once(':')
        .and_then(|(at, id)| Some((at.parse().ok()?, id)))
        .ok_or_else(|| format!("invalid cursor: {cursor}"))
}

/// The last `max` bytes of `text`, starting at a line where one starts within them.
fn tail(text: &str, max: usize) -> (&str, bool) {
    if text.len() <= max {
        return (text, false);
    }
    let mut start = text.len() - max;
    while !text.is_char_boundary(start) {
        start += 1;
    }
    if let Some(line) = text[start..].find('\n') {
        start += line + 1;
    }
    (&text[start..], true)
}

fn head_chars(text: &str, max: usize) -> String {
    let mut head: String = text.chars().take(max).collect();
    if head.len() < text.len() {
        head.push('…');
    }
    head
}

fn approval_kind(kind: &agent::ApprovalKind) -> &'static str {
    match kind {
        agent::ApprovalKind::ExecCommand { .. } => "exec_command",
        agent::ApprovalKind::FileRead { .. } => "file_read",
        agent::ApprovalKind::FileChange { .. } => "file_change",
        agent::ApprovalKind::ToolUse { .. } => "tool_use",
    }
}

/// The latest turn's status and assistant message, from a timeline holding at least that turn.
fn last_turn(thread_id: &str, timeline: &Timeline) -> (LastTurn, usize) {
    let message = timeline
        .entries
        .iter()
        .rev()
        .find_map(|entry| match &entry.content {
            EntryContent::Item(ItemContent::AssistantMessage { text }) => Some(text.as_str()),
            _ => None,
        });
    let (assistant_head, redacted) = match message {
        Some(text) => {
            let (text, redacted) = redact(text);
            let head = head_chars(&text, ASSISTANT_HEAD_CHARS);
            (
                Some(fence(&format!("{thread_id} last assistant message"), &head)),
                redacted,
            )
        }
        None => (None, 0),
    };
    (
        LastTurn {
            status: timeline.last_turn_status,
            assistant_head,
        },
        redacted,
    )
}

/// The stored latest turn of a thread that is not resident, folded from its rows alone.
fn stored_last_turn(store: &SessionStore, thread_id: &str) -> Option<(LastTurn, usize)> {
    let start = *store.turn_index(thread_id).ok()??.starts.last()?;
    let log = store.read_rows(thread_id, start..u64::MAX).ok()?;
    let timeline = Timeline::fold_stored(&log.records);
    Some(last_turn(thread_id, &timeline))
}

fn search_kinds(kinds: &[ItemKind], omitted: &mut Vec<String>) -> Vec<SearchKind> {
    let mut mapped = Vec::new();
    for kind in kinds {
        let kind = match kind {
            ItemKind::UserMessage => SearchKind::UserMessage,
            ItemKind::AssistantMessage => SearchKind::AssistantMessage,
            ItemKind::Command => SearchKind::CommandExecution,
            ItemKind::FileChange => SearchKind::FileChange,
            ItemKind::ToolCall => SearchKind::ToolCall,
            ItemKind::Steer => SearchKind::Steer,
            ItemKind::Subagent | ItemKind::ImageRead | ItemKind::WebSearch | ItemKind::Other => {
                SearchKind::Other
            }
            ItemKind::Reasoning => {
                omitted.push("reasoning is not indexed and was not searched".into());
                continue;
            }
        };
        if !mapped.contains(&kind) {
            mapped.push(kind);
        }
    }
    if mapped.contains(&SearchKind::Other) {
        omitted.push(
            "subagent, image_read, web_search and other items are indexed as one kind, so asking \
             for any of them matches all four"
                .into(),
        );
    }
    mapped
}

impl AppState {
    pub(super) fn op_list_projects(
        &mut self,
        caller: &Caller,
        _params: ListProjectsParams,
        _cx: &mut HostCx,
    ) -> Result<serde_json::Value, String> {
        let projects: Vec<serde_json::Value> = self
            .projects
            .iter()
            .filter(|project| {
                caller
                    .grant
                    .scope
                    .reaches(caller.project_id.as_deref(), Some(&project.id))
            })
            .map(|project| {
                let (mut active, mut settled, mut archived) = (0, 0, 0);
                for meta in self
                    .sessions
                    .iter()
                    .filter(|meta| meta.project_id.as_deref() == Some(project.id.as_str()))
                {
                    match thread_state(meta) {
                        ThreadState::Archived => archived += 1,
                        ThreadState::Settled => settled += 1,
                        ThreadState::Active => active += 1,
                    }
                }
                serde_json::json!({
                    "id": project.id,
                    "name": project.name,
                    "root": project.root,
                    "quick_chat": project.quick_chat,
                    "threads": { "active": active, "settled": settled, "archived": archived },
                })
            })
            .collect();
        Ok(serde_json::json!({ "projects": projects }))
    }

    /// The time a thread is ordered and filtered by: its last activity, else its creation.
    fn activity_at(&self, meta: &SessionMeta) -> u64 {
        self.thread_activity
            .get(&meta.id)
            .and_then(|activity| activity.last_activity_at())
            .unwrap_or(meta.created_at.saturating_mul(1000))
    }

    fn thread_row(&self, meta: &SessionMeta) -> ThreadRow {
        let activity = self.thread_activity.get(&meta.id);
        ThreadRow {
            id: meta.id.clone(),
            title: meta.title.clone(),
            project_id: meta.project_id.clone(),
            provider: provider_key(meta.provider),
            profile_id: meta.profile_id.clone(),
            acp_agent_id: meta.acp_agent_id.clone(),
            model: meta.model.clone(),
            state: thread_state(meta),
            settled_at: meta.settled_at,
            archived_at: meta.archived_at,
            pinned: meta.pinned_at.is_some(),
            follow_up: meta.follow_up.clone(),
            parked: meta.parked_at.is_some(),
            last_activity_at: activity.and_then(|activity| activity.last_activity_at()),
            last_human_message_at: activity.and_then(|activity| activity.last_human_message_at),
            parent_session_id: meta.parent_session_id.clone(),
            pull_requests: meta
                .pull_requests
                .iter()
                .filter(|link| link.visible())
                .map(|link| PullRequestRow {
                    key: pull_request_label(&link.key),
                    url: link.url.clone(),
                    state: link.snapshot.as_ref().map(|snapshot| snapshot.state),
                })
                .collect(),
            started_by: meta.started_by.clone(),
            worktree: None,
        }
    }

    /// Why automatic settlement does or does not apply to a thread, from the same holds,
    /// rule and due time the settlement evaluator uses, plus the automatic-only protections.
    pub(super) fn settlement_explanation(
        &self,
        meta: &SessionMeta,
        now: u64,
        projection: &ActivityProjection<'_>,
    ) -> SettlementExplanation {
        let activity = self.thread_activity.get(&meta.id);
        let last_activity = activity.and_then(|activity| activity.last_activity_at());
        let automation = self.settings.settlement_automation;
        let tcode_core::settlement::SettlementBlockers {
            pending_approval,
            pending_input,
            live_run,
            background_work,
            unsettled_children,
            pull_request_watches,
            pending_human_message,
            scheduled_wake,
            pending_agent_message,
        } = self.settlement_blockers(meta, projection);
        let mut reasons: Vec<&'static str> = [
            (meta.archived_at.is_some(), reason::ARCHIVED),
            (
                meta.archived_at.is_none() && meta.is_settled(),
                reason::SETTLED,
            ),
            (
                meta.settled_override == Some(SettledOverride::Active),
                reason::OVERRIDE_ACTIVE,
            ),
            (meta.auto_settle_disabled_at.is_some(), reason::KEEP),
            (meta.pinned_at.is_some(), reason::PINNED),
            (meta.parent_session_id.is_some(), reason::ORCHESTRATE_CHILD),
            (
                automation == SettlementAutomation::Off,
                reason::AUTOMATION_OFF,
            ),
            (
                automation == SettlementAutomation::Suggest,
                reason::AUTOMATION_SUGGEST_ONLY,
            ),
            (meta.follow_up.is_some(), reason::FOLLOW_UP),
            (meta.parked_at.is_some(), reason::PARKED),
            (
                meta.settle_suggestion_dismissed_at
                    .is_some_and(|dismissed| {
                        last_activity.is_none_or(|at| dismissed.saturating_mul(1000) >= at)
                    }),
                reason::SUGGESTION_DISMISSED,
            ),
            (pending_agent_message, reason::PENDING_AGENT_MESSAGE),
            (pending_approval, reason::PENDING_APPROVAL),
            (pending_input, reason::PENDING_INPUT),
            (live_run, reason::LIVE_RUN),
            (background_work, reason::BACKGROUND_WORK),
            (unsettled_children, reason::UNSETTLED_CHILDREN),
            (pull_request_watches, reason::PULL_REQUEST_WATCHES),
            (pending_human_message, reason::PENDING_HUMAN_MESSAGE),
            (scheduled_wake, reason::SCHEDULED_WAKE),
        ]
        .into_iter()
        .filter_map(|(holds, key)| holds.then_some(key))
        .collect();
        let rule = self.settlement_rule(meta);
        let due = self.settlement_due(meta, now, projection);
        if reasons.is_empty() && due.is_none() {
            reasons.push(if activity.is_none() {
                reason::ACTIVITY_UNKNOWN
            } else if rule.after_days_from_project {
                reason::PROJECT_TIME_RULE_OFF
            } else {
                reason::TIME_RULE_OFF
            });
        }
        let source = |from_project: bool| if from_project { "project" } else { "settings" };
        let would_auto_settle = reasons.is_empty();
        SettlementExplanation {
            would_auto_settle,
            due_at: due.filter(|_| would_auto_settle).map(|due| due.due_at),
            reasons,
            rule: SettlementRuleFacts {
                after_days: rule.after_days,
                after_days_source: source(rule.after_days_from_project),
                on_merge: rule.on_merge,
                on_merge_source: source(rule.on_merge_from_project),
            },
        }
    }

    pub(super) fn op_list_threads(
        &mut self,
        caller: &Caller,
        params: ListThreadsParams,
        cx: &mut HostCx,
    ) -> Answer {
        let scope = params
            .scope
            .clone()
            .unwrap_or_else(|| caller.grant.scope.clone());
        let states = params
            .states
            .clone()
            .unwrap_or_else(|| vec![ThreadState::Active, ThreadState::Settled]);
        let title_query = params.title_query.as_deref().map(str::to_lowercase);
        let include_children = params.include_children.unwrap_or(false);
        let include_worktree = params.include_worktree.unwrap_or(false);
        let after = match params.cursor.as_deref().map(parse_cursor).transpose() {
            Ok(after) => after,
            Err(error) => return Answer::Now(Err(error)),
        };
        let limit = params.limit.unwrap_or(PAGE_LIMIT).clamp(1, PAGE_CAP) as usize;
        let mut matching: Vec<(u64, &SessionMeta)> = self
            .sessions
            .iter()
            .filter(|meta| {
                self.target_reachable(caller, &scope, meta, false)
                    && states.contains(&thread_state(meta))
                    && (include_children || meta.parent_session_id.is_none())
                    && params
                        .follow_up
                        .is_none_or(|wanted| meta.follow_up.is_some() == wanted)
                    && title_query
                        .as_deref()
                        .is_none_or(|query| meta.title.to_lowercase().contains(query))
            })
            .map(|meta| (self.activity_at(meta), meta))
            .filter(|(at, _)| {
                params.active_since.is_none_or(|since| *at >= since)
                    && params.active_before.is_none_or(|before| *at < before)
            })
            .collect();
        matching.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.id.cmp(&b.1.id)));
        let start = after.map_or(0, |(cursor_at, cursor_id)| {
            matching.partition_point(|(at, meta)| {
                *at > cursor_at || (*at == cursor_at && meta.id.as_str() <= cursor_id)
            })
        });
        let page = &matching[start..];
        let shown = &page[..page.len().min(limit)];
        let next_cursor = (page.len() > shown.len())
            .then(|| shown.last().map(|(at, meta)| format!("{at}:{}", meta.id)))
            .flatten();
        let mut omitted = Vec::new();
        if next_cursor.is_some() {
            omitted.push(format!(
                "{} more threads; pass next_cursor for the next page",
                page.len() - shown.len()
            ));
        }
        let mut threads: Vec<ThreadRow> = shown
            .iter()
            .map(|(_, meta)| {
                let mut row = self.thread_row(meta);
                if include_worktree {
                    row.worktree = Some(worktree_facts(meta));
                }
                row
            })
            .collect();
        let total = matching.len();
        if !include_worktree {
            return Answer::Now(to_value(&ThreadPage {
                threads,
                total,
                next_cursor,
                omitted,
            }));
        }
        Answer::Later(cx.unblock(move || {
            let dirty = dirty_states(
                threads
                    .iter()
                    .filter_map(|row| row.worktree.as_ref().map(|w| w.cwd.clone())),
            );
            for worktree in threads.iter_mut().filter_map(|row| row.worktree.as_mut()) {
                worktree.dirty = dirty.get(&worktree.cwd).copied().flatten();
            }
            to_value(&ThreadPage {
                threads,
                total,
                next_cursor,
                omitted,
            })
        }))
    }

    pub(super) fn op_get_thread(
        &mut self,
        _caller: &Caller,
        params: GetThreadParams,
        _cx: &mut HostCx,
    ) -> Result<serde_json::Value, String> {
        let meta = self
            .find_meta(&params.thread_id)
            .ok_or_else(|| format!("unknown thread: {}", params.thread_id))?;
        let projection = ActivityProjection::new(self);
        let resident = self.resident(&meta.id);
        let mut queue = QueueCounts::default();
        for message in resident.iter().flat_map(|session| &session.queue) {
            match message.origin {
                MessageOrigin::Human => queue.human += 1,
                MessageOrigin::Agent => queue.agent += 1,
                MessageOrigin::Server => queue.server += 1,
            }
            if message.not_before.is_some() {
                queue.scheduled += 1;
            }
        }
        let terminals = resident
            .map(|session| &session.terminal_workspace)
            .or_else(|| {
                self.terminal_workspaces
                    .get(&ConversationDestination::Thread(meta.id.clone()))
            })
            .into_iter()
            .flat_map(|workspace| &workspace.terminals)
            .map(|entry| TerminalRow {
                id: entry.id,
                label: entry.terminal.label(),
            })
            .collect();
        let mut thread = self.thread_row(&meta);
        thread.worktree = Some(worktree_facts(&meta));
        let detail = ThreadDetail {
            settlement: self.settlement_explanation(&meta, now_millis(), &projection),
            activity: projection.session_activity(&meta, resident),
            queue,
            pending_approvals: resident
                .into_iter()
                .flat_map(|session| &session.timeline.pending_approvals)
                .map(|approval| PendingApproval {
                    id: approval.id.clone(),
                    kind: approval_kind(&approval.kind),
                })
                .collect(),
            pending_input: resident
                .and_then(|session| session.timeline.pending_user_input.as_ref())
                .map(|input| PendingInput {
                    request_id: input.request_id.clone(),
                    questions: input.questions.len(),
                }),
            control: self.control_status(&meta),
            lineage: Lineage {
                parent_session_id: meta.parent_session_id.clone(),
                children: self
                    .sessions
                    .iter()
                    .filter(|child| child.parent_session_id.as_deref() == Some(meta.id.as_str()))
                    .map(|child| child.id.clone())
                    .collect(),
                pending_fork: meta.pending_fork,
                imported_from: meta.imported_from.clone(),
                native_subagent: meta.native_subagent.is_some(),
            },
            terminals,
            omitted: vec![
                "worktree.dirty: read it live with get_git or list_threads include_worktree".into(),
                "the thread a fork was made from is not recorded".into(),
            ],
            thread,
        };
        to_value(&detail)
    }

    pub(super) fn op_search(
        &mut self,
        caller: &Caller,
        params: SearchParams,
        cx: &mut HostCx,
    ) -> Answer {
        let query = params.query.trim().to_owned();
        if query.is_empty() {
            return Answer::Now(Err("search needs a non-empty query".into()));
        }
        let scope = params
            .scope
            .clone()
            .unwrap_or_else(|| caller.grant.scope.clone());
        let mut omitted = Vec::new();
        let mut readable: Vec<(u64, SessionMeta)> = self
            .sessions
            .iter()
            .filter(|meta| self.target_reachable(caller, &scope, meta, true))
            .map(|meta| (self.activity_at(meta), meta.clone()))
            .collect();
        if let Some(ids) = &params.thread_ids {
            readable.retain(|(_, meta)| ids.contains(&meta.id));
            for id in ids {
                if !readable.iter().any(|(_, meta)| &meta.id == id) {
                    omitted.push(format!("thread {id} is outside the requested scope"));
                }
            }
        }
        readable.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.id.cmp(&b.1.id)));
        let sessions: Vec<SessionMeta> = readable.into_iter().map(|(_, meta)| meta).collect();
        let kinds = params
            .kinds
            .as_deref()
            .map(|kinds| search_kinds(kinds, &mut omitted));
        omitted.push("neighbouring entries: read around a hit's anchor with read from/to".into());
        omitted.push("command and tool output is not indexed; use search_in_item".into());
        let offset: usize = match params.cursor.as_deref() {
            Some(cursor) => match cursor.parse() {
                Ok(offset) => offset,
                Err(_) => return Answer::Now(Err(format!("invalid cursor: {cursor}"))),
            },
            None => 0,
        };
        let filter = SearchFilter {
            kinds,
            limit: params.limit.unwrap_or(SEARCH_LIMIT).clamp(1, SEARCH_CAP) as usize,
            offset,
        };
        if filter.kinds.as_ref().is_some_and(Vec::is_empty) {
            return Answer::Now(to_value(&SearchAnswer {
                query,
                hits: Vec::new(),
                per_thread: Vec::new(),
                per_turn: Vec::new(),
                total: 0,
                truncated: false,
                next_cursor: None,
                redacted: 0,
                omitted,
            }));
        }
        let search = self.session_search.clone();
        Answer::Later(cx.unblock(move || {
            let results = search.search_filtered(&sessions, &query, &filter);
            let mut redacted = 0;
            let hits: Vec<SearchHitRow> = results
                .hits
                .into_iter()
                .map(|hit| {
                    let anchor = format!("{}#{}", hit.hit.session_id, hit.position);
                    let (snippet, count) = redact(&hit.hit.snippet);
                    redacted += count;
                    SearchHitRow {
                        snippet: fence(&anchor, &snippet),
                        anchor,
                        thread_id: hit.hit.session_id,
                        title: hit.hit.session_title,
                        turn: hit.hit.turn,
                        kind: hit.kind,
                    }
                })
                .collect();
            if results.per_turn.len() > PER_TURN_CAP {
                omitted.push(format!(
                    "per_turn lists the first {PER_TURN_CAP} of {} turns with hits",
                    results.per_turn.len()
                ));
            }
            let next_cursor = results
                .truncated
                .then(|| (filter.offset + hits.len()).to_string());
            to_value(&SearchAnswer {
                query,
                per_thread: results
                    .per_thread
                    .into_iter()
                    .map(|(thread_id, hits)| ThreadCount { thread_id, hits })
                    .collect(),
                per_turn: results
                    .per_turn
                    .into_iter()
                    .take(PER_TURN_CAP)
                    .map(|(thread_id, turn, hits)| TurnCount {
                        thread_id,
                        turn,
                        hits,
                    })
                    .collect(),
                hits,
                total: results.total,
                truncated: results.truncated,
                next_cursor,
                redacted,
                omitted,
            })
        }))
    }

    pub(super) fn op_get_git(
        &mut self,
        caller: &Caller,
        params: GetGitParams,
        cx: &mut HostCx,
    ) -> Answer {
        let mut answer = GitAnswer::default();
        let cwd = match (&params.thread_id, &params.project_id) {
            (Some(thread_id), None) => {
                answer.thread_id = Some(thread_id.clone());
                self.find_meta(thread_id)
                    .map(|meta| meta.cwd)
                    .ok_or_else(|| format!("unknown thread: {thread_id}"))
            }
            (None, Some(project_id)) => {
                answer.project_id = Some(project_id.clone());
                if caller
                    .grant
                    .scope
                    .reaches(caller.project_id.as_deref(), Some(project_id))
                {
                    self.projects
                        .iter()
                        .find(|project| &project.id == project_id)
                        .map(|project| project.root.clone())
                        .ok_or_else(|| format!("unknown project: {project_id}"))
                } else {
                    Err(format!(
                        "refused: project {project_id} is outside your grant's scope ({})",
                        caller.grant.scope.describe()
                    ))
                }
            }
            _ => Err("pass exactly one of thread_id and project_id".into()),
        };
        answer.cwd = match cwd {
            Ok(cwd) => cwd,
            Err(error) => return Answer::Now(Err(error)),
        };
        Answer::Later(cx.unblock(move || {
            let cwd = answer.cwd.clone();
            match params.what {
                GitWhat::Status => {
                    let status = tcode_services::git::read_status(&cwd);
                    if status.is_repo {
                        answer.status = Some(status);
                    } else {
                        answer.error = Some("not a git repository".into());
                    }
                }
                GitWhat::Branches => {
                    answer.branches = Some(tcode_services::git::list_git_branches(&cwd));
                    answer.current_branch = tcode_services::git::read_git_branch(&cwd);
                }
                GitWhat::Diff => {
                    let diff = tcode_services::git::load_git_diff(
                        &cwd,
                        tcode_protocol::GitDiffScope::WorkingTree,
                        None,
                        false,
                    );
                    answer.error = diff.error;
                    if diff.truncated {
                        answer
                            .omitted
                            .push("git's diff was cut at the host's diff limit".into());
                    }
                    let mut text = String::new();
                    let mut left_out = 0;
                    for patch in diff
                        .changes
                        .iter()
                        .filter_map(|change| change.diff.as_ref())
                    {
                        if text.len() + patch.len() > RESPONSE_BUDGET {
                            left_out += 1;
                            continue;
                        }
                        text.push_str(patch);
                        if !patch.ends_with('\n') {
                            text.push('\n');
                        }
                    }
                    if left_out > 0 {
                        answer.omitted.push(format!(
                            "the diffs of {left_out} files did not fit the response budget"
                        ));
                    }
                    let (text, redacted) = redact(&text);
                    answer.redacted = redacted;
                    answer.diff = Some(text);
                    answer.files = Some(
                        diff.changes
                            .into_iter()
                            .map(|change| DiffFile {
                                path: change.path,
                                kind: change.kind,
                            })
                            .collect(),
                    );
                }
            }
            to_value(&answer)
        }))
    }

    pub(super) fn op_get_pull_request(
        &mut self,
        _caller: &Caller,
        params: GetPullRequestParams,
        _cx: &mut HostCx,
    ) -> Result<serde_json::Value, String> {
        let meta = self
            .find_meta(&params.thread_id)
            .ok_or_else(|| format!("unknown thread: {}", params.thread_id))?;
        let link = find_pull_request(&meta, &params.key).ok_or_else(|| {
            format!(
                "no pull request {} is linked to thread {}",
                params.key, params.thread_id
            )
        })?;
        to_value(&PullRequestAnswer {
            key: pull_request_label(&link.key),
            pull_request: link,
            omitted: vec![
                "the host's last snapshot only (checks and reviews rolled up as checks_state \
                 and review_decision, as of snapshot.synced_at); nothing was fetched"
                    .into(),
            ],
        })
    }

    pub(super) fn op_list_providers(
        &mut self,
        _caller: &Caller,
        _params: ListProvidersParams,
        _cx: &mut HostCx,
    ) -> Result<serde_json::Value, String> {
        let status = self.providers_status_snapshot();
        let providers: Vec<ProviderRow> = ProviderKind::NATIVE
            .into_iter()
            .chain([ProviderKind::Acp])
            .map(|kind| {
                let caps = kind.caps();
                ProviderRow {
                    kind: provider_key(kind),
                    caps: CapsFacts {
                        steering: caps.supports_steering,
                        fork: caps.supports_fork,
                        native_rewind: caps.native_rewind,
                        manual_compaction: caps.manual_compaction,
                        goals: caps.goals.map(|goals| GoalFacts {
                            pause: goals.pause,
                            live: goals.live,
                        }),
                        per_turn_effort: caps.per_turn_effort,
                        mcp_servers: caps.mcp_servers,
                        native_computer_use: caps.native_computer_use,
                    },
                    version: status.provider_versions.get(&kind).map(|version| {
                        tcode_protocol::ProviderVersionStatus {
                            // The full command carries the host's PATH and HOME.
                            update_command: None,
                            ..version.clone()
                        }
                    }),
                    models: status
                        .model_catalogs
                        .get(&kind)
                        .into_iter()
                        .flatten()
                        .map(|model| ModelRow {
                            id: model.id.clone(),
                            display_name: model.display_name.clone(),
                            is_default: model.is_default,
                        })
                        .collect(),
                    models_loading: status.models_loading.get(&kind).copied().unwrap_or(false),
                    profiles: self
                        .settings
                        .profiles_for_kind(kind)
                        .into_iter()
                        .map(|profile| ProfileRow {
                            display_name: self.settings.profile_display_name(&profile.id),
                            enabled: profile.settings.enabled,
                            status: status.provider_snapshots.get(&profile.id).cloned(),
                            usage: status.provider_usage.get(&profile.id).cloned(),
                            env_names: profile
                                .settings
                                .env
                                .iter()
                                .map(|var| var.name.clone())
                                .collect(),
                            custom_models: profile.settings.custom_models.clone(),
                            hidden_models: profile.settings.hidden_models.clone(),
                            id: profile.id,
                        })
                        .collect(),
                    acp_agents: if kind == ProviderKind::Acp {
                        self.settings
                            .acp_agents
                            .values()
                            .map(|agent| AcpAgentRow {
                                id: agent.id.clone(),
                                name: agent.name.clone(),
                                version: agent.version.clone(),
                                enabled: agent.enabled,
                            })
                            .collect()
                    } else {
                        Vec::new()
                    },
                }
            })
            .collect();
        Ok(serde_json::json!({ "providers": to_value(&providers)? }))
    }

    /// Settings hold no secret values (those live in the keychain-backed secret store), so the
    /// whole document is readable; `keys` narrows it to top-level fields.
    pub(super) fn op_get_settings(
        &mut self,
        _caller: &Caller,
        params: GetSettingsParams,
        _cx: &mut HostCx,
    ) -> Result<serde_json::Value, String> {
        let mut settings = self.settings.clone();
        // Keys this build does not know are not described anywhere and may hold anything.
        settings.unknown.clear();
        let mut settings = serde_json::to_value(&settings).map_err(|e| e.to_string())?;
        strip_env_values(&mut settings);
        if let Some(keys) = params.keys
            && let serde_json::Value::Object(fields) = &mut settings
        {
            fields.retain(|key, _| keys.iter().any(|wanted| wanted == key));
            let missing: Vec<&str> = keys
                .iter()
                .map(String::as_str)
                .filter(|key| !fields.contains_key(*key))
                .collect();
            if !missing.is_empty() {
                return Err(format!("unknown settings keys: {}", missing.join(", ")));
            }
        }
        let redacted = redact_strings(&mut settings);
        Ok(serde_json::json!({
            "settings": settings,
            "redacted": redacted,
            "omitted": ["environment variable values", "settings keys this build does not know"],
        }))
    }

    pub(super) fn op_read_terminal(
        &mut self,
        caller: &Caller,
        params: ReadTerminalParams,
        _cx: &mut HostCx,
    ) -> Result<serde_json::Value, String> {
        let id = params.terminal_id;
        let owner = self
            .terminal_owner(id)
            .map(str::to_owned)
            .ok_or_else(|| format!("unknown terminal: {id}"))?;
        let meta = self
            .find_meta(&owner)
            .ok_or_else(|| format!("unknown terminal: {id}"))?;
        if !self.target_reachable(caller, &caller.grant.scope, &meta, true) {
            return Err(format!(
                "refused: terminal {id} belongs to thread {owner}, outside your grant's scope ({})",
                caller.grant.scope.describe()
            ));
        }
        let terminal = self
            .terminal_handle(id)
            .ok_or_else(|| format!("terminal {id} is not running"))?;
        let snapshot = terminal.peek_snapshot();
        let screen = snapshot.text();
        let (shown, cut) = tail(&screen, TERMINAL_TAIL_BYTES);
        let (text, redacted) = redact(shown);
        let mut omitted = vec!["scrollback above the visible screen is not included".to_owned()];
        if cut {
            omitted.push(format!(
                "the screen was cut to its last {TERMINAL_TAIL_BYTES} bytes"
            ));
        }
        to_value(&TerminalAnswer {
            terminal_id: id,
            thread_id: owner,
            title: meta.title,
            exited: snapshot.exited,
            exit_code: snapshot.exit_code,
            text: fence(&format!("terminal {id}"), &text),
            redacted,
            omitted,
        })
    }

    pub(super) fn op_review_candidates(
        &mut self,
        caller: &Caller,
        params: ReviewCandidatesParams,
        cx: &mut HostCx,
    ) -> Answer {
        let scope = params
            .scope
            .clone()
            .unwrap_or_else(|| caller.grant.scope.clone());
        let now = now_millis();
        let since = params
            .since
            .unwrap_or_else(|| now.saturating_sub(REVIEW_WINDOW_MS));
        let projection = ActivityProjection::new(self);
        let mut in_scope: Vec<(u64, &SessionMeta)> = self
            .sessions
            .iter()
            .filter(|meta| {
                meta.archived_at.is_none() && self.target_reachable(caller, &scope, meta, false)
            })
            .map(|meta| (self.activity_at(meta), meta))
            .collect();
        in_scope.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.id.cmp(&b.1.id)));
        let projects: Vec<(String, PathBuf)> = self
            .projects
            .iter()
            .filter(|project| {
                !project.quick_chat
                    && scope.reaches(caller.project_id.as_deref(), Some(&project.id))
            })
            .map(|project| (project.id.clone(), project.root.clone()))
            .collect();
        let thread_cwds: HashSet<PathBuf> =
            in_scope.iter().map(|(_, meta)| meta.cwd.clone()).collect();
        let threads = in_scope.len();
        let mut collapsed = Vec::new();
        let mut candidates = Vec::new();
        let mut cold = HashSet::new();
        let mut redacted = 0;
        let mut without_enough_data = 0;
        for (at, meta) in in_scope {
            let collapse = |reason| CollapsedRow {
                id: meta.id.clone(),
                title: meta.title.clone(),
                reason,
                parent_session_id: meta.parent_session_id.clone(),
            };
            if meta.parent_session_id.is_some() {
                collapsed.push(collapse("orchestrate_child"));
                continue;
            }
            if meta.is_settled() && at < since {
                continue;
            }
            let holds = self.settlement_blockers(meta, &projection);
            if holds.live_run {
                collapsed.push(collapse("running"));
                continue;
            }
            if now.saturating_sub(at) < JUST_ACTIVE_MS {
                collapsed.push(collapse("just_active"));
                continue;
            }
            let activity_known = self.thread_activity.contains_key(&meta.id);
            if !activity_known {
                without_enough_data += 1;
            }
            let last = match self.resident(&meta.id) {
                Some(session) => {
                    let (last, count) = last_turn(&meta.id, &session.timeline);
                    redacted += count;
                    Some(last)
                }
                None => {
                    cold.insert(meta.id.clone());
                    None
                }
            };
            let mut thread = self.thread_row(meta);
            thread.worktree = Some(worktree_facts(meta));
            candidates.push(ReviewCandidate {
                thread,
                settlement: self.settlement_explanation(meta, now, &projection),
                last_turn: last,
                pending_approval: holds.pending_approval,
                pending_input: holds.pending_input,
                manual_unsettle: meta.settled_override == Some(SettledOverride::Active),
                idle_days: (!meta.is_settled() && activity_known)
                    .then(|| now.saturating_sub(at) / DAY_MS),
            });
        }
        let mut omitted = Vec::new();
        if collapsed.len() > REVIEW_LIST_CAP {
            omitted.push(format!(
                "{} more collapsed threads (running, just active or orchestrated children) are \
                 not listed; list_threads finds them",
                collapsed.len() - REVIEW_LIST_CAP
            ));
            collapsed.truncate(REVIEW_LIST_CAP);
        }
        let store = self.store.clone();
        Answer::Later(cx.unblock(move || {
            for candidate in &mut candidates {
                if !cold.contains(&candidate.thread.id) {
                    continue;
                }
                match stored_last_turn(&store, &candidate.thread.id) {
                    Some((last, count)) => {
                        redacted += count;
                        candidate.last_turn = Some(last);
                    }
                    // A thread whose activity is known but whose log yields no turn lacks data;
                    // one without known activity was counted already.
                    None if candidate.thread.last_activity_at.is_some() => {
                        without_enough_data += 1;
                    }
                    None => {}
                }
            }
            let dirty = dirty_states(
                candidates
                    .iter()
                    .filter_map(|candidate| candidate.thread.worktree.as_ref())
                    .map(|worktree| worktree.cwd.clone())
                    .chain(projects.iter().map(|(_, root)| root.clone())),
            );
            for worktree in candidates
                .iter_mut()
                .filter_map(|candidate| candidate.thread.worktree.as_mut())
            {
                worktree.dirty = dirty.get(&worktree.cwd).copied().flatten();
            }
            let unattributed_dirty_worktrees = projects
                .iter()
                .filter(|(_, root)| {
                    dirty.get(root).copied().flatten() == Some(true) && !thread_cwds.contains(root)
                })
                .map(|(project_id, root)| UnattributedWorktree {
                    project_id: project_id.clone(),
                    root: root.clone(),
                })
                .collect();
            let mut used = 0;
            let mut left_out = Vec::new();
            candidates.retain(|candidate| {
                let size = serde_json::to_vec(candidate).map_or(0, |bytes| bytes.len());
                if used + size > RESPONSE_BUDGET {
                    left_out.push(candidate.thread.id.clone());
                    return false;
                }
                used += size;
                true
            });
            if !left_out.is_empty() {
                let named = left_out.len().min(REVIEW_LIST_CAP);
                let unnamed = left_out.len() - named;
                omitted.push(format!(
                    "{} candidates did not fit the response budget; get_thread each of: {}{}",
                    left_out.len(),
                    left_out[..named].join(", "),
                    if unnamed > 0 {
                        format!(" and {unnamed} more, narrowed by scope or since")
                    } else {
                        String::new()
                    }
                ));
            }
            to_value(&ReviewAnswer {
                candidates,
                collapsed,
                unattributed_dirty_worktrees,
                coverage: Coverage {
                    projects: projects.len(),
                    threads,
                    without_enough_data,
                    window_since: since,
                },
                redacted,
                omitted,
            })
        }))
    }
}

/// Blank every environment variable's value under an `env` key, whatever shape the list
/// has (`{name, value}` entries or `[name, value]` pairs); the names stay.
fn strip_env_values(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(fields) => {
            for (key, field) in fields.iter_mut() {
                if key == "env"
                    && let serde_json::Value::Array(vars) = field
                {
                    for var in vars {
                        match var {
                            serde_json::Value::Object(var) => {
                                var.remove("value");
                            }
                            serde_json::Value::Array(pair) if pair.len() == 2 => {
                                pair[1] = serde_json::Value::String(String::new());
                            }
                            _ => {}
                        }
                    }
                } else {
                    strip_env_values(field);
                }
            }
        }
        serde_json::Value::Array(items) => items.iter_mut().for_each(strip_env_values),
        _ => {}
    }
}

/// Redact every string in `value` in place; how many secrets were replaced.
fn redact_strings(value: &mut serde_json::Value) -> usize {
    match value {
        serde_json::Value::String(text) => {
            let (clean, count) = redact(text);
            *text = clean;
            count
        }
        serde_json::Value::Array(items) => items.iter_mut().map(redact_strings).sum(),
        serde_json::Value::Object(fields) => fields.values_mut().map(redact_strings).sum(),
        _ => 0,
    }
}

#[cfg(test)]
#[path = "observe_tests.rs"]
mod tests;
