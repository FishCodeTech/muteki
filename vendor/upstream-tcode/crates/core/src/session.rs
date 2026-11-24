//! Pure timeline fold: canonical [`AgentEvent`]s in, renderable timeline out.
//!
//! The same fold is used for live event streams and for JSONL replay, so the
//! UI renders identically in both cases.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use agent::{
    AgentEvent, ApprovalRequest, ChangeCompleteness, DeltaKind, FileChange, ItemContent,
    ItemStatus, PlanStep, ResumeCursor, ThreadItem, TokenUsage, TurnStatus, UserInputDelivery,
    UserInputQuestion,
};
use serde::{Deserialize, Serialize};

use crate::git::merge_file_changes_by_path;

mod boundaries;
mod superseded;

pub use boundaries::{TurnBoundaries, TurnBoundaryChange};

pub use superseded::{TurnSnapshots, drop_turn_diffs};

/// Bump when the persisted turn-boundary policy changes.
pub const TURN_INDEX_FOLD_VERSION: u32 = 3;

fn synthetic_entry_id(
    prefix: &str,
    ts: Option<u64>,
    next: &mut u64,
    mut exists: impl FnMut(&str) -> bool,
) -> String {
    let Some(ts) = ts else {
        *next += 1;
        return format!("{prefix}-{next}");
    };
    let base = format!("{prefix}-{ts}");
    let mut id = base.clone();
    let mut duplicate = 1;
    while exists(&id) {
        id = format!("{base}-{duplicate}");
        duplicate += 1;
    }
    id
}

/// Claude Code's own prompt for resuming work after a usage-window reset.
pub const RESUME_PROMPT: &str = "Continue from where you left off.";

/// A local review note attached to a range in the diff panel. These live in
/// the composer draft until the next send; they are never written to session
/// history as separate events.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewComment {
    pub file: String,
    pub line_start: u32,
    pub line_end: u32,
    pub side: ReviewSide,
    pub text: String,
    pub code_excerpt: String,
    pub(crate) section_id: String,
    pub(crate) section_title: String,
    pub(crate) start_index: usize,
    pub(crate) end_index: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewSide {
    Old,
    New,
}

impl ReviewComment {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        file: String,
        line_start: u32,
        line_end: u32,
        side: ReviewSide,
        text: String,
        code_excerpt: String,
        section_id: String,
        section_title: String,
        start_index: usize,
        end_index: usize,
    ) -> Self {
        Self {
            file,
            line_start: line_start.min(line_end),
            line_end: line_start.max(line_end),
            side,
            text,
            code_excerpt,
            section_id,
            section_title,
            start_index: start_index.min(end_index),
            end_index: start_index.max(end_index),
        }
    }

    pub fn range_label(&self) -> String {
        let marker = match self.side {
            ReviewSide::Old => "-",
            ReviewSide::New => "+",
        };
        if self.line_start == self.line_end {
            format!("{marker}{}", self.line_start)
        } else {
            format!("{marker}{} to {marker}{}", self.line_start, self.line_end)
        }
    }
}

fn escape_review_attribute(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn review_fence(contents: &str) -> String {
    let longest = contents
        .split(|character| character != '`')
        .map(str::len)
        .max()
        .unwrap_or(0);
    let fence = "`".repeat(3.max(longest + 1));
    format!("{fence}diff\n{}\n{fence}", contents.trim_end())
}

/// Serialize review notes as `<review_comment ...>` blocks in the agent prompt.
pub fn append_review_comments_to_prompt(prompt: &str, comments: &[ReviewComment]) -> String {
    if comments.is_empty() {
        return prompt.to_string();
    }
    let blocks = comments
        .iter()
        .map(|comment| {
            format!(
                "<review_comment sectionId=\"{}\" sectionTitle=\"{}\" filePath=\"{}\" startIndex=\"{}\" endIndex=\"{}\" rangeLabel=\"{}\">\n{}\n{}\n</review_comment>",
                escape_review_attribute(&comment.section_id),
                escape_review_attribute(&comment.section_title),
                escape_review_attribute(&comment.file),
                comment.start_index,
                comment.end_index,
                escape_review_attribute(&comment.range_label()),
                comment.text.trim(),
                review_fence(&comment.code_excerpt),
            )
        })
        .collect::<Vec<_>>()
        .join("\n\n");
    let prompt = prompt.trim();
    if prompt.is_empty() {
        blocks
    } else {
        format!("{prompt}\n\n{blocks}")
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Author {
    pub device_id: String,
    pub name: String,
}

/// One persisted event, optionally tagged with the wall-clock time (unix ms)
/// at which it was recorded. Legacy `.jsonl` lines replay with `ts == None`;
/// envelope lines carry the recorded timestamp.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StoredEvent {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<MessageOrigin>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author: Option<Author>,
    pub ts: Option<u64>,
    pub event: AgentEvent,
    /// The byte length of an item output that a host shortened before sending
    /// this record; the full output stays on the host. Never set on a record
    /// read from the log.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub elided: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageOrigin {
    Human,
    Agent,
    Server,
}

impl MessageOrigin {
    pub fn legacy(has_parent: bool) -> Self {
        if has_parent { Self::Agent } else { Self::Human }
    }
}

impl StoredEvent {
    /// The item an event creates or continues.
    pub fn item_id(&self) -> Option<&str> {
        match &self.event {
            AgentEvent::ItemStarted(item)
            | AgentEvent::ItemUpdated(item)
            | AgentEvent::ItemCompleted(item) => Some(&item.id),
            AgentEvent::Delta { item_id, .. } => Some(item_id),
            _ => None,
        }
    }
}

impl From<AgentEvent> for StoredEvent {
    fn from(event: AgentEvent) -> Self {
        StoredEvent {
            origin: None,
            author: None,
            ts: None,
            event,
            elided: None,
        }
    }
}

/// One renderable row in the chat timeline.
#[derive(Debug, Clone, PartialEq)]
pub struct TimelineEntry {
    /// Provider item id (or a synthetic id for errors).
    pub id: String,
    pub content: EntryContent,
    /// Wall-clock time (unix ms) this entry was first observed, if known.
    pub ts: Option<u64>,
    /// Index into [`Timeline::turns`] of the turn this entry belongs to.
    pub turn: usize,
}

/// Per-turn ("Work Log" section) metadata folded from turn lifecycle events.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TurnMeta {
    /// Provider-native id for this turn, used to attach replacement turn-diff
    /// snapshots without relying on ambient "current turn" state during replay.
    pub provider_turn_id: Option<String>,
    /// Opaque provider-owned restore point for the user message that opened
    /// this turn. Present only when the provider exposes a native rewind API.
    pub provider_checkpoint_id: Option<String>,
    /// When the turn began (TurnStarted, or the opening user message).
    pub start_ts: Option<u64>,
    /// When the turn finished (TurnCompleted).
    pub end_ts: Option<u64>,
    pub status: Option<TurnStatus>,
    /// Whether this turn is currently running.
    pub running: bool,
    /// File changes causally attributed to this turn. Provider-native net
    /// snapshots replace this wholesale; structured file-operation items form
    /// a partial fallback without consulting the ambient Git working tree.
    pub changes: Option<TurnChangeSet>,
    /// Wall-clock breakdown of the finished turn. `None` while the turn runs,
    /// whenever the turn lacks a timestamped `TurnStarted`/`TurnCompleted` pair
    /// to measure against, and whenever the recorded clock regressed across the
    /// turn's end — a missing or untrustworthy timestamp yields no breakdown
    /// rather than an invented one.
    pub timing: Option<TurnTiming>,
    /// Model that actually served this turn, when reported by the provider.
    pub served_model: Option<String>,
    /// Provider-reported cost for this turn, in US dollars.
    pub cost_usd: Option<f64>,
    /// Provider-reported duration, distinct from tcode's observed wall clock.
    pub provider_duration_ms: Option<u64>,
}

/// How a finished turn's wall clock divided between waiting on tools and
/// everything else (the model thinking and answering). Millisecond based, and
/// derived purely from the timestamps already recorded on the event stream, so
/// a live session and a replay of its log agree exactly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct TurnTiming {
    /// The observed `TurnStarted`..`TurnCompleted` span.
    pub total_ms: u64,
    /// The union of the intervals in which at least one tool-like item was in
    /// progress. Tools running in parallel are counted once, never summed.
    pub tool_ms: u64,
}

impl TurnTiming {
    /// Build a breakdown. Tool time is already intersected with the turn's
    /// bounds by [`ToolClock`]; the clamp here is only a last-resort guard that
    /// keeps `tool_ms <= total_ms` true for hand-built values.
    pub fn new(total_ms: u64, tool_ms: u64) -> Self {
        Self {
            total_ms,
            tool_ms: tool_ms.min(total_ms),
        }
    }
}

/// Running union-of-intervals accounting for the tool-like items of the open
/// turn, intersected with the turn's own `TurnStarted`..`TurnCompleted` bounds.
/// Only the current turn can have items in flight, so one accumulator is
/// enough; it resets whenever a turn opens.
#[derive(Debug, Clone, Default, PartialEq)]
struct ToolClock {
    /// Item ids currently known to be in progress.
    open: HashSet<String>,
    /// Start of the interval that opened when `open` became non-empty.
    union_start: Option<u64>,
    /// Union of the closed tool-active intervals so far, in ms.
    tool_ms: u64,
    /// Latest timestamp observed anywhere inside the turn — tool lifecycle
    /// events, ordinary items, and deltas alike. It clamps a clock that steps
    /// backwards, and it is the watermark a completion must not precede.
    clock: Option<u64>,
    /// The authoritative turn start: the timestamp of the observed
    /// `TurnStarted`. Tool time is measured from here, never earlier, and its
    /// absence means the turn gets no breakdown at all.
    turn_start: Option<u64>,
    /// A tool-like item was observed without a timestamp, so this turn cannot
    /// produce a trustworthy breakdown.
    untimed: bool,
}

impl ToolClock {
    /// Anchor the clock to the authoritative `TurnStarted` time. Anything
    /// accumulated before it belonged to earlier work and is discarded; a tool
    /// still open across the boundary is rebased to begin exactly at the turn
    /// start, so only the in-bounds part of its interval counts.
    fn begin_turn(&mut self, ts: u64) {
        self.advance(ts);
        self.turn_start = Some(ts);
        self.tool_ms = 0;
        if let Some(start) = self.union_start {
            self.union_start = Some(start.max(ts));
        }
    }

    /// Note a timestamp seen inside the turn, whatever event carried it. The
    /// turn's own completion is the one event excluded: it is measured
    /// *against* this watermark rather than folded into it.
    fn observe(&mut self, ts: Option<u64>) {
        if let Some(ts) = ts {
            self.advance(ts);
        }
    }

    /// Record a tool-like lifecycle transition. `active` marks the item as in
    /// progress; otherwise it is finished.
    fn mark(&mut self, ts: Option<u64>, item_id: &str, active: bool) {
        let Some(ts) = ts else {
            self.untimed = true;
            return;
        };
        let ts = self.advance(ts);
        if active {
            // Repeated updates for an already-open item change nothing; the
            // first sighting opens it.
            if self.open.insert(item_id.to_owned()) && self.open.len() == 1 {
                self.union_start = Some(ts);
            }
        } else if self.open.remove(item_id) && self.open.is_empty() {
            self.close(ts);
        }
    }

    /// Accept a timestamp, never letting it move the clock backwards: a
    /// backward stamp contributes a zero-length step instead of underflowing.
    fn advance(&mut self, ts: u64) -> u64 {
        let clamped = self.clock.map_or(ts, |clock| ts.max(clock));
        self.clock = Some(clamped);
        clamped
    }

    /// Charge the open interval up to `ts`, intersected with the turn start.
    fn close(&mut self, ts: u64) {
        if let Some(start) = self.union_start.take() {
            let start = self.turn_start.map_or(start, |bound| start.max(bound));
            self.tool_ms = self.tool_ms.saturating_add(ts.saturating_sub(start));
        }
    }

    /// Close the turn at `end`, charging any still-open tools up to it. `end`
    /// is the turn's own upper bound, so the interval never extends past it.
    fn finish(mut self, end: u64) -> u64 {
        self.close(end);
        self.tool_ms
    }
}

/// Derive the finished turn's breakdown, or `None` when the events cannot
/// support one: no timestamped `TurnStarted`/`TurnCompleted` pair (legacy logs,
/// or a turn opened only by a user message), a tool-like item seen without a
/// timestamp, or a completion that precedes work already recorded inside the
/// turn.
///
/// That last case is a wall clock that regressed across the turn boundary: some
/// event inside the turn is stamped later than the turn's own end, so the true
/// bounds are unknowable. Clamping the aggregate would keep the total honest
/// while silently misattributing the split between the buckets — a tool that
/// "ran" past the end would eat AI time that may never have been tool time at
/// all. There is no defensible attribution to guess at, so the breakdown is
/// withheld and the UI falls back to the bare completion clock.
fn finish_timing(clock: ToolClock, end: Option<u64>) -> Option<TurnTiming> {
    let (start, end) = (clock.turn_start?, end?);
    if clock.untimed || end < start || clock.clock.is_some_and(|latest| end < latest) {
        return None;
    }
    Some(TurnTiming::new(end - start, clock.finish(end)))
}

/// Which lifecycle event carried a tool-like item. The variant is authoritative
/// over the item's own `status` field, which several providers drop entirely
/// (Codex maps `webSearch` and unmodeled items to statusless content).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ToolLifecycle {
    Started,
    Updated,
    Completed,
}

/// A tool-like item's own view of its state. `Unknown` is a tool-like item that
/// reports no status of its own (`WebSearch`, `Other`); `None` from
/// [`tool_item_state`] means the item is model output and is never timed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ToolState {
    Active,
    Finished,
    Unknown,
}

/// Classify an item as tool-like — a command, file edit, tool call, subagent,
/// web search, or an unmodeled provider item — and read the status it carries.
fn tool_item_state(content: &ItemContent) -> Option<ToolState> {
    let status = match content {
        ItemContent::CommandExecution { status, .. }
        | ItemContent::FileChange { status, .. }
        | ItemContent::ToolCall { status, .. }
        | ItemContent::Subagent { status, .. } => *status,
        ItemContent::ImageRead { .. }
        | ItemContent::WebSearch { .. }
        | ItemContent::Other { .. } => {
            return Some(ToolState::Unknown);
        }
        ItemContent::UserMessage { .. }
        | ItemContent::AssistantMessage { .. }
        | ItemContent::Reasoning { .. } => return None,
    };
    Some(match status {
        ItemStatus::InProgress => ToolState::Active,
        ItemStatus::Completed
        | ItemStatus::Failed
        | ItemStatus::Interrupted
        | ItemStatus::Declined => ToolState::Finished,
    })
}

/// Whether a tool-like item is in progress after this transition. The lifecycle
/// variant wins: a start always opens the interval and a completion always
/// closes it, whatever status the snapshot carries (or fails to carry). Only an
/// update defers to an explicit status, and a statusless update keeps the item
/// active.
fn tool_is_active(lifecycle: ToolLifecycle, state: ToolState) -> bool {
    match lifecycle {
        ToolLifecycle::Started => true,
        ToolLifecycle::Completed => false,
        ToolLifecycle::Updated => state != ToolState::Finished,
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct TurnChangeSet {
    pub changes: Vec<FileChange>,
    pub completeness: ChangeCompleteness,
}

#[derive(Debug, Clone, PartialEq)]
pub enum EntryContent {
    Item(ItemContent),
    /// A user message injected into an already-open turn. Provider-originated
    /// user messages live in [`EntryContent::Item`]; this tcode-only variant
    /// carries the local delivery state that [`ItemContent`] does not model.
    Steer {
        text: String,
        /// Delivery state for a message injected into an already-open turn.
        status: SteeringStatus,
        /// Byte length of an injected context prefix folded into `text` (the
        /// orchestrate guidance + configuration composed ahead of the user's own
        /// words). When present, the UI renders `text[..context_len]` as a
        /// collapsed disclosure row and keeps the bubble to `text[context_len..]`.
        /// `None` for ordinary messages and for logs predating the annotation.
        context_len: Option<usize>,
        /// Local paths of the image attachments sent with this message (empty
        /// for text-only messages and for logs predating the field).
        attachments: Vec<String>,
    },
    Error {
        message: String,
        /// Unix seconds when the turn failed because the usage window is exhausted.
        limit_resets_at: Option<u64>,
    },
    #[rustfmt::skip]
    ProviderStartError { error: String },
    /// A tcode-level conversation handoff, rendered as a divider before the
    /// first user message sent to the new provider, with the handoff `prompt`
    /// as sent disclosed under it.
    ProviderRelay {
        from_provider: agent::ProviderKind,
        from_model: Option<String>,
        to_provider: agent::ProviderKind,
        to_model: Option<String>,
        prompt: Option<String>,
        /// The directory the thread moved from, when the handoff follows a move.
        moved_from: Option<std::path::PathBuf>,
    },
    /// The provider changed the model actually serving this session.
    ModelChanged {
        from: Option<String>,
        to: String,
        reason: Option<String>,
    },
    /// The provider compacted its context window (a "Context compacted" work-log row).
    ContextCompacted(agent::Compaction),
    /// The user changed the context window for the next provider turn.
    ContextWindowChanged {
        window: u64,
    },
    /// Another thread's model acted on this thread through `tcode_control`
    /// (a system row naming the actor and the action).
    ControlAction(agent::ControlAction),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SteeringStatus {
    Pending,
    Accepted,
}

/// A structured question set the agent is waiting on, or working past, from
/// [`AgentEvent::UserInputRequested`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PendingUserInput {
    pub request_id: String,
    pub questions: Vec<UserInputQuestion>,
    pub delivery: UserInputDelivery,
}

/// The turn a live provider is running, located in the session's whole log so
/// a fold of any window of it can find the turn among its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunningTurn {
    /// Index of the turn among every turn the whole log folds to.
    pub turn: u64,
    /// When the turn began, if the record that opened it carried a time.
    pub started_at: Option<u64>,
}

/// Folded view of a session's event history. Two timelines are equal only
/// when every later event folds the same onto both, so equality covers the
/// private state the fold continues from, not just what renders.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Timeline {
    /// Top-level entries are shared so virtualized UI snapshots can retain a
    /// turn without cloning its potentially large message, command-output, and
    /// diff payloads. Updates use [`Arc::make_mut`], preserving the value
    /// semantics of a cloned [`Timeline`] while keeping read snapshots cheap.
    pub entries: Vec<Arc<TimelineEntry>>,
    /// One entry per turn ("Work Log" section), in order.
    pub turns: Vec<TurnMeta>,
    pub turn_running: bool,
    pub turn_retry: Option<agent::TurnRetry>,
    pub pending_approvals: Vec<ApprovalRequest>,
    /// The latest structured plan/task list (`PlanUpdated`), if any.
    pub plan_steps: Vec<PlanStep>,
    /// The explanation string from the latest `PlanUpdated`, if any.
    pub plan_explanation: Option<String>,
    /// The harness's standing goal from the latest `GoalUpdated`, if any.
    pub goal: Option<agent::Goal>,
    /// The open user-input request (Claude `AskUserQuestion`, Codex
    /// `requestUserInput` / `request_user_input_async`), if any. Cleared when
    /// it resolves or the turn ends.
    pub pending_user_input: Option<PendingUserInput>,
    pub usage: Option<TokenUsage>,
    pub resume: Option<ResumeCursor>,
    pub provider_session_id: Option<String>,
    pub model: Option<String>,
    /// Latest model reported by the serving provider. Seeded from `model`.
    last_served_model: Option<String>,
    pub last_turn_status: Option<TurnStatus>,
    boundaries: TurnBoundaries,
    /// Monotonic counter for synthetic entry ids.
    next_synthetic_id: u64,
    /// FileChange items known to have completed successfully. The ids rebuild
    /// deterministically from persisted ItemCompleted events during replay.
    committed_file_change_items: HashSet<String>,
    /// Tool-time accounting for the open turn (see [`TurnMeta::timing`]).
    tool_clock: ToolClock,
    /// Full output lengths of the items whose records arrived shortened
    /// ([`StoredEvent::elided`]), keyed by item id.
    pub elided_outputs: HashMap<String, u64>,
    entry_index: EntryIndex,
}

/// Where each entry id first appears in [`Timeline::entries`], so the fold
/// finds an item's entry without a linear search per event, which made
/// folding a long log quadratic. It is derived from `entries`: it covers the
/// prefix it has seen, takes no part in equality, is reset by the fold's own
/// edits other than appends, and every hit is checked against `entries`,
/// which code outside the fold may also edit.
#[derive(Debug, Clone, Default)]
struct EntryIndex {
    first: HashMap<String, usize>,
    covered: usize,
}

impl PartialEq for EntryIndex {
    fn eq(&self, _: &Self) -> bool {
        true
    }
}

impl Timeline {
    /// The position of the first entry with `id`.
    fn first_entry(&mut self, id: &str) -> Option<usize> {
        let index = &mut self.entry_index;
        if index.covered > self.entries.len() {
            *index = EntryIndex::default();
        }
        for (position, entry) in self.entries.iter().enumerate().skip(index.covered) {
            index.first.entry(entry.id.clone()).or_insert(position);
        }
        index.covered = self.entries.len();
        let position = *index.first.get(id)?;
        if self.entries[position].id == id {
            return Some(position);
        }
        self.entry_index = EntryIndex::default();
        self.first_entry(id)
    }

    /// Edits to `entries` other than appends move the positions the index
    /// holds.
    fn reindex_entries(&mut self) {
        self.entry_index = EntryIndex::default();
    }

    /// Fold a whole event sequence (replay path). Accepts either bare
    /// [`AgentEvent`]s (ts unknown) or timestamped [`StoredEvent`]s.
    pub fn fold_events(events: impl IntoIterator<Item = impl Into<StoredEvent>>) -> Self {
        let mut timeline = Self::default();
        for event in events {
            timeline.apply_stored(&event.into());
        }
        timeline
    }

    /// Fold records a client already holds, without cloning their payloads:
    /// a history page refolds the whole held log, whose tool outputs and
    /// diffs are the bulk of it.
    pub fn fold_stored<'a>(records: impl IntoIterator<Item = &'a StoredEvent>) -> Self {
        let mut timeline = Self::default();
        for record in records {
            timeline.apply_stored(record);
        }
        timeline
    }

    /// Fold one record, remembering whether it carried a shortened output.
    pub fn apply_stored(&mut self, stored: &StoredEvent) {
        if let Some(bytes) = stored.elided
            && let Some(item_id) = stored.item_id()
        {
            self.elided_outputs.insert(item_id.to_owned(), bytes);
        }
        self.apply_at(stored.ts, &stored.event);
    }

    /// The latest `report_result` call this thread made: the tool-call item id
    /// and the report text. Read from the call itself, so a reload or a cold log
    /// sees exactly the report the live stream did. A call the broker failed
    /// still counts: the child chose that text as its report.
    pub fn last_report(&self) -> Option<(&str, &str)> {
        self.entries
            .iter()
            .rev()
            .find_map(|entry| match &entry.content {
                EntryContent::Item(ItemContent::ToolCall {
                    name,
                    input,
                    status,
                    ..
                }) if is_report_result_tool(name) && *status != ItemStatus::InProgress => input
                    .get("text")
                    .and_then(serde_json::Value::as_str)
                    .map(|text| (entry.id.as_str(), text)),
                _ => None,
            })
    }

    /// Clear any lingering "running" state (used after replaying a stored
    /// session whose provider process is no longer live).
    pub fn mark_idle(&mut self) {
        self.turn_retry = None;
        self.boundaries.settle_running(None);
        self.turn_running = false;
        self.pending_approvals.clear();
        self.pending_user_input = None;
        for turn in &mut self.turns {
            turn.running = false;
        }
    }

    /// The running turn of a fold that holds the whole log.
    pub fn running_turn(&self) -> Option<RunningTurn> {
        let turn = self.turns.len().checked_sub(1)?;
        (self.turn_running && self.turns[turn].running).then(|| RunningTurn {
            turn: turn as u64,
            started_at: self.turns[turn].start_ts,
        })
    }

    /// Take liveness from the host instead of the records: only the turn at
    /// `running` runs, timed from `started_at` when given. A fold of a window
    /// cannot tell a live turn from one whose provider stopped without a
    /// record, and a window cut inside the turn misses its start. Nothing the
    /// records built is discarded, so a later settle can revive the turn.
    pub fn settle_running_turn(
        &mut self,
        turn_running: bool,
        running: Option<usize>,
        started_at: Option<u64>,
    ) {
        let running = running.filter(|turn| turn_running && *turn < self.turns.len());
        self.turn_running = turn_running;
        self.boundaries.settle_running(running);
        for (index, turn) in self.turns.iter_mut().enumerate() {
            turn.running = running == Some(index);
        }
        if let (Some(turn), Some(started_at)) = (running, started_at) {
            self.turns[turn].start_ts = Some(started_at);
        }
    }

    /// First user message in the timeline, if any (used for session titles).
    pub fn first_user_message(&self) -> Option<&str> {
        self.entries.iter().find_map(|entry| match &entry.content {
            EntryContent::Item(ItemContent::UserMessage { text, .. })
            | EntryContent::Steer { text, .. } => Some(text.as_str()),
            _ => None,
        })
    }

    /// Apply one event recorded at `ts` (unix ms). Mutates in place.
    pub fn apply_at(&mut self, ts: Option<u64>, event: &AgentEvent) {
        // Root provenance wraps a canonical event; delegated updates are projected by the host.
        if let AgentEvent::Origin { agent_id, event, .. } = event {
            if agent_id.is_none() { self.apply_at(ts, event); }
            return;
        }
        if let AgentEvent::ContinuationStarted { turn_id, .. } = event {
            self.apply_at(ts, &AgentEvent::TurnStarted { turn_id: turn_id.clone() });
            return;
        }
        // Every timestamped event inside the open turn raises the turn's
        // watermark, not just tool lifecycle ones: a completion stamped before
        // any of them means the wall clock regressed, and the breakdown is
        // withheld rather than guessed at. The completion itself is excluded —
        // it is what gets compared against the watermark.
        if self.turn_is_open() && !matches!(event, AgentEvent::TurnCompleted { .. }) {
            self.tool_clock.observe(ts);
        }
        let boundary = self.boundaries.apply(event);
        if boundary.opened {
            self.push_turn(ts);
        }
        match event {
            AgentEvent::TurnRetry { retry } => {
                self.turn_retry = retry.clone();
            }
            AgentEvent::ProviderRelay {
                from_provider,
                from_model,
                to_provider,
                to_model,
                prompt,
                moved_from,
            } => {
                let turn = self.begin_user_turn();
                let id = self.synthetic_id("relay", ts);
                self.entries.push(Arc::new(TimelineEntry {
                    id,
                    content: EntryContent::ProviderRelay {
                        from_provider: *from_provider,
                        from_model: from_model.clone(),
                        to_provider: *to_provider,
                        to_model: to_model.clone(),
                        prompt: prompt.clone(),
                        moved_from: moved_from.clone(),
                    },
                    ts,
                    turn,
                }));
            }
            AgentEvent::SessionStarted {
                provider_session_id,
                resume,
                model,
            } => {
                self.provider_session_id = Some(provider_session_id.clone());
                self.resume = Some(resume.clone());
                if model.is_some() {
                    self.model = model.clone();
                }
                self.last_served_model = self.model.clone();
            }
            AgentEvent::ServedModel { model, reason } => {
                let turn = self.ensure_turn();
                self.turns[turn].served_model = Some(model.clone());
                // A `[1m]` launch id and the bare id the API reports back
                // name the same model, so that pair draws no divider.
                let same_model = self
                    .last_served_model
                    .as_deref()
                    .map(agent::claude::strip_context_window_suffix)
                    == Some(agent::claude::strip_context_window_suffix(model));
                if !same_model {
                    let from = self.last_served_model.clone();
                    let id = self.synthetic_id("model", ts);
                    self.entries.push(Arc::new(TimelineEntry {
                        id,
                        content: EntryContent::ModelChanged {
                            from,
                            to: model.clone(),
                            reason: reason.clone(),
                        },
                        ts,
                        turn,
                    }));
                }
                self.last_served_model = Some(model.clone());
            }
            AgentEvent::TurnStarted { turn_id } => {
                self.turn_retry = None;
                if let Some(usage) = self.usage.as_mut()
                    && usage.freshness == agent::ContextFreshness::Current
                {
                    usage.freshness = agent::ContextFreshness::LastKnown;
                }
                let turn = self.ensure_turn();
                // TurnStarted is the authoritative turn start; prefer it over
                // the opening user message's time when known. It is also the
                // only start the timing breakdown will measure from.
                if let Some(ts) = ts {
                    self.turns[turn].start_ts = Some(ts);
                    self.tool_clock.begin_turn(ts);
                }
                self.turns[turn].provider_turn_id = Some(turn_id.clone());
                self.turns[turn].running = true;
                self.turn_running = true;
                self.last_turn_status = None;
            }
            AgentEvent::TurnAccepted { .. }
            | AgentEvent::McpServersNotAttached
            | AgentEvent::BackgroundTasksChanged { .. }
            | AgentEvent::ModelFallbackDetected { .. }
            | AgentEvent::TurnBlocked { .. } => {}
            AgentEvent::TurnChangesUpdated {
                turn_id,
                changes,
                completeness,
            } => {
                let turn = boundary
                    .target
                    .expect("snapshot or checkpoint opens a turn");
                if !turn_id.is_empty() {
                    self.turns[turn].provider_turn_id = Some(turn_id.clone());
                }
                self.turns[turn].changes = Some(TurnChangeSet {
                    changes: changes.clone(),
                    completeness: *completeness,
                });
            }
            AgentEvent::TurnCheckpoint {
                turn_id,
                checkpoint_id,
            } => {
                let turn = boundary
                    .target
                    .expect("snapshot or checkpoint opens a turn");
                self.turns[turn].provider_turn_id = Some(turn_id.clone());
                self.turns[turn].provider_checkpoint_id = Some(checkpoint_id.clone());
            }
            AgentEvent::RewindCompleted {
                checkpoint_id,
                mode,
                ..
            } => {
                if mode.includes_conversation() {
                    if let Some(target) = boundary.rewind {
                        self.rewind_conversation(target);
                    } else {
                        log::warn!(
                            "provider rewind target is absent from the local timeline: {checkpoint_id}"
                        );
                    }
                }
            }
            AgentEvent::RewindFailed { .. } => {}
            AgentEvent::TurnCompleted { status, usage, .. } => {
                let newly_completed = self
                    .boundaries
                    .current()
                    .is_none_or(|turn| self.turns[turn].status.is_none());
                let usage = usage.map(|mut usage| {
                    usage.context_window = usage
                        .context_window
                        .or(self.usage.and_then(|u| u.context_window));
                    if let Some(processed) = usage.turn_processed_tokens {
                        let previous = self
                            .usage
                            .and_then(|u| u.total_processed_tokens)
                            .unwrap_or(0);
                        usage.total_processed_tokens =
                            Some(previous.saturating_add(if newly_completed {
                                processed
                            } else {
                                0
                            }));
                    }
                    usage
                });
                self.turn_running = false;
                self.last_turn_status = Some(*status);
                if let Some(turn) = self.boundaries.current() {
                    if ts.is_some() {
                        self.turns[turn].end_ts = ts;
                    }
                    self.turns[turn].status = Some(*status);
                    self.turns[turn].running = false;
                    if let Some(usage) = usage {
                        self.turns[turn].cost_usd = usage.cost_usd;
                        self.turns[turn].provider_duration_ms = usage.duration_ms;
                    }
                    let clock = std::mem::take(&mut self.tool_clock);
                    // A repeated completion finds an already-spent clock; it
                    // must not erase the breakdown the first one derived.
                    if let Some(timing) = finish_timing(clock, self.turns[turn].end_ts) {
                        self.turns[turn].timing = Some(timing);
                    }
                }
                if usage.is_some() {
                    self.usage = usage;
                }
                // A finished turn can no longer be waiting on approvals or input;
                // a background subagent it spawned still can.
                self.pending_approvals
                    .retain(|request| request.parent_item_id.is_some());
                self.pending_user_input = None;
            }
            AgentEvent::ItemStarted(item) => {
                self.upsert_item(ts, item);
                self.track_tool_item(ts, item, ToolLifecycle::Started);
            }
            AgentEvent::ItemUpdated(item) => {
                self.upsert_item(ts, item);
                self.track_tool_item(ts, item, ToolLifecycle::Updated);
            }
            AgentEvent::ItemCompleted(item) => {
                self.upsert_item(ts, item);
                self.track_tool_item(ts, item, ToolLifecycle::Completed);
                if matches!(
                    &item.content,
                    ItemContent::FileChange {
                        status: ItemStatus::Completed,
                        ..
                    }
                ) {
                    self.committed_file_change_items.insert(item.id.clone());
                    if let Some(turn) = self.item_turn(&item.id) {
                        self.refresh_partial_turn_changes(turn);
                    }
                }
            }
            AgentEvent::SteerRequested {
                request_id,
                text,
                attachments,
            } => self.request_steer(ts, request_id, text, attachments),
            AgentEvent::SteerAccepted { request_id } => {
                self.accept_steer(request_id, boundary.target)
            }
            AgentEvent::SteerRejected { request_id } => self.reject_steer(request_id),
            AgentEvent::Delta {
                item_id,
                kind,
                text,
            } => self.apply_delta(ts, item_id, *kind, text),
            AgentEvent::ApprovalRequested(request) => {
                if !self.pending_approvals.iter().any(|r| r.id == request.id) {
                    self.pending_approvals.push(request.clone());
                }
            }
            AgentEvent::ApprovalResolved { request_id, .. } => {
                self.pending_approvals.retain(|r| r.id != *request_id);
            }
            AgentEvent::UserInputRequested {
                request_id,
                questions,
                delivery,
            } => {
                self.pending_user_input = Some(PendingUserInput {
                    request_id: request_id.clone(),
                    questions: questions.clone(),
                    delivery: *delivery,
                });
            }
            AgentEvent::UserInputResolved { request_id, .. } => {
                if self
                    .pending_user_input
                    .as_ref()
                    .is_some_and(|pending| pending.request_id == *request_id)
                {
                    self.pending_user_input = None;
                }
            }
            AgentEvent::TokenUsage(usage) => {
                let mut usage = *usage;
                usage.context_window = usage
                    .context_window
                    .or(self.usage.and_then(|u| u.context_window));
                if usage.turn_processed_tokens.is_some() || usage.total_processed_tokens.is_none() {
                    usage.total_processed_tokens = self
                        .usage
                        .and_then(|u| u.total_processed_tokens)
                        .or(usage.total_processed_tokens);
                }
                self.usage = Some(usage);
            }
            // Logged once where the live provider event arrives; a fold also
            // replays stored records, which would log every old warning again.
            AgentEvent::Warning { .. } | AgentEvent::PlanResolved { .. } => {}
            AgentEvent::ProviderStartFailed { error } => {
                let turn = self.ensure_turn();
                let id = self.synthetic_id("error", ts);
                self.entries.push(Arc::new(TimelineEntry {
                    id,
                    content: EntryContent::ProviderStartError {
                        error: error.clone(),
                    },
                    ts,
                    turn,
                }));
            }
            AgentEvent::Error { message, .. } | AgentEvent::TurnFailed { message, .. } => {
                let turn = self.ensure_turn();
                let id = self.synthetic_id("error", ts);
                self.entries.push(Arc::new(TimelineEntry {
                    id,
                    content: EntryContent::Error {
                        message: message.clone(),
                        limit_resets_at: None,
                    },
                    ts,
                    turn,
                }));
            }
            AgentEvent::UsageLimitReached { resets_at } => {
                let Some((index, entry)) = self
                    .entries
                    .iter()
                    .enumerate()
                    .rev()
                    .find(|(_, entry)| matches!(entry.content, EntryContent::Error { .. }))
                else {
                    return;
                };
                let EntryContent::Error { message, .. } = &entry.content else {
                    unreachable!();
                };
                self.entries[index] = Arc::new(TimelineEntry {
                    id: entry.id.clone(),
                    content: EntryContent::Error {
                        message: message.clone(),
                        limit_resets_at: Some(*resets_at),
                    },
                    ts: entry.ts,
                    turn: entry.turn,
                });
            }
            AgentEvent::SessionClosed { reason } => {
                self.turn_retry = None;
                // An abnormal close carries the provider's dying words (exit
                // status, stderr tail). Fold them into the transcript so the
                // cause survives past the one-shot toast — a reopened session
                // must still show why the work stopped.
                if let Some(reason) = reason {
                    let turn = self.ensure_turn();
                    let id = self.synthetic_id("error", ts);
                    self.entries.push(Arc::new(TimelineEntry {
                        id,
                        content: EntryContent::Error {
                            message: reason.clone(),
                            limit_resets_at: None,
                        },
                        ts,
                        turn,
                    }));
                }
                self.turn_running = false;
                self.pending_approvals.clear();
                self.pending_user_input = None;
                if let Some(turn) = self.boundaries.current() {
                    self.turns[turn].running = false;
                }
            }
            AgentEvent::PlanUpdated {
                steps, explanation, ..
            } => {
                self.plan_steps = steps.clone();
                self.plan_explanation = explanation.clone();
            }
            AgentEvent::GoalUpdated { goal } => self.goal = goal.clone(),
            AgentEvent::ProposedPlanDelta { item_id, text } => {
                self.apply_delta(ts, item_id, DeltaKind::AssistantText, text);
            }
            AgentEvent::ProposedPlan { item_id, markdown } => {
                self.upsert_item(
                    ts,
                    &ThreadItem {
                        id: item_id.clone(),
                        parent_item_id: None,
                        content: ItemContent::AssistantMessage {
                            text: markdown.clone(),
                        },
                    },
                );
            }
            AgentEvent::ControlAction(action) => {
                let turn = self.ensure_turn();
                let id = self.synthetic_id("control", ts);
                self.entries.push(Arc::new(TimelineEntry {
                    id,
                    content: EntryContent::ControlAction(action.clone()),
                    ts,
                    turn,
                }));
            }
            AgentEvent::ContextCompacted(compaction) => {
                let in_progress = compaction.in_progress;
                let usage = self.usage.get_or_insert_with(Default::default);
                if in_progress && usage.freshness == agent::ContextFreshness::Compacting {
                    return;
                }
                usage.freshness = if in_progress {
                    agent::ContextFreshness::Compacting
                } else {
                    agent::ContextFreshness::AwaitingObservation
                };
                if !in_progress {
                    usage.used_tokens = None;
                    usage.input_tokens = None;
                    usage.cached_input_tokens = None;
                    usage.output_tokens = None;
                }
                let turn = self.ensure_turn();
                let id = self.synthetic_id("compacted", ts);
                self.entries.push(Arc::new(TimelineEntry {
                    id,
                    content: EntryContent::ContextCompacted(compaction.clone()),
                    ts,
                    turn,
                }));
            }
            AgentEvent::ContextWindowChanged { window } => {
                let turn = self.ensure_turn();
                let id = self.synthetic_id("context-window", ts);
                self.entries.push(Arc::new(TimelineEntry {
                    id,
                    content: EntryContent::ContextWindowChanged { window: *window },
                    ts,
                    turn,
                }));
            }
            // Composer metadata belongs to the runtime, not the timeline.
            AgentEvent::ProviderCommands { .. } | AgentEvent::ProviderOptions { .. }
            | AgentEvent::ProcessStarted { .. }
            | AgentEvent::NativeAgentUpdate { .. }
            | AgentEvent::RuntimeOperationResult { .. }
            | AgentEvent::RuntimeCapabilities { .. }
            | AgentEvent::ControlDelivery { .. }
            | AgentEvent::UsageDetail { .. }
            | AgentEvent::ContinuationStarted { .. }
            | AgentEvent::Origin { .. }
            | AgentEvent::BackgroundTaskList { .. }
            | AgentEvent::RewindPreviewed { .. }
            | AgentEvent::RewindUndone { .. }
            | AgentEvent::RewindUndoFailed { .. }
            | AgentEvent::UnreadableRecord { .. } => {}
        }
    }

    /// The turn a provider-addressed record lands on: the first turn carrying
    /// `turn_id`, else the current one. `None` means the record opens a turn.
    fn provider_turn(&self, turn_id: &str) -> Option<usize> {
        self.boundaries.provider_turn(turn_id)
    }

    /// Whether the current turn is still accumulating. A turn is finished once
    /// a `TurnCompleted` has been folded, which records a status even when the
    /// event carried no timestamp to store as `end_ts`; both must be checked or
    /// a stray later transition would leak into the next turn's accounting
    fn turn_is_open(&self) -> bool {
        self.boundaries.is_open()
    }

    /// Feed one item lifecycle transition to the open turn's clock, ignoring
    /// items that are not tool-like. Transitions arriving after the turn has
    /// already been finalized are ignored too, so a settled breakdown cannot be
    /// reopened.
    fn track_tool_item(&mut self, ts: Option<u64>, item: &ThreadItem, lifecycle: ToolLifecycle) {
        let Some(state) = tool_item_state(&item.content) else {
            return;
        };
        if self.turn_is_open() {
            self.tool_clock
                .mark(ts, &item.id, tool_is_active(lifecycle, state));
        }
    }

    /// Push a new (open) turn and make it current. `start_ts` seeds the turn's
    /// start time (refined later by a TurnStarted event if one arrives).
    fn push_turn(&mut self, start_ts: Option<u64>) {
        self.tool_clock = ToolClock::default();
        self.turns.push(TurnMeta {
            provider_turn_id: None,
            provider_checkpoint_id: None,
            start_ts,
            end_ts: None,
            status: None,
            running: false,
            changes: None,
            timing: None,
            served_model: None,
            cost_usd: None,
            provider_duration_ms: None,
        });
    }

    /// Apply a provider-confirmed conversation rewind. The event log remains
    /// append-only; replaying this marker produces the provider's authoritative
    /// active history without tcode rewriting either transcript file.
    fn rewind_conversation(&mut self, target_turn: usize) {
        self.entries.retain(|entry| entry.turn < target_turn);
        self.reindex_entries();
        self.turns.truncate(target_turn);
        self.tool_clock = ToolClock::default();
        self.turn_running = false;
        self.pending_approvals.clear();
        self.pending_user_input = None;
        self.plan_steps.clear();
        self.plan_explanation = None;
        self.usage = None;
        self.last_turn_status = self.turns.last().and_then(|turn| turn.status);
        self.committed_file_change_items
            .retain(|item_id| self.entries.iter().any(|entry| entry.id == *item_id));
    }

    fn ensure_turn(&self) -> usize {
        self.boundaries
            .current()
            .expect("the turn-opening policy supplied a turn")
    }

    fn begin_user_turn(&mut self) -> usize {
        self.turn_retry = None;
        self.ensure_turn()
    }

    /// Ids for entries that have no provider item. The UI pages history in
    /// from the tail, so an id must not depend on how many records precede it:
    /// a timestamped record names itself, and only untimestamped legacy logs
    /// fall back to the fold-relative counter.
    fn synthetic_id(&mut self, prefix: &str, ts: Option<u64>) -> String {
        let mut next = self.next_synthetic_id;
        let id = synthetic_entry_id(prefix, ts, &mut next, |id| self.first_entry(id).is_some());
        self.next_synthetic_id = next;
        id
    }

    fn refresh_partial_turn_changes(&mut self, turn: usize) {
        if self.turns[turn]
            .changes
            .as_ref()
            .is_some_and(|changes| changes.completeness == ChangeCompleteness::Exact)
        {
            return;
        }
        let fragments = self.entries.iter().filter_map(|entry| {
            if entry.turn != turn || !self.committed_file_change_items.contains(&entry.id) {
                return None;
            }
            match &entry.content {
                EntryContent::Item(ItemContent::FileChange { changes, .. }) => {
                    Some(changes.as_slice())
                }
                _ => None,
            }
        });
        let changes = merge_file_changes_by_path(fragments.flatten());
        self.turns[turn].changes = Some(TurnChangeSet {
            changes,
            completeness: ChangeCompleteness::Partial,
        });
    }

    fn item_turn(&mut self, item_id: &str) -> Option<usize> {
        self.first_entry(item_id)
            .map(|position| self.entries[position].turn)
    }

    fn upsert_item(&mut self, ts: Option<u64>, item: &ThreadItem) {
        // Runtime reroutes native-subagent child items into mirror sessions;
        // legacy logs still contain them inline — drop, never render.
        if item.parent_item_id.is_some() {
            return;
        }
        let mut incoming = EntryContent::Item(item.content.clone());
        if let Some(position) = self.first_entry(&item.id) {
            let entry = Arc::make_mut(&mut self.entries[position]);
            entry.content = merge_content(
                std::mem::replace(&mut entry.content, incoming.clone()),
                incoming,
            );
        } else {
            let turn = if matches!(
                incoming,
                EntryContent::Item(ItemContent::UserMessage { .. })
            ) {
                let turn = self.begin_user_turn();
                if self.entries.iter().any(|entry| {
                    entry.turn == turn
                        && matches!(
                            entry.content,
                            EntryContent::Item(ItemContent::UserMessage { .. })
                                | EntryContent::Steer { .. }
                        )
                }) {
                    // Legacy logs represented a steer as a second UserMessage
                    // item. Preserve their historical accepted rendering.
                    let EntryContent::Item(ItemContent::UserMessage {
                        text,
                        context_len,
                        attachments,
                    }) = incoming
                    else {
                        unreachable!();
                    };
                    incoming = EntryContent::Steer {
                        text,
                        status: SteeringStatus::Accepted,
                        context_len,
                        attachments,
                    };
                }
                turn
            } else {
                self.ensure_turn()
            };
            self.entries.push(Arc::new(TimelineEntry {
                id: item.id.clone(),
                content: incoming,
                ts,
                turn,
            }));
        }
    }

    fn request_steer(
        &mut self,
        ts: Option<u64>,
        request_id: &str,
        text: &str,
        attachments: &[String],
    ) {
        if self.first_entry(request_id).is_some() {
            return;
        }
        let turn = self.ensure_turn();
        self.entries.push(Arc::new(TimelineEntry {
            id: request_id.to_owned(),
            content: EntryContent::Steer {
                text: text.to_owned(),
                status: SteeringStatus::Pending,
                context_len: None,
                attachments: attachments.to_vec(),
            },
            ts,
            turn,
        }));
    }

    fn accept_steer(&mut self, request_id: &str, target: Option<usize>) {
        let Some(position) = self.first_entry(request_id) else {
            return;
        };
        if !matches!(
            self.entries[position].content,
            EntryContent::Steer {
                status: SteeringStatus::Pending,
                ..
            }
        ) {
            return;
        }

        let mut entry = self.entries.remove(position);
        self.reindex_entries();
        if let EntryContent::Steer {
            status: status @ SteeringStatus::Pending,
            ..
        } = &mut Arc::make_mut(&mut entry).content
        {
            *status = SteeringStatus::Accepted;
        }
        if let Some(turn) = target {
            Arc::make_mut(&mut entry).turn = turn;
        }
        self.entries.push(entry);
    }

    /// The provider never consumed the steer; the runtime re-sends its text as
    /// an ordinary message, which records its own user entry.
    fn reject_steer(&mut self, request_id: &str) {
        self.entries.retain(|entry| {
            entry.id != request_id
                || !matches!(
                    entry.content,
                    EntryContent::Steer {
                        status: SteeringStatus::Pending,
                        ..
                    }
                )
        });
        self.reindex_entries();
    }

    fn apply_delta(&mut self, ts: Option<u64>, item_id: &str, kind: DeltaKind, text: &str) {
        // A stream continues the item in the turn being streamed. Some
        // providers reuse a placeholder id for every turn's stream, so an
        // earlier turn's entry with the same id is a different item.
        let current_turn = self.boundaries.current();
        if let Some(entry) = self
            .entries
            .iter_mut()
            .rev()
            .take_while(|e| Some(e.turn) == current_turn)
            .find(|e| e.id == item_id)
        {
            let entry = Arc::make_mut(entry);
            match (&mut entry.content, kind) {
                (
                    EntryContent::Item(ItemContent::AssistantMessage { text: existing }),
                    DeltaKind::AssistantText,
                )
                | (
                    EntryContent::Item(ItemContent::Reasoning { text: existing }),
                    DeltaKind::ReasoningText,
                ) => {
                    existing.push_str(text);
                }
                (
                    EntryContent::Item(ItemContent::CommandExecution { output, .. }),
                    DeltaKind::CommandOutput,
                ) => {
                    output.push_str(text);
                }
                _ => log::warn!("delta kind {kind:?} does not match item {item_id}"),
            }
            return;
        }
        // Providers may stream deltas before announcing the item: create lazily.
        let content = match kind {
            DeltaKind::AssistantText => {
                EntryContent::Item(ItemContent::AssistantMessage { text: text.into() })
            }
            DeltaKind::ReasoningText => {
                EntryContent::Item(ItemContent::Reasoning { text: text.into() })
            }
            DeltaKind::CommandOutput => EntryContent::Item(ItemContent::CommandExecution {
                command: String::new(),
                output: text.into(),
                exit_code: None,
                status: ItemStatus::InProgress,
            }),
        };
        let turn = self.ensure_turn();
        self.entries.push(Arc::new(TimelineEntry {
            id: item_id.to_string(),
            content,
            ts,
            turn,
        }));
    }
}

/// The tool a dispatched child reports through, as every provider names it:
/// Claude Code `mcp__tcode_report__report_result`, Codex `tcode_report/report_result`.
pub fn is_report_result_tool(name: &str) -> bool {
    name.ends_with("report_result") && name.contains("tcode_report")
}

/// Merge an authoritative item snapshot over an existing entry, keeping
/// incremental text or file diffs when the snapshot omits them.
fn merge_content(existing: EntryContent, incoming: EntryContent) -> EntryContent {
    match (existing, incoming) {
        (
            EntryContent::Steer { status, .. },
            EntryContent::Item(ItemContent::UserMessage {
                text,
                context_len,
                attachments,
            }),
        ) => EntryContent::Steer {
            text,
            status,
            context_len,
            attachments,
        },
        (
            EntryContent::Item(ItemContent::AssistantMessage { text: old }),
            EntryContent::Item(ItemContent::AssistantMessage { text: new }),
        ) => EntryContent::Item(ItemContent::AssistantMessage {
            text: merge_text(old, new),
        }),
        (
            EntryContent::Item(ItemContent::Reasoning { text: old }),
            EntryContent::Item(ItemContent::Reasoning { text: new }),
        ) => EntryContent::Item(ItemContent::Reasoning {
            text: merge_text(old, new),
        }),
        (
            EntryContent::Item(ItemContent::CommandExecution {
                output: old_output, ..
            }),
            EntryContent::Item(ItemContent::CommandExecution {
                command,
                output,
                exit_code,
                status,
            }),
        ) => EntryContent::Item(ItemContent::CommandExecution {
            command,
            output: merge_text(old_output, output),
            exit_code,
            status,
        }),
        (
            EntryContent::Item(ItemContent::FileChange {
                changes: existing_changes,
                ..
            }),
            EntryContent::Item(ItemContent::FileChange {
                mut changes,
                status,
            }),
        ) => {
            for change in &mut changes {
                if change.diff.is_none()
                    && let Some(existing_diff) = existing_changes
                        .iter()
                        .find(|existing| existing.path == change.path)
                        .and_then(|existing| existing.diff.as_ref())
                {
                    change.diff = Some(existing_diff.clone());
                }
            }
            EntryContent::Item(ItemContent::FileChange { changes, status })
        }
        (
            EntryContent::Item(ItemContent::Subagent {
                summary: old_summary,
                model: old_model,
                effort: old_effort,
                ..
            }),
            EntryContent::Item(ItemContent::Subagent {
                agent_type,
                description,
                status,
                summary,
                model,
                effort,
            }),
        ) => EntryContent::Item(ItemContent::Subagent {
            agent_type,
            description,
            status,
            summary: summary.or(old_summary),
            model: model.or(old_model),
            effort: effort.or(old_effort),
        }),
        (_, incoming) => incoming,
    }
}

/// Merge an item snapshot's text (`new`) over text already accumulated from
/// deltas (`old`).
///
/// Snapshots (`ItemStarted` / `ItemUpdated` / `ItemCompleted`) are authoritative
/// when they carry text, but they can *lag* the delta stream: providers emit an
/// item snapshot holding the text so far while more deltas are still arriving.
/// Three rules:
///
/// * an empty snapshot never clobbers accumulated text;
/// * a snapshot that is only a prefix of what the deltas already produced (a
///   lagging/partial snapshot) never shortens it — shortening would make the
///   next delta look like a fresh append and duplicate the overlapping text;
/// * a snapshot with different text replaces (never concatenates onto) the
///   accumulated text.
fn merge_text(old: String, new: String) -> String {
    if new.is_empty() || old.starts_with(new.as_str()) {
        old
    } else {
        new
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent::{ApprovalDecision, ApprovalKind, FileChangeKind, RewindMode};
    use serde_json::json;

    fn user_msg(id: &str, text: &str) -> AgentEvent {
        AgentEvent::ItemCompleted(ThreadItem {
            id: id.into(),
            parent_item_id: None,
            content: ItemContent::UserMessage {
                text: text.into(),
                context_len: None,
                attachments: Vec::new(),
            },
        })
    }

    #[test]
    fn usage_replay_without_timestamps_keeps_turn_totals_and_known_capacity() {
        let usage = |processed| TokenUsage {
            freshness: agent::ContextFreshness::Current,
            used_tokens: Some(550),
            turn_processed_tokens: processed,
            ..Default::default()
        };
        let timeline = Timeline::fold_events([
            AgentEvent::TurnStarted {
                turn_id: "one".into(),
            },
            AgentEvent::TokenUsage(TokenUsage {
                context_window: Some(1000000),
                ..usage(None)
            }),
            AgentEvent::TurnCompleted {
                turn_id: "one".into(),
                status: TurnStatus::Completed,
                usage: Some(usage(Some(100))),
            },
            AgentEvent::TurnStarted {
                turn_id: "two".into(),
            },
            AgentEvent::TokenUsage(usage(None)),
            AgentEvent::TurnCompleted {
                turn_id: "two".into(),
                status: TurnStatus::Interrupted,
                usage: Some(usage(Some(20))),
            },
        ]);
        let usage = timeline.usage.unwrap();
        assert_eq!(usage.total_processed_tokens, Some(120));
        assert_eq!(usage.context_window, Some(1000000));
        assert_eq!(timeline.turns.len(), 2);
    }

    #[test]
    fn provider_relay_marker_folds_before_the_next_user_message() {
        let timeline = Timeline::fold_events([
            user_msg("u1", "before"),
            AgentEvent::TurnCompleted {
                turn_id: "turn-1".into(),
                status: TurnStatus::Completed,
                usage: None,
            },
            AgentEvent::ProviderRelay {
                from_provider: agent::ProviderKind::ClaudeCode,
                from_model: Some("opus".into()),
                to_provider: agent::ProviderKind::Codex,
                to_model: Some("gpt-5".into()),
                prompt: None,
                moved_from: None,
            },
            user_msg("u2", "after"),
        ]);

        assert_eq!(timeline.turns.len(), 2);
        assert!(matches!(
            timeline.entries[1].content,
            EntryContent::ProviderRelay {
                from_provider: agent::ProviderKind::ClaudeCode,
                to_provider: agent::ProviderKind::Codex,
                ..
            }
        ));
        assert_eq!(timeline.entries[1].turn, timeline.entries[2].turn);
        assert!(matches!(
            &timeline.entries[2].content,
            EntryContent::Item(ItemContent::UserMessage { text, .. }) if text == "after"
        ));
    }

    #[test]
    fn repeated_compacting_status_announces_one_compaction() {
        // Claude re-sends `status: compacting` per compaction phase and as a
        // 30s keepalive while one compaction runs.
        let compacting = || {
            AgentEvent::ContextCompacted(agent::Compaction {
                in_progress: true,
                ..Default::default()
            })
        };
        let compacted = || {
            AgentEvent::ContextCompacted(agent::Compaction {
                trigger: Some("auto".into()),
                ..Default::default()
            })
        };
        let timeline = Timeline::fold_events([
            user_msg("u1", "go"),
            compacting(),
            compacting(),
            compacting(),
            compacted(),
            compacting(),
            compacting(),
            compacted(),
        ]);

        let compactions: Vec<bool> = timeline
            .entries
            .iter()
            .filter_map(|entry| match &entry.content {
                EntryContent::ContextCompacted(c) => Some(c.in_progress),
                _ => None,
            })
            .collect();
        assert_eq!(compactions, [true, false, true, false]);
    }

    #[test]
    fn context_window_change_folds_into_the_timeline() {
        let timeline =
            Timeline::fold_events([AgentEvent::ContextWindowChanged { window: 500_000 }]);

        assert!(timeline.entries.iter().any(|entry| matches!(
            entry.content,
            EntryContent::ContextWindowChanged { window: 500_000 }
        )));
    }

    #[test]
    fn structured_user_input_blocks_until_resolution_or_turn_end() {
        let request = AgentEvent::UserInputRequested {
            request_id: "que_1".into(),
            questions: vec![UserInputQuestion {
                id: "que_1:0".into(),
                header: "Scope".into(),
                question: "Which crate?".into(),
                options: Vec::new(),
                multi_select: false,
                prefill: None,
            }],
            delivery: UserInputDelivery::Blocking,
        };
        let mut timeline = Timeline::default();
        timeline.apply_at(None, &request);
        assert_eq!(
            timeline
                .pending_user_input
                .as_ref()
                .map(|pending| pending.request_id.as_str()),
            Some("que_1")
        );

        timeline.apply_at(
            None,
            &AgentEvent::UserInputResolved {
                request_id: "que_1".into(),
                answers: serde_json::Map::new(),
            },
        );
        assert!(timeline.pending_user_input.is_none());

        timeline.apply_at(None, &request);
        timeline.apply_at(
            None,
            &AgentEvent::TurnCompleted {
                turn_id: "opencode-1".into(),
                status: TurnStatus::Interrupted,
                usage: None,
            },
        );
        assert!(timeline.pending_user_input.is_none());
    }

    #[test]
    fn provider_conversation_rewind_is_an_append_only_timeline_marker() {
        let mut events = Vec::new();
        for index in 1..=3 {
            events.extend([
                user_msg(&format!("user-{index}"), &format!("prompt {index}")),
                AgentEvent::TurnStarted {
                    turn_id: format!("turn-{index}"),
                },
                AgentEvent::TurnCheckpoint {
                    turn_id: format!("turn-{index}"),
                    checkpoint_id: format!("checkpoint-{index}"),
                },
                AgentEvent::TurnCompleted {
                    turn_id: format!("turn-{index}"),
                    status: TurnStatus::Completed,
                    usage: None,
                },
            ]);
        }
        events.push(AgentEvent::RewindCompleted {
            checkpoint_id: "checkpoint-2".into(),
            mode: RewindMode::Conversation,
            prefill: Some("prompt 2".into()),
        });

        let mut timeline = Timeline::fold_events(events);
        assert_eq!(timeline.turns.len(), 1);
        assert_eq!(timeline.entries.len(), 1);
        assert!(matches!(
            &timeline.entries[0].content,
            EntryContent::Item(ItemContent::UserMessage { text, .. }) if text == "prompt 1"
        ));
        assert_eq!(
            timeline.turns[0].provider_checkpoint_id.as_deref(),
            Some("checkpoint-1")
        );

        // New work after the marker opens a fresh turn; removed history does
        // not reappear even though the underlying JSONL remains append-only.
        timeline.apply_at(None, &user_msg("user-4", "replacement prompt"));
        assert_eq!(timeline.turns.len(), 2);
        assert!(matches!(
            &timeline.entries[1].content,
            EntryContent::Item(ItemContent::UserMessage { text, .. }) if text == "replacement prompt"
        ));
    }

    #[test]
    fn cloned_timeline_entries_are_shared_until_updated() {
        let mut timeline = Timeline::fold_events([user_msg("user-1", "before")]);
        let snapshot = timeline.clone();

        assert!(Arc::ptr_eq(&timeline.entries[0], &snapshot.entries[0]));

        timeline.apply_at(None, &user_msg("user-1", "after"));

        assert!(!Arc::ptr_eq(&timeline.entries[0], &snapshot.entries[0]));
        assert!(matches!(
            &snapshot.entries[0].content,
            EntryContent::Item(ItemContent::UserMessage { text, .. }) if text == "before"
        ));
        assert!(matches!(
            &timeline.entries[0].content,
            EntryContent::Item(ItemContent::UserMessage { text, .. }) if text == "after"
        ));
    }

    #[test]
    fn partial_turn_changes_include_only_successful_top_level_file_operations() {
        for (label, parent, status, included) in [
            ("completed", None, ItemStatus::Completed, true),
            ("failed", None, ItemStatus::Failed, false),
            ("in progress", None, ItemStatus::InProgress, false),
            ("child", Some("spawn-1"), ItemStatus::Completed, false),
        ] {
            let mut timeline =
                Timeline::fold_events([user_msg("user-1", "edit it"), turn_started()]);
            for (id, path, diff) in [
                (
                    "edit-1",
                    "src/lib.rs",
                    "--- a/src/lib.rs\n+++ b/src/lib.rs\n-old\n+middle\n",
                ),
                ("edit-2", "other.rs", "-gone\n+new\n"),
                ("edit-3", "src/lib.rs", "-middle\n-final\n+replacement\n"),
            ] {
                timeline.apply_at(
                    None,
                    &AgentEvent::ItemCompleted(ThreadItem {
                        id: id.into(),
                        parent_item_id: parent.map(str::to_string),
                        content: ItemContent::FileChange {
                            changes: vec![FileChange {
                                path: path.into(),
                                kind: FileChangeKind::Modify,
                                diff: Some(diff.into()),
                            }],
                            status,
                        },
                    }),
                );
            }
            let changes = &timeline.turns[0].changes;
            assert_eq!(changes.is_some(), included, "{label}");
            if let Some(changes) = changes {
                assert_eq!(changes.completeness, ChangeCompleteness::Partial);
                assert_eq!(
                    changes
                        .changes
                        .iter()
                        .map(|change| (change.path.as_str(), change.diff.as_deref()))
                        .collect::<Vec<_>>(),
                    [
                        (
                            "src/lib.rs",
                            Some(
                                "--- a/src/lib.rs\n+++ b/src/lib.rs\n-old\n+middle\n-middle\n-final\n+replacement\n"
                            )
                        ),
                        ("other.rs", Some("-gone\n+new\n")),
                    ],
                    "{label}"
                );
            }
        }
    }

    #[test]
    fn completed_file_snapshot_keeps_diff_from_started_snapshot() {
        let path = "/tmp/tcode-outside-workspace.txt";
        let timeline = Timeline::fold_events([
            user_msg("user-1", "write it"),
            AgentEvent::ItemStarted(ThreadItem {
                id: "write-external".into(),
                parent_item_id: None,
                content: ItemContent::FileChange {
                    changes: vec![FileChange {
                        path: path.into(),
                        kind: FileChangeKind::Create,
                        diff: Some("+visible diff".into()),
                    }],
                    status: ItemStatus::InProgress,
                },
            }),
            AgentEvent::ItemCompleted(ThreadItem {
                id: "write-external".into(),
                parent_item_id: None,
                content: ItemContent::FileChange {
                    changes: vec![FileChange {
                        path: path.into(),
                        kind: FileChangeKind::Create,
                        diff: None,
                    }],
                    status: ItemStatus::Completed,
                },
            }),
        ]);

        let change_set = timeline.turns[0]
            .changes
            .as_ref()
            .expect("completed change set");
        assert_eq!(change_set.changes.len(), 1);
        assert_eq!(change_set.changes[0].path, path);
        assert_eq!(change_set.changes[0].diff.as_deref(), Some("+visible diff"));
    }

    #[test]
    fn exact_turn_snapshot_replaces_partial_operations_and_survives_late_items() {
        let mut timeline = Timeline::fold_events([
            user_msg("user-1", "edit it"),
            AgentEvent::TurnStarted {
                turn_id: "turn-1".into(),
            },
            AgentEvent::ItemCompleted(ThreadItem {
                id: "edit-1".into(),
                parent_item_id: None,
                content: ItemContent::FileChange {
                    changes: vec![FileChange {
                        path: "src/lib.rs".into(),
                        kind: FileChangeKind::Modify,
                        diff: Some("-intermediate\n+value\n".into()),
                    }],
                    status: ItemStatus::Completed,
                },
            }),
        ]);

        timeline.apply_at(
            None,
            &AgentEvent::TurnChangesUpdated {
                turn_id: "turn-1".into(),
                changes: vec![FileChange {
                    path: "src/lib.rs".into(),
                    kind: FileChangeKind::Modify,
                    diff: Some("-before\n+after\n".into()),
                }],
                completeness: ChangeCompleteness::Exact,
            },
        );
        timeline.apply_at(
            None,
            &AgentEvent::ItemCompleted(ThreadItem {
                id: "edit-2".into(),
                parent_item_id: None,
                content: ItemContent::FileChange {
                    changes: vec![FileChange {
                        path: "late.txt".into(),
                        kind: FileChangeKind::Create,
                        diff: Some("+late\n".into()),
                    }],
                    status: ItemStatus::Completed,
                },
            }),
        );

        let change_set = timeline.turns[0].changes.as_ref().unwrap();
        assert_eq!(change_set.completeness, ChangeCompleteness::Exact);
        assert_eq!(change_set.changes.len(), 1);
        assert_eq!(change_set.changes[0].path, "src/lib.rs");
        assert_eq!(
            change_set.changes[0].diff.as_deref(),
            Some("-before\n+after\n")
        );
    }

    #[test]
    fn delayed_turn_snapshot_attaches_by_provider_turn_id() {
        let mut timeline = Timeline::fold_events([
            user_msg("user-1", "first"),
            AgentEvent::TurnStarted {
                turn_id: "turn-1".into(),
            },
            AgentEvent::TurnCompleted {
                turn_id: "turn-1".into(),
                status: TurnStatus::Completed,
                usage: None,
            },
            user_msg("user-2", "second"),
            AgentEvent::TurnStarted {
                turn_id: "turn-2".into(),
            },
        ]);
        timeline.apply_at(
            None,
            &AgentEvent::TurnChangesUpdated {
                turn_id: "turn-1".into(),
                changes: vec![FileChange {
                    path: "first.txt".into(),
                    kind: FileChangeKind::Create,
                    diff: Some("+first\n".into()),
                }],
                completeness: ChangeCompleteness::Exact,
            },
        );

        assert_eq!(
            timeline.turns[0].changes.as_ref().unwrap().changes[0].path,
            "first.txt"
        );
        assert!(timeline.turns[1].changes.is_none());
    }

    #[test]
    fn fold_marks_only_mid_turn_user_messages_as_steered() {
        let events = vec![
            user_msg("user-a", "A"),
            AgentEvent::TurnStarted {
                turn_id: "t1".into(),
            },
            AgentEvent::ItemCompleted(ThreadItem {
                id: "assistant-a".into(),
                parent_item_id: None,
                content: ItemContent::AssistantMessage {
                    text: "working".into(),
                },
            }),
            user_msg("user-b", "B"),
            AgentEvent::TurnCompleted {
                turn_id: "t1".into(),
                status: TurnStatus::Completed,
                usage: None,
            },
            user_msg("user-c", "C"),
            AgentEvent::TurnStarted {
                turn_id: "t2".into(),
            },
        ];
        let timeline = Timeline::fold_events(events);
        let users: Vec<(&str, Option<SteeringStatus>)> = timeline
            .entries
            .iter()
            .filter_map(|entry| match &entry.content {
                EntryContent::Item(ItemContent::UserMessage { text, .. }) => {
                    Some((text.as_str(), None))
                }
                EntryContent::Steer { text, status, .. } => Some((text.as_str(), Some(*status))),
                _ => None,
            })
            .collect();

        assert_eq!(
            users,
            vec![
                ("A", None),
                ("B", Some(SteeringStatus::Accepted)),
                ("C", None),
            ]
        );
    }

    #[test]
    fn correlated_steering_replays_pending_then_only_matching_acceptance() {
        let request = AgentEvent::SteerRequested {
            request_id: "steer-a".into(),
            text: "change direction".into(),
            attachments: Vec::new(),
        };
        let encoded = serde_json::to_string(&request).unwrap();
        let decoded: AgentEvent = serde_json::from_str(&encoded).unwrap();
        let mut timeline = Timeline::fold_events([
            user_msg("user-a", "start"),
            AgentEvent::TurnStarted {
                turn_id: "turn-a".into(),
            },
            decoded,
        ]);

        assert!(matches!(
            &timeline.entries[1].content,
            EntryContent::Steer {
                text,
                status: SteeringStatus::Pending,
                ..
            } if text == "change direction"
        ));

        timeline.apply_at(
            None,
            &AgentEvent::SteerAccepted {
                request_id: "steer-b".into(),
            },
        );
        assert!(matches!(
            timeline.entries[1].content,
            EntryContent::Steer {
                status: SteeringStatus::Pending,
                ..
            }
        ));

        let accepted = AgentEvent::SteerAccepted {
            request_id: "steer-a".into(),
        };
        let accepted: AgentEvent =
            serde_json::from_str(&serde_json::to_string(&accepted).unwrap()).unwrap();
        timeline.apply_at(None, &accepted);
        assert!(matches!(
            timeline.entries[1].content,
            EntryContent::Steer {
                status: SteeringStatus::Accepted,
                ..
            }
        ));

        // A restart folds the persisted request and acceptance to the same
        // accepted state; confirmation cannot regress to pending on replay.
        let replayed = Timeline::fold_events([
            user_msg("user-a", "start"),
            AgentEvent::TurnStarted {
                turn_id: "turn-a".into(),
            },
            request,
            accepted,
        ]);
        assert!(matches!(
            replayed.entries[1].content,
            EntryContent::Steer {
                status: SteeringStatus::Accepted,
                ..
            }
        ));
    }

    #[test]
    fn rejected_steer_leaves_the_timeline_live_and_on_replay() {
        let events = [
            user_msg("user-a", "start"),
            AgentEvent::TurnStarted {
                turn_id: "turn-a".into(),
            },
            AgentEvent::SteerRequested {
                request_id: "steer-a".into(),
                text: "late".into(),
                attachments: Vec::new(),
            },
            AgentEvent::TurnCompleted {
                turn_id: "turn-a".into(),
                status: TurnStatus::Completed,
                usage: None,
            },
            AgentEvent::SteerRejected {
                request_id: "steer-a".into(),
            },
        ];
        let mut live = Timeline::fold_events(events[..4].iter().cloned());
        assert!(live.entries.iter().any(|entry| entry.id == "steer-a"));
        live.apply_at(None, &events[4]);
        assert!(!live.entries.iter().any(|entry| entry.id == "steer-a"));

        let replayed = Timeline::fold_events(events);
        assert!(!replayed.entries.iter().any(|entry| entry.id == "steer-a"));
    }

    #[test]
    fn accepted_steer_moves_to_its_consumption_position_live_and_on_replay() {
        let assistant_item = |id: &str| {
            AgentEvent::ItemCompleted(ThreadItem {
                id: id.into(),
                parent_item_id: None,
                content: ItemContent::AssistantMessage { text: id.into() },
            })
        };
        let events = vec![
            user_msg("user-a", "start"),
            AgentEvent::TurnStarted {
                turn_id: "turn-a".into(),
            },
            AgentEvent::SteerRequested {
                request_id: "S".into(),
                text: "change direction".into(),
                attachments: Vec::new(),
            },
            assistant_item("A"),
            assistant_item("B"),
            AgentEvent::SteerAccepted {
                request_id: "S".into(),
            },
            assistant_item("C"),
            assistant_item("D"),
        ];

        let mut live = Timeline::default();
        for event in &events {
            live.apply_at(None, event);
        }
        let replayed = Timeline::fold_events(events);

        let live_ids: Vec<&str> = live.entries.iter().map(|entry| entry.id.as_str()).collect();
        let replayed_ids: Vec<&str> = replayed
            .entries
            .iter()
            .map(|entry| entry.id.as_str())
            .collect();
        assert_eq!(live_ids, ["user-a", "A", "B", "S", "C", "D"]);
        assert_eq!(replayed_ids, live_ids);
        assert!(matches!(
            live.entries[3].content,
            EntryContent::Steer {
                status: SteeringStatus::Accepted,
                ..
            }
        ));
        assert!(matches!(
            replayed.entries[3].content,
            EntryContent::Steer {
                status: SteeringStatus::Accepted,
                ..
            }
        ));
        assert_eq!(replayed.entries[3].turn, live.entries[3].turn);
    }

    fn assistant_delta(id: &str, text: &str) -> AgentEvent {
        AgentEvent::Delta {
            item_id: id.into(),
            kind: DeltaKind::AssistantText,
            text: text.into(),
        }
    }

    #[test]
    fn streamed_item_snapshots_preserve_partial_text_and_accept_rewrites() {
        for kind in [
            DeltaKind::AssistantText,
            DeltaKind::ReasoningText,
            DeltaKind::CommandOutput,
        ] {
            let snapshot = |text: &str| {
                AgentEvent::ItemUpdated(ThreadItem {
                    id: "msg".into(),
                    parent_item_id: None,
                    content: match kind {
                        DeltaKind::AssistantText => {
                            ItemContent::AssistantMessage { text: text.into() }
                        }
                        DeltaKind::ReasoningText => ItemContent::Reasoning { text: text.into() },
                        DeltaKind::CommandOutput => ItemContent::CommandExecution {
                            command: "echo hi".into(),
                            output: text.into(),
                            exit_code: Some(0),
                            status: ItemStatus::Completed,
                        },
                    },
                })
            };
            let mut timeline = Timeline::fold_events([
                AgentEvent::Delta {
                    item_id: "msg".into(),
                    kind,
                    text: "第一段\n".into(),
                },
                AgentEvent::Delta {
                    item_id: "msg".into(),
                    kind,
                    text: "second".into(),
                },
            ]);
            for (incoming, expected) in [
                ("第一段\n", "第一段\nsecond"),
                ("", "第一段\nsecond"),
                ("第一段\nsecond", "第一段\nsecond"),
                ("rewritten", "rewritten"),
            ] {
                timeline.apply_at(None, &snapshot(incoming));
                assert_eq!(timeline.entries.len(), 1);
                let text = match &timeline.entries[0].content {
                    EntryContent::Item(
                        ItemContent::AssistantMessage { text } | ItemContent::Reasoning { text },
                    ) => text,
                    EntryContent::Item(ItemContent::CommandExecution {
                        command,
                        output,
                        exit_code,
                        status,
                    }) => {
                        assert_eq!(command, "echo hi");
                        assert_eq!(*exit_code, Some(0));
                        assert_eq!(*status, ItemStatus::Completed);
                        output
                    }
                    other => panic!("unexpected item: {other:?}"),
                };
                assert_eq!(text, expected, "{kind:?}: snapshot {incoming:?}");
            }
        }
    }

    /// Modeled on crates/agent/tests/fixtures/codex/v2_messages.jsonl:
    /// file-change item + approval + deltas for message/reasoning/command output.
    #[test]
    fn fold_codex_style_trace_with_approval() {
        let changes = vec![FileChange {
            path: "/tmp/probe-codex/hello.txt".into(),
            kind: FileChangeKind::Create,
            diff: Some("hi\n".into()),
        }];
        let mut timeline = Timeline::default();
        timeline.apply_at(
            None,
            &AgentEvent::TurnStarted {
                turn_id: "turn-1".into(),
            },
        );
        timeline.apply_at(
            None,
            &AgentEvent::ItemStarted(ThreadItem {
                id: "patch-1".into(),
                parent_item_id: None,
                content: ItemContent::FileChange {
                    changes: changes.clone(),
                    status: ItemStatus::InProgress,
                },
            }),
        );
        timeline.apply_at(
            None,
            &AgentEvent::ApprovalRequested(ApprovalRequest {
                id: "41".into(),
                parent_item_id: None,
                turn_id: Some("turn-1".into()),
                kind: ApprovalKind::FileChange {
                    changes: changes.clone(),
                    reason: None,
                },
                options: Vec::new(),
            }),
        );

        assert!(timeline.turn_running);
        assert_eq!(timeline.pending_approvals.len(), 1);

        timeline.apply_at(
            None,
            &AgentEvent::ApprovalResolved {
                request_id: "41".into(),
                decision: ApprovalDecision::Option("accept".into()),
            },
        );
        assert!(timeline.pending_approvals.is_empty());

        // Deltas create items lazily.
        timeline.apply_at(
            None,
            &AgentEvent::Delta {
                item_id: "message-1".into(),
                kind: DeltaKind::AssistantText,
                text: "PONG".into(),
            },
        );
        timeline.apply_at(
            None,
            &AgentEvent::Delta {
                item_id: "reasoning-1".into(),
                kind: DeltaKind::ReasoningText,
                text: "Checking".into(),
            },
        );
        timeline.apply_at(
            None,
            &AgentEvent::Delta {
                item_id: "command-1".into(),
                kind: DeltaKind::CommandOutput,
                text: "ok\n".into(),
            },
        );
        timeline.apply_at(
            None,
            &AgentEvent::TokenUsage(TokenUsage {
                used_tokens: Some(123),
                context_window: Some(200000),
                ..Default::default()
            }),
        );
        timeline.apply_at(
            None,
            &AgentEvent::ItemCompleted(ThreadItem {
                id: "patch-1".into(),
                parent_item_id: None,
                content: ItemContent::FileChange {
                    changes: changes.clone(),
                    status: ItemStatus::Completed,
                },
            }),
        );
        timeline.apply_at(
            None,
            &AgentEvent::TurnCompleted {
                turn_id: "turn-1".into(),
                status: TurnStatus::Completed,
                usage: None,
            },
        );

        assert!(!timeline.turn_running);
        assert_eq!(timeline.entries.len(), 4);
        assert!(matches!(
            &timeline.entries[0].content,
            EntryContent::Item(ItemContent::FileChange { changes, .. })
                if changes.len() == 1 && changes[0].path.ends_with("hello.txt")
        ));
        assert!(matches!(
            &timeline.entries[1].content,
            EntryContent::Item(ItemContent::AssistantMessage { text }) if text == "PONG"
        ));
        assert!(matches!(
            &timeline.entries[2].content,
            EntryContent::Item(ItemContent::Reasoning { text }) if text == "Checking"
        ));
        assert!(matches!(
            &timeline.entries[3].content,
            EntryContent::Item(ItemContent::CommandExecution { output, .. }) if output == "ok\n"
        ));
        assert_eq!(timeline.usage.unwrap().used_tokens, Some(123));
    }

    #[test]
    fn timestamps_and_turn_grouping_fold_across_two_exchanges() {
        let mut timeline = Timeline::fold_events([
            at(
                999_900,
                AgentEvent::SessionStarted {
                    provider_session_id: "78b7774c".into(),
                    resume: ResumeCursor(json!({ "session_id": "78b7774c" })),
                    model: Some("claude-opus-4-8".into()),
                },
            ),
            at(1_000_000, user_msg("u1", "first")),
            at(
                1_000_500,
                AgentEvent::TurnStarted {
                    turn_id: "t1".into(),
                },
            ),
            at(1_001_000, assistant_delta("a1", "Hi! ")),
            at(
                1_001_500,
                assistant_delta("a1", "How can I help you today?"),
            ),
            at(1_002_000, assistant("a1", "Hi! How can I help you today?")),
            at(
                1_005_500,
                AgentEvent::TurnCompleted {
                    turn_id: "t1".into(),
                    status: TurnStatus::Completed,
                    usage: Some(TokenUsage {
                        input_tokens: Some(3355),
                        output_tokens: Some(17),
                        ..Default::default()
                    }),
                },
            ),
        ]);
        assert_eq!(timeline.entries.len(), 2);
        assert!(matches!(
            &timeline.entries[0].content,
            EntryContent::Item(ItemContent::UserMessage { text, .. }) if text == "first"
        ));
        assert!(matches!(
            &timeline.entries[1].content,
            EntryContent::Item(ItemContent::AssistantMessage { text }) if text == "Hi! How can I help you today?"
        ));
        assert!(!timeline.turn_running);
        assert_eq!(timeline.last_turn_status, Some(TurnStatus::Completed));
        assert_eq!(timeline.usage.as_ref().unwrap().output_tokens, Some(17));
        assert_eq!(timeline.model.as_deref(), Some("claude-opus-4-8"));
        assert_eq!(
            timeline.resume,
            Some(ResumeCursor(json!({ "session_id": "78b7774c" })))
        );
        assert_eq!(timeline.first_user_message(), Some("first"));

        timeline.apply_stored(&at(2_000_000, user_msg("u2", "second")));
        timeline.apply_stored(&at(
            2_000_400,
            AgentEvent::TurnStarted {
                turn_id: "t2".into(),
            },
        ));
        assert_eq!(timeline.turns.len(), 2);
        assert_eq!(timeline.turns[0].start_ts, Some(1_000_500));
        assert_eq!(timeline.turns[0].end_ts, Some(1_005_500));
        assert_eq!(timeline.turns[0].status, Some(TurnStatus::Completed));
        assert!(!timeline.turns[0].running);
        assert!(timeline.turns[1].running);
        assert!(timeline.turn_running);

        let u1 = &timeline.entries[0];
        assert_eq!(u1.ts, Some(1_000_000));
        assert_eq!(u1.turn, 0);
        let a1 = &timeline.entries[1];
        assert_eq!(a1.turn, 0);
        let u2 = timeline
            .entries
            .iter()
            .find(|e| matches!(&e.content, EntryContent::Item(ItemContent::UserMessage { text, .. }) if text == "second"))
            .unwrap();
        assert_eq!(u2.turn, 1);

        let mut cold = timeline.clone();
        cold.mark_idle();
        assert!(!cold.turn_running);
        assert!(cold.turns.iter().all(|t| !t.running));
    }

    #[test]
    fn plan_updated_tracks_latest_steps() {
        use agent::PlanStepStatus;
        let mut timeline = Timeline::default();
        timeline.apply_at(
            None,
            &AgentEvent::PlanUpdated {
                turn_id: Some("t".into()),
                explanation: Some("Working".into()),
                steps: vec![
                    PlanStep {
                        step: "a".into(),
                        status: PlanStepStatus::Completed,
                    },
                    PlanStep {
                        step: "b".into(),
                        status: PlanStepStatus::InProgress,
                    },
                ],
            },
        );
        assert_eq!(
            timeline.plan_steps,
            [
                PlanStep {
                    step: "a".into(),
                    status: PlanStepStatus::Completed
                },
                PlanStep {
                    step: "b".into(),
                    status: PlanStepStatus::InProgress
                },
            ]
        );
        assert_eq!(timeline.plan_explanation.as_deref(), Some("Working"));
        for explanation in [Some("Revised"), None] {
            timeline.apply_at(
                None,
                &AgentEvent::PlanUpdated {
                    turn_id: Some("t".into()),
                    explanation: explanation.map(str::to_string),
                    steps: vec![PlanStep {
                        step: "replacement".into(),
                        status: PlanStepStatus::Pending,
                    }],
                },
            );
            assert_eq!(
                timeline.plan_steps,
                [PlanStep {
                    step: "replacement".into(),
                    status: PlanStepStatus::Pending
                }]
            );
            assert_eq!(timeline.plan_explanation.as_deref(), explanation);
        }
    }

    #[test]
    fn provider_start_failure_folds_semantically() {
        let mut timeline = Timeline::default();
        timeline.apply_at(
            Some(1_234),
            &AgentEvent::ProviderStartFailed {
                error: "spawn failed".into(),
            },
        );

        let provider_error = &timeline.entries[0];
        assert_eq!(provider_error.ts, Some(1_234));
        assert!(timeline.turns.get(provider_error.turn).is_some());
        assert!(matches!(
            &provider_error.content,
            EntryContent::ProviderStartError { error } if error == "spawn failed"
        ));

        timeline.apply_at(
            Some(1_235),
            &AgentEvent::Error {
                message: "boom".into(),
                fatal: true,
            },
        );
        assert!(matches!(
            &timeline.entries[1].content,
            EntryContent::Error { message, .. } if message == "boom"
        ));
    }

    #[test]
    fn usage_limit_reset_is_folded_into_the_latest_error() {
        let mut timeline = Timeline::default();
        timeline.apply_at(
            None,
            &AgentEvent::Error {
                message: "boom".into(),
                fatal: false,
            },
        );
        timeline.apply_at(None, &AgentEvent::UsageLimitReached { resets_at: 42 });

        assert!(matches!(
            &timeline.entries[0].content,
            EntryContent::Error {
                message,
                limit_resets_at: Some(42),
            } if message == "boom"
        ));
    }

    #[test]
    fn errors_and_session_close_fold_into_timeline() {
        let mut timeline = Timeline::default();
        timeline.apply_at(
            None,
            &AgentEvent::TurnStarted {
                turn_id: "t".into(),
            },
        );
        timeline.apply_at(
            None,
            &AgentEvent::ApprovalRequested(ApprovalRequest {
                id: "req".into(),
                parent_item_id: None,
                turn_id: None,
                kind: ApprovalKind::ExecCommand {
                    command: "rm -rf /".into(),
                    cwd: None,
                    reason: None,
                },
                options: Vec::new(),
            }),
        );
        timeline.apply_at(
            None,
            &AgentEvent::Error {
                message: "boom".into(),
                fatal: true,
            },
        );
        timeline.apply_at(None, &AgentEvent::SessionClosed { reason: None });

        assert!(!timeline.turn_running);
        assert!(timeline.pending_approvals.is_empty());
        assert!(matches!(
            &timeline.entries[0].content,
            EntryContent::Error { message, .. } if message == "boom"
        ));
        // A silent close (reason: None) leaves no entry…
        let entries_after_silent_close = timeline.entries.len();
        // …but an abnormal close records the provider's dying words.
        timeline.apply_at(
            None,
            &AgentEvent::SessionClosed {
                reason: Some("codex app-server exited with exit status: 1\nstderr:\nboom".into()),
            },
        );
        assert_eq!(timeline.entries.len(), entries_after_silent_close + 1);
        assert!(matches!(
            &timeline.entries.last().unwrap().content,
            EntryContent::Error { message, .. } if message.contains("stderr:\nboom")
        ));
    }

    #[test]
    fn parented_items_are_dropped_while_subagent_spawn_status_merges() {
        let spawn = ThreadItem {
            id: "spawn".into(),
            parent_item_id: None,
            content: ItemContent::Subagent {
                agent_type: "general-purpose".into(),
                description: "Ping test".into(),
                status: ItemStatus::InProgress,
                summary: None,
                model: Some("opus".into()),
                effort: None,
            },
        };
        let child = ThreadItem {
            id: "spawn:user-1".into(),
            parent_item_id: Some("spawn".into()),
            content: ItemContent::UserMessage {
                text: "ping".into(),
                context_len: None,
                attachments: Vec::new(),
            },
        };
        let completed = ThreadItem {
            content: ItemContent::Subagent {
                agent_type: "general-purpose".into(),
                description: "Ping test".into(),
                status: ItemStatus::Completed,
                summary: Some("pong".into()),
                model: None,
                effort: Some("high".into()),
            },
            ..spawn.clone()
        };
        let timeline = Timeline::fold_events([
            AgentEvent::ItemStarted(spawn),
            AgentEvent::ItemCompleted(child),
            AgentEvent::ItemCompleted(completed),
        ]);
        assert_eq!(timeline.entries.len(), 1);
        assert_eq!(timeline.entries[0].id, "spawn");
        assert!(matches!(
            &timeline.entries[0].content,
            EntryContent::Item(ItemContent::Subagent {
                status: ItemStatus::Completed,
                summary: Some(summary),
                model: Some(model),
                effort: Some(effort),
                ..
            }) if summary == "pong" && model == "opus" && effort == "high"
        ));
    }

    #[test]
    fn stored_user_messages_keep_injected_context_and_accept_older_plain_messages() {
        for (record, expected_text, expected_context) in [
            (
                r#"{"type":"item_completed","id":"u1","content":{"kind":"user_message","text":"just words"}}"#,
                "just words",
                None,
            ),
            (
                r#"{"type":"item_completed","id":"u1","content":{"kind":"user_message","text":"PREFIX\n\nvisible","context_len":8}}"#,
                "PREFIX\n\nvisible",
                Some(8),
            ),
        ] {
            let event: AgentEvent = serde_json::from_str(record).unwrap();
            let timeline = Timeline::fold_events([event]);
            assert!(matches!(&timeline.entries[0].content,
                EntryContent::Item(ItemContent::UserMessage { text, context_len, .. })
                    if text == expected_text && *context_len == expected_context));
        }
    }

    fn at(ts: u64, event: AgentEvent) -> StoredEvent {
        StoredEvent {
            origin: None,
            author: None,
            ts: Some(ts),
            event,
            elided: None,
        }
    }

    fn started(ts: u64, item: ThreadItem) -> StoredEvent {
        at(ts, AgentEvent::ItemStarted(item))
    }

    fn updated(ts: u64, item: ThreadItem) -> StoredEvent {
        at(ts, AgentEvent::ItemUpdated(item))
    }

    fn completed(ts: u64, item: ThreadItem) -> StoredEvent {
        at(ts, AgentEvent::ItemCompleted(item))
    }

    fn command(id: &str, status: ItemStatus) -> ThreadItem {
        ThreadItem {
            id: id.into(),
            parent_item_id: None,
            content: ItemContent::CommandExecution {
                command: "ls".into(),
                output: String::new(),
                exit_code: None,
                status,
            },
        }
    }

    /// A shell command that is still running.
    fn running(id: &str) -> ThreadItem {
        command(id, ItemStatus::InProgress)
    }

    /// The same command, finished.
    fn ran(id: &str) -> ThreadItem {
        command(id, ItemStatus::Completed)
    }

    fn subagent(id: &str, status: ItemStatus) -> ThreadItem {
        ThreadItem {
            id: id.into(),
            parent_item_id: None,
            content: ItemContent::Subagent {
                agent_type: "explore".into(),
                description: "look around".into(),
                status,
                summary: None,
                model: None,
                effort: None,
            },
        }
    }

    fn assistant(id: &str, text: &str) -> AgentEvent {
        AgentEvent::ItemCompleted(ThreadItem {
            id: id.into(),
            parent_item_id: None,
            content: ItemContent::AssistantMessage { text: text.into() },
        })
    }

    /// A statusless tool-like item: its lifecycle is only knowable from the
    /// event variant that carried it (Codex maps `webSearch` this way).
    fn web_search(id: &str) -> ThreadItem {
        ThreadItem {
            id: id.into(),
            parent_item_id: None,
            content: ItemContent::WebSearch {
                query: "rust union of intervals".into(),
            },
        }
    }

    /// The other statusless shape: a provider item canonicalization does not
    /// model yet.
    fn other_item(id: &str) -> ThreadItem {
        ThreadItem {
            id: id.into(),
            parent_item_id: None,
            content: ItemContent::Other {
                provider_kind: "customTool".into(),
                summary: "doing something".into(),
            },
        }
    }

    fn turn_started() -> AgentEvent {
        AgentEvent::TurnStarted {
            turn_id: "turn-1".into(),
        }
    }

    fn turn_completed() -> AgentEvent {
        AgentEvent::TurnCompleted {
            turn_id: "turn-1".into(),
            status: TurnStatus::Completed,
            usage: None,
        }
    }

    /// Fold timestamped events and return the first turn's breakdown.
    fn timing_of(events: Vec<StoredEvent>) -> Option<TurnTiming> {
        Timeline::fold_events(events).turns[0].timing
    }

    #[test]
    fn tool_timing_follows_lifecycle_including_updates_failures_and_reused_ids() {
        for (label, activity, expected_tool_ms) in [
            (
                "sequential",
                vec![
                    started(1_000, running("a")),
                    completed(2_000, ran("a")),
                    started(3_000, running("b")),
                    completed(4_000, ran("b")),
                ],
                2_000,
            ),
            (
                "overlapping and nested",
                vec![
                    started(1_000, running("a")),
                    started(1_500, subagent("b", ItemStatus::InProgress)),
                    started(1_800, running("c")),
                    completed(2_000, ran("c")),
                    completed(2_500, ran("a")),
                    completed(3_000, subagent("b", ItemStatus::Completed)),
                ],
                2_000,
            ),
            (
                "updates and reused id",
                vec![
                    started(1_000, running("a")),
                    updated(1_400, running("a")),
                    updated(2_200, running("a")),
                    completed(3_000, ran("a")),
                    started(4_000, running("a")),
                    completed(4_500, ran("a")),
                ],
                2_500,
            ),
            (
                "missing start",
                vec![updated(1_000, running("a")), completed(2_500, ran("a"))],
                1_500,
            ),
            (
                "backward completion followed by clock recovery",
                vec![
                    started(2_000, running("a")),
                    completed(1_000, ran("a")),
                    started(3_000, running("b")),
                    completed(5_000, ran("b")),
                ],
                2_000,
            ),
            (
                "failed",
                vec![
                    started(1_000, running("a")),
                    updated(2_000, command("a", ItemStatus::Failed)),
                ],
                1_000,
            ),
            (
                "start carries terminal status",
                vec![started(1_000, ran("a")), completed(3_000, ran("a"))],
                2_000,
            ),
            (
                "statusless web search",
                vec![
                    started(2_000, web_search("a")),
                    updated(3_000, web_search("a")),
                    completed(4_500, web_search("a")),
                ],
                2_500,
            ),
            (
                "statusless provider item",
                vec![
                    started(2_000, other_item("a")),
                    updated(3_000, other_item("a")),
                    completed(4_500, other_item("a")),
                ],
                2_500,
            ),
        ] {
            let mut events = vec![at(0, turn_started())];
            events.extend(activity);
            events.push(at(6_000, turn_completed()));
            let timing = timing_of(events).unwrap_or_else(|| panic!("{label}: missing timing"));
            assert_eq!(
                (timing.total_ms, timing.tool_ms),
                (6_000, expected_tool_ms),
                "{label}"
            );
        }
    }

    #[test]
    fn unobserved_unfinished_or_inconsistent_turns_have_no_timing_breakdown() {
        for (label, events) in [
            (
                "legacy",
                vec![
                    user_msg("u", "hi").into(),
                    turn_started().into(),
                    AgentEvent::ItemStarted(running("a")).into(),
                    AgentEvent::ItemCompleted(ran("a")).into(),
                    turn_completed().into(),
                ],
            ),
            (
                "untimed tool",
                vec![
                    at(1_000, turn_started()),
                    AgentEvent::ItemStarted(running("a")).into(),
                    AgentEvent::ItemCompleted(ran("a")).into(),
                    at(9_000, turn_completed()),
                ],
            ),
            (
                "running",
                vec![at(1_000, turn_started()), started(2_000, running("a"))],
            ),
            (
                "no observed start",
                vec![
                    at(1_000, user_msg("u", "go")),
                    started(2_000, running("a")),
                    completed(3_000, ran("a")),
                    at(9_000, turn_completed()),
                ],
            ),
            (
                "untimed start",
                vec![
                    at(1_000, user_msg("u", "go")),
                    turn_started().into(),
                    at(9_000, turn_completed()),
                ],
            ),
            (
                "no end timestamp",
                vec![at(1_000, turn_started()), turn_completed().into()],
            ),
            (
                "end before start",
                vec![at(9_000, turn_started()), at(1_000, turn_completed())],
            ),
            (
                "tool beyond end",
                vec![
                    at(0, turn_started()),
                    started(1_000, running("a")),
                    completed(2_000, ran("a")),
                    started(19_000, running("b")),
                    completed(25_000, ran("b")),
                    at(20_000, turn_completed()),
                ],
            ),
            (
                "assistant beyond end",
                vec![
                    at(1_000, turn_started()),
                    at(30_000, assistant("a", "late")),
                    at(20_000, turn_completed()),
                ],
            ),
            (
                "reasoning beyond end",
                vec![
                    at(1_000, turn_started()),
                    at(
                        30_000,
                        AgentEvent::Delta {
                            item_id: "r".into(),
                            kind: DeltaKind::ReasoningText,
                            text: "late".into(),
                        },
                    ),
                    at(20_000, turn_completed()),
                ],
            ),
        ] {
            assert_eq!(timing_of(events), None, "{label}");
        }
        let timeline =
            Timeline::fold_events([at(1_000, user_msg("u", "go")), at(9_000, turn_completed())]);
        assert_eq!(timeline.turns[0].start_ts, Some(1_000));
        assert_eq!(timeline.turns[0].timing, None);
    }

    #[test]
    fn timing_charges_only_tool_intervals_inside_the_observed_turn() {
        for (label, activity, expected_tool_ms) in [
            (
                "before turn",
                vec![
                    started(1_100, running("a")),
                    completed(2_000, ran("a")),
                    at(5_000, turn_started()),
                ],
                0,
            ),
            (
                "straddling start",
                vec![
                    started(1_100, running("a")),
                    at(5_000, turn_started()),
                    completed(6_000, ran("a")),
                ],
                1_000,
            ),
            (
                "unfinished tool",
                vec![at(5_000, turn_started()), started(6_000, running("a"))],
                3_000,
            ),
            (
                "model only",
                vec![
                    at(5_000, turn_started()),
                    at(6_000, assistant("a", "answer")),
                ],
                0,
            ),
        ] {
            let mut events = vec![at(1_000, user_msg("u1", "go"))];
            events.extend(activity);
            events.push(at(9_000, turn_completed()));
            let timing = timing_of(events).unwrap_or_else(|| panic!("{label}: missing timing"));
            assert_eq!(
                (timing.total_ms, timing.tool_ms),
                (4_000, expected_tool_ms),
                "{label}"
            );
        }
    }

    #[test]
    fn a_turn_finalized_without_a_timestamp_rejects_later_tool_transitions() {
        for (label, first_end, first_timing) in [
            (
                "timed",
                Some(4_000),
                Some(TurnTiming {
                    total_ms: 4_000,
                    tool_ms: 1_000,
                }),
            ),
            ("untimed", None, None),
        ] {
            let mut timeline = Timeline::fold_events([
                at(0, turn_started()),
                started(1_000, running("a")),
                completed(2_000, ran("a")),
                StoredEvent {
                    origin: None,
                    author: None,
                    ts: first_end,
                    event: turn_completed(),
                    elided: None,
                },
            ]);
            assert_eq!(timeline.turns[0].timing, first_timing, "{label}");
            for event in [
                started(4_500, running("ghost")),
                at(5_000, user_msg("u2", "again")),
                at(5_000, turn_started()),
                at(9_000, turn_completed()),
            ] {
                timeline.apply_stored(&event);
            }
            assert_eq!(timeline.turns.len(), 2, "{label}");
            assert_eq!(timeline.turns[0].timing, first_timing, "{label}");
            assert_eq!(
                timeline.turns[1].timing,
                Some(TurnTiming {
                    total_ms: 4_000,
                    tool_ms: 0,
                }),
                "{label}"
            );
        }
    }

    #[test]
    fn served_model_change_sets_turn_meta_and_adds_one_divider() {
        for (requested, transitions, served, expected) in [
            (
                "requested",
                vec![("served", Some("capacity")), ("served", Some("capacity"))],
                "served",
                vec![(Some("requested"), "served", Some("capacity"))],
            ),
            (
                "claude-opus-5[1m]",
                vec![("claude-opus-5", None)],
                "claude-opus-5",
                vec![],
            ),
            (
                "claude-opus-5[1m]",
                vec![("claude-opus-5", None), ("claude-opus-5[3m]", None)],
                "claude-opus-5[3m]",
                vec![(Some("claude-opus-5"), "claude-opus-5[3m]", None)],
            ),
        ] {
            let mut timeline = Timeline::fold_events([
                AgentEvent::SessionStarted {
                    provider_session_id: "session".into(),
                    resume: ResumeCursor(json!({})),
                    model: Some(requested.into()),
                },
                user_msg("user", "hello"),
            ]);
            for (model, reason) in transitions {
                timeline.apply_at(
                    None,
                    &AgentEvent::ServedModel {
                        model: model.into(),
                        reason: reason.map(str::to_string),
                    },
                );
            }
            assert_eq!(timeline.turns[0].served_model.as_deref(), Some(served));
            let changes = timeline
                .entries
                .iter()
                .filter_map(|entry| match &entry.content {
                    EntryContent::ModelChanged { from, to, reason } => {
                        Some((from.as_deref(), to.as_str(), reason.as_deref()))
                    }
                    _ => None,
                })
                .collect::<Vec<_>>();
            assert_eq!(changes, expected, "{requested} -> {served}");
        }
    }

    /// Logs recorded before pi named its streamed messages carry every
    /// turn's assistant stream under the same placeholder id. A stream
    /// continues the open turn's item; the earlier turn's entry is a
    /// different message, and a suffix of the log (as history pages load)
    /// must fold its turns exactly like the whole log does.
    #[test]
    fn streamed_deltas_continue_the_open_turn_not_an_earlier_turn_with_the_same_id() {
        let turn = |n: u64| {
            [
                StoredEvent {
                    origin: None,
                    author: None,
                    ts: Some(n * 10),
                    event: user_msg(&format!("user-{n}"), "go"),
                    elided: None,
                },
                StoredEvent {
                    origin: None,
                    author: None,
                    ts: Some(n * 10 + 1),
                    event: AgentEvent::TurnStarted {
                        turn_id: format!("turn-{n}"),
                    },
                    elided: None,
                },
                StoredEvent {
                    origin: None,
                    author: None,
                    ts: Some(n * 10 + 2),
                    event: assistant_delta("pi-assistant-0:0", &format!("text {n}")),
                    elided: None,
                },
                StoredEvent {
                    origin: None,
                    author: None,
                    ts: Some(n * 10 + 3),
                    event: AgentEvent::TurnCompleted {
                        turn_id: format!("turn-{n}"),
                        status: TurnStatus::Completed,
                        usage: None,
                    },
                    elided: None,
                },
            ]
        };
        let log: Vec<StoredEvent> = [turn(1), turn(2), turn(3)].concat();
        let full = Timeline::fold_events(log.iter().cloned());
        assert_eq!(
            full.entries
                .iter()
                .filter(|entry| entry.id == "pi-assistant-0:0")
                .map(|entry| match &entry.content {
                    EntryContent::Item(ItemContent::AssistantMessage { text }) =>
                        (entry.turn, text.as_str()),
                    _ => unreachable!(),
                })
                .collect::<Vec<_>>(),
            [(0, "text 1"), (1, "text 2"), (2, "text 3")],
            "each turn keeps its own streamed message"
        );

        let suffix = Timeline::fold_events(log[4..].iter().cloned());
        let entries = |timeline: &Timeline, skip: usize| {
            timeline.entries[skip..]
                .iter()
                .map(|entry| {
                    (
                        entry.id.clone(),
                        entry.turn - timeline.entries[skip].turn,
                        format!("{:?}", entry.content),
                    )
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(
            entries(&suffix, 0),
            entries(&full, full.entries.len() - suffix.entries.len()),
            "a page boundary at a turn start folds the same turns as the full log"
        );
    }

    #[test]
    fn synthetic_entry_ids_do_not_depend_on_how_much_earlier_history_is_folded() {
        let error = |ts: u64| StoredEvent {
            origin: None,
            author: None,
            ts: Some(ts),
            event: AgentEvent::Error {
                message: format!("boom {ts}"),
                fatal: false,
            },
            elided: None,
        };
        let log = [
            StoredEvent {
                origin: None,
                author: None,
                ts: Some(1),
                event: user_msg("user-1", "go"),
                elided: None,
            },
            error(2),
            StoredEvent {
                origin: None,
                author: None,
                ts: Some(3),
                event: AgentEvent::TurnCompleted {
                    turn_id: "turn-1".into(),
                    status: TurnStatus::Failed,
                    usage: None,
                },
                elided: None,
            },
            StoredEvent {
                origin: None,
                author: None,
                ts: Some(4),
                event: user_msg("user-2", "again"),
                elided: None,
            },
            error(5),
            error(5),
        ];
        let full = Timeline::fold_events(log.iter().cloned());
        let suffix = Timeline::fold_events(log[3..].iter().cloned());
        let ids = |timeline: &Timeline| {
            timeline
                .entries
                .iter()
                .filter(|entry| matches!(entry.content, EntryContent::Error { .. }))
                .map(|entry| entry.id.clone())
                .collect::<Vec<_>>()
        };
        assert_eq!(ids(&full), ["error-2", "error-5", "error-5-1"]);
        assert_eq!(ids(&suffix), ["error-5", "error-5-1"]);

        let legacy = Timeline::fold_events([
            AgentEvent::Error {
                message: "old".into(),
                fatal: false,
            },
            AgentEvent::Error {
                message: "older".into(),
                fatal: false,
            },
        ]);
        assert_eq!(ids(&legacy), ["error-1", "error-2"]);
    }
}
