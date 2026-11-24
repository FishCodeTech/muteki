use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::hash::{DefaultHasher, Hash as _, Hasher as _};
use std::ops::Range;
use std::path::Path;
use std::sync::Arc;

use agent::{FileChange, ItemContent, ItemStatus, TurnStatus};
use tcode_core::notice::{HostNotice, host_notice};
use tcode_core::project::{Project, SessionMeta};
use tcode_core::session::{EntryContent, SteeringStatus, TimelineEntry, TurnMeta, TurnTiming};

/// The row, its turn, the entries it renders (a Work Log header's whole run),
/// the turn's entries for the trailer (empty unless the row is the turn's
/// last), and the pinned message ids.
pub(crate) type RowRenderArgs<'a> = (
    &'a TimelineRow,
    &'a TurnMeta,
    &'a Path,
    &'a [Arc<TimelineEntry>],
    &'a [Arc<TimelineEntry>],
    (Option<&'a str>, Option<&'a str>),
);

/// A chronological block in a turn. File-change entries stay in activity runs
/// for summary counting, but are rendered by the turn-level CHANGED FILES card.
#[derive(Debug)]
pub(crate) enum Segment<'a> {
    ActivityRun(Vec<&'a TimelineEntry>),
    Images(Vec<&'a TimelineEntry>),
    Relay(&'a TimelineEntry),
    ModelChange(&'a TimelineEntry),
    ContextCompacted(&'a TimelineEntry),
    ContextWindowChanged(&'a TimelineEntry),
    ControlAction(&'a TimelineEntry),
    User(&'a TimelineEntry),
    Assistant(&'a TimelineEntry),
    Error(&'a TimelineEntry),
}

#[derive(Debug)]
pub(crate) struct SegmentedEntries<'a> {
    pub(crate) flow: Vec<Segment<'a>>,
    /// The entry indices each segment of `flow` spans, in `entries` order.
    /// A run's range covers the pending steers it skipped over, so a row that
    /// re-segments its own range folds to the same segment.
    pub(crate) ranges: Vec<Range<usize>>,
    pub(crate) pending_steers: Vec<&'a TimelineEntry>,
}

pub(crate) fn displayed_error_text(content: &EntryContent) -> Cow<'_, str> {
    match content {
        EntryContent::Error { message, .. } => Cow::Borrowed(message),
        EntryContent::ProviderStartError { error } => {
            crate::tr!("errors.provider_start", error = error)
        }
        _ => unreachable!("displayed_error_text requires error timeline content"),
    }
}

/// What the error card of an exhausted usage window offers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LimitResumeAction {
    /// A resume is queued for the reset: countdown plus cancel.
    Scheduled {
        queued_id: u64,
        remaining_secs: u64,
    },
    /// The reset is still ahead and nothing is queued for it.
    Schedule,
    /// The reset has passed while the error still ends the thread and nothing
    /// is queued after it. The scheduled resume lives in the host's in-memory
    /// queue, so a restart loses it; the card offers to continue now instead.
    /// A queued message (the continue itself while the provider starts, or
    /// the user's own) means something already follows the error.
    ContinueNow,
    None,
}

pub(crate) fn limit_resume_action(
    resets_at: u64,
    queued_id: Option<u64>,
    now: u64,
    ends_thread: bool,
    queue_empty: bool,
) -> LimitResumeAction {
    match queued_id {
        Some(queued_id) => LimitResumeAction::Scheduled {
            queued_id,
            remaining_secs: resets_at.saturating_sub(now),
        },
        None if resets_at > now => LimitResumeAction::Schedule,
        None if ends_thread && queue_empty => LimitResumeAction::ContinueNow,
        None => LimitResumeAction::None,
    }
}

pub(crate) type UserContent<'a> = (&'a str, Option<SteeringStatus>, Option<usize>, &'a [String]);

pub(crate) fn user_content(content: &EntryContent) -> Option<UserContent<'_>> {
    match content {
        EntryContent::Item(ItemContent::UserMessage {
            text,
            context_len,
            attachments,
        }) => Some((text, None, *context_len, attachments)),
        EntryContent::Steer {
            text,
            status,
            context_len,
            attachments,
        } => Some((text, Some(*status), *context_len, attachments)),
        _ => None,
    }
}

/// IDs whose message action rows stay visible without hover.
pub(crate) fn latest_message_ids(
    entries: &[Arc<TimelineEntry>],
) -> (Option<String>, Option<String>) {
    let mut last_user_id = None;
    let mut last_assistant_id = None;
    for entry in entries.iter().rev() {
        if last_user_id.is_none()
            && matches!(
                entry.content,
                EntryContent::Item(ItemContent::UserMessage { .. }) | EntryContent::Steer { .. }
            )
        {
            last_user_id = Some(entry.id.clone());
        }
        if last_assistant_id.is_none()
            && matches!(
                entry.content,
                EntryContent::Item(ItemContent::AssistantMessage { .. })
            )
        {
            last_assistant_id = Some(entry.id.clone());
        }
        if last_user_id.is_some() && last_assistant_id.is_some() {
            break;
        }
    }
    (last_user_id, last_assistant_id)
}

/// Coalesce only adjacent activity entries, leaving messages and errors at
/// their exact positions in the timeline.
pub(crate) fn segment_entries<'a>(
    entries: &'a [Arc<TimelineEntry>],
    turn_running: bool,
) -> SegmentedEntries<'a> {
    let mut segments = Vec::new();
    let mut ranges = Vec::new();
    let mut activities = Vec::new();
    let mut run_start = None;
    let mut pending_steers = Vec::new();
    let flush_activities = |segments: &mut Vec<Segment<'a>>,
                            ranges: &mut Vec<Range<usize>>,
                            activities: &mut Vec<&'a TimelineEntry>,
                            run_start: &mut Option<usize>,
                            end: usize| {
        if !activities.is_empty() {
            segments.push(Segment::ActivityRun(std::mem::take(activities)));
            ranges.push(run_start.take().expect("an activity run has a start")..end);
        }
        *run_start = None;
    };

    for (index, entry) in entries.iter().enumerate() {
        let entry = entry.as_ref();
        if turn_running
            && matches!(
                entry.content,
                EntryContent::Steer {
                    status: SteeringStatus::Pending,
                    ..
                }
            )
        {
            pending_steers.push(entry);
            continue;
        }
        let mut single = |segments: &mut Vec<Segment<'a>>,
                          ranges: &mut Vec<Range<usize>>,
                          segment: Segment<'a>| {
            flush_activities(segments, ranges, &mut activities, &mut run_start, index);
            segments.push(segment);
            ranges.push(index..index + 1);
        };
        match &entry.content {
            EntryContent::Item(item)
                if matches!(item, ItemContent::ImageRead { .. })
                    || matches!(item, ItemContent::ToolCall { image_reads, status: ItemStatus::Completed, .. } if !image_reads.is_empty()) =>
            {
                flush_activities(
                    &mut segments,
                    &mut ranges,
                    &mut activities,
                    &mut run_start,
                    index,
                );
                if let Some(Segment::Images(images)) = segments.last_mut() {
                    images.push(entry);
                    ranges.last_mut().expect("an image run has a range").end = index + 1;
                } else {
                    segments.push(Segment::Images(vec![entry]));
                    ranges.push(index..index + 1);
                }
            }
            EntryContent::Item(ItemContent::ImageRead { .. })
            | EntryContent::Item(ItemContent::CommandExecution { .. })
            | EntryContent::Item(ItemContent::ToolCall { .. })
            | EntryContent::Item(ItemContent::Subagent { .. })
            | EntryContent::Item(ItemContent::WebSearch { .. })
            | EntryContent::Item(ItemContent::Other { .. })
            | EntryContent::Item(ItemContent::FileChange { .. }) => {
                run_start.get_or_insert(index);
                activities.push(entry);
            }
            EntryContent::Item(ItemContent::Reasoning { .. }) => {
                if activities.last().is_some_and(|previous| {
                    matches!(
                        &previous.content,
                        EntryContent::Item(ItemContent::Reasoning { text })
                            if text.trim().is_empty()
                    )
                }) {
                    activities.pop();
                }
                run_start.get_or_insert(index);
                activities.push(entry);
            }
            EntryContent::Item(ItemContent::UserMessage { .. }) | EntryContent::Steer { .. } => {
                single(&mut segments, &mut ranges, Segment::User(entry));
            }
            EntryContent::ProviderRelay { .. } => {
                single(&mut segments, &mut ranges, Segment::Relay(entry));
            }
            EntryContent::ModelChanged { .. } => {
                single(&mut segments, &mut ranges, Segment::ModelChange(entry));
            }
            EntryContent::ContextCompacted(_) => {
                single(&mut segments, &mut ranges, Segment::ContextCompacted(entry));
            }
            EntryContent::ContextWindowChanged { .. } => {
                single(
                    &mut segments,
                    &mut ranges,
                    Segment::ContextWindowChanged(entry),
                );
            }
            EntryContent::ControlAction(_) => {
                single(&mut segments, &mut ranges, Segment::ControlAction(entry));
            }
            EntryContent::Item(ItemContent::AssistantMessage { .. }) => {
                single(&mut segments, &mut ranges, Segment::Assistant(entry));
            }
            EntryContent::Error { .. } | EntryContent::ProviderStartError { .. } => {
                single(&mut segments, &mut ranges, Segment::Error(entry));
            }
        }
    }
    flush_activities(
        &mut segments,
        &mut ranges,
        &mut activities,
        &mut run_start,
        entries.len(),
    );
    SegmentedEntries {
        flow: segments,
        ranges,
        pending_steers,
    }
}

/// Which segment, if any, is the turn's *live* work log — the one that opens on
/// its own while the turn is running.
///
/// A running turn whose last segment is prose has none: liveness is carried by
/// the turn-level working indicator standing bare after every segment, so every
/// run is already settled. Nothing is appended to host the indicator.
pub(crate) fn live_activity_segment(segments: &[Segment<'_>], turn_running: bool) -> Option<usize> {
    let last = segments
        .iter()
        .rposition(|segment| matches!(segment, Segment::ActivityRun(_)))?;
    (!turn_running || last + 1 == segments.len()).then_some(last)
}

#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct WorkLogCounts {
    pub(crate) commands: usize,
    pub(crate) files: usize,
    pub(crate) tools: usize,
    pub(crate) subagents: usize,
}

pub(crate) fn work_log_counts(entries: &[&TimelineEntry]) -> WorkLogCounts {
    let mut counts = WorkLogCounts::default();
    let mut files = HashSet::new();

    for entry in entries {
        match &entry.content {
            EntryContent::Item(ItemContent::CommandExecution { .. }) => counts.commands += 1,
            EntryContent::Item(ItemContent::FileChange { changes, .. }) => {
                files.extend(changes.iter().map(|change| change.path.as_str()));
            }
            EntryContent::Item(ItemContent::ToolCall { .. })
            | EntryContent::Item(ItemContent::WebSearch { .. })
            | EntryContent::Item(ItemContent::ImageRead { .. })
            | EntryContent::Item(ItemContent::Other { .. }) => counts.tools += 1,
            EntryContent::Item(ItemContent::Subagent { .. }) => counts.subagents += 1,
            EntryContent::ContextCompacted(_)
            | EntryContent::ContextWindowChanged { .. }
            | EntryContent::ControlAction(_)
            | EntryContent::Steer { .. }
            | EntryContent::Item(ItemContent::UserMessage { .. })
            | EntryContent::Item(ItemContent::AssistantMessage { .. })
            | EntryContent::Item(ItemContent::Reasoning { .. })
            | EntryContent::Error { .. }
            | EntryContent::ProviderStartError { .. }
            | EntryContent::ProviderRelay { .. }
            | EntryContent::ModelChanged { .. } => {}
        }
    }
    counts.files = files.len();
    counts
}

pub(crate) fn work_log_capsule_label(counts: &WorkLogCounts, activity_count: usize) -> String {
    let mut clauses = Vec::new();
    if counts.tools > 0 {
        clauses.push(if counts.tools == 1 {
            crate::tr!("chat.work_log_tool_one").into_owned()
        } else {
            crate::tr!("chat.work_log_tools", count = counts.tools).into_owned()
        });
    }
    if counts.files > 0 {
        clauses.push(if counts.files == 1 {
            crate::tr!("chat.work_log_edit_one").into_owned()
        } else {
            crate::tr!("chat.work_log_edits", count = counts.files).into_owned()
        });
    }
    if counts.commands > 0 {
        clauses.push(if counts.commands == 1 {
            crate::tr!("chat.work_log_command_one").into_owned()
        } else {
            crate::tr!("chat.work_log_commands", count = counts.commands).into_owned()
        });
    }
    if clauses.is_empty() && activity_count > 0 {
        clauses.push(if activity_count == 1 {
            crate::tr!("chat.work_log_activity_one").into_owned()
        } else {
            crate::tr!("chat.work_log_activities", count = activity_count).into_owned()
        });
    }
    clauses.join(" · ")
}

pub(crate) fn activity_run_duration_ms(
    activities: &[&TimelineEntry],
    turn: &TurnMeta,
    is_last: bool,
) -> u64 {
    let first = activities.iter().find_map(|entry| entry.ts);
    let last = activities.iter().rev().find_map(|entry| entry.ts);
    match (first, last) {
        (Some(first), Some(last)) => last.saturating_sub(first),
        _ if is_last => turn.timing.map_or(0, |timing| timing.total_ms),
        _ => 0,
    }
}

// A failed command inside a run is normal agent probing (grep exit 1, a
// retried build); it never fails the run. Only the turn's own terminal
// status colors the snapshot, and only on the segment that carries it.
pub(crate) fn work_log_outcome(
    turn: &TurnMeta,
    _activities: &[&TimelineEntry],
    is_last: bool,
) -> TurnStatus {
    if is_last {
        turn.status.unwrap_or(TurnStatus::Completed)
    } else {
        TurnStatus::Completed
    }
}

/// `text` collapsed to a single spaced line: every whitespace run (newlines
/// included) becomes one space, so a multi-line command shows its full content
/// in a one-line preview instead of just its first line. Clipped to more
/// characters than any row can render; the row fades out what does not fit.
pub(crate) fn one_line(text: &str) -> String {
    const MAX_CHARS: usize = 600;
    text.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(MAX_CHARS)
        .collect()
}

/// Like [`one_line`], but line breaks stay visible: each break between
/// non-empty lines becomes a literal `\n` marker in the output, and the
/// returned byte ranges let the row paint those markers fainter than the
/// text around them so they read as break symbols, not command content.
pub(crate) fn one_line_with_break_markers(text: &str) -> (String, Vec<Range<usize>>) {
    const MAX_CHARS: usize = 600;
    let mut out = String::new();
    let mut markers = Vec::new();
    let mut chars = 0usize;
    for line in text.lines() {
        let mut on_new_line = !out.is_empty();
        for word in line.split_whitespace() {
            if on_new_line {
                let start = out.len();
                out.push_str("\\n");
                markers.push(start..out.len());
                chars += 2;
                on_new_line = false;
            } else if !out.is_empty() {
                out.push(' ');
                chars += 1;
            }
            out.push_str(word);
            chars += word.chars().count();
            if chars >= MAX_CHARS {
                return (out, markers);
            }
        }
    }
    (out, markers)
}

/// A short one-line summary of a tool call's input for the Work Log.
pub(crate) fn tool_brief(input: &serde_json::Value) -> String {
    match input {
        serde_json::Value::Object(map) => map
            .get("query")
            .or_else(|| map.get("path"))
            .or_else(|| map.get("command"))
            .or_else(|| map.get("summary"))
            // Question tools: Claude `AskUserQuestion` items carry `question`,
            // Codex `request_user_input_async` items carry `title`.
            .or_else(|| {
                let first = map.get("questions")?.get(0)?;
                first.get("question").or_else(|| first.get("title"))
            })
            .and_then(|v| v.as_str())
            .map(one_line)
            .unwrap_or_default(),
        serde_json::Value::String(s) => one_line(s),
        _ => String::new(),
    }
}

/// Wall-clock duration formatted as "XmYYs" / "YYs".
pub(crate) fn format_duration(secs: u64) -> String {
    if secs >= 60 {
        crate::tr!(
            "time.duration_minutes",
            minutes = secs / 60,
            seconds = format!("{:02}", secs % 60)
        )
        .into_owned()
    } else {
        crate::tr!("time.duration_seconds", seconds = secs).into_owned()
    }
}

pub(crate) fn format_elapsed_deciseconds(elapsed_ms: u64) -> String {
    let deciseconds = elapsed_ms / 100;
    let seconds = deciseconds / 10;
    let tenth = deciseconds % 10;
    if seconds < 60 {
        format!("{seconds}.{tenth}s")
    } else {
        format!("{}m {}.{tenth}s", seconds / 60, seconds % 60)
    }
}

pub(crate) fn format_compact_span(secs: u64) -> String {
    if secs >= 3600 {
        format!(
            "{}h{:02}m{:02}s",
            secs / 3600,
            (secs % 3600) / 60,
            secs % 60
        )
    } else if secs >= 60 {
        format!("{}m{:02}s", secs / 60, secs % 60)
    } else {
        format!("{secs}s")
    }
}

/// A finished turn's duration. Long turns roll up into hours rather than
/// growing an unreadable minute count (a full day reads `24h 00m 59s`, not
/// `1440m 59s`), keeping the seconds because this row reports actual elapsed
/// time. The live "Working for" indicator keeps [`format_duration`].
pub(crate) fn format_span(secs: u64) -> String {
    if secs >= 3600 {
        crate::tr!(
            "time.duration_hours",
            hours = secs / 3600,
            minutes = format!("{:02}", (secs % 3600) / 60),
            seconds = format!("{:02}", secs % 60)
        )
        .into_owned()
    } else {
        format_duration(secs)
    }
}

pub(crate) fn divergent_served_model<'a>(
    served_model: Option<&'a str>,
    requested_model: Option<&str>,
) -> Option<&'a str> {
    match (served_model, requested_model) {
        (Some(served), Some(requested)) if served != requested => Some(served),
        _ => None,
    }
}

pub(crate) fn format_cost_usd(cost: f64) -> String {
    if cost.abs() >= 0.01 {
        return format!("${cost:.2}");
    }

    let decimals = if (cost * 1_000.).round() != 0. { 3 } else { 4 };
    let formatted = format!("{cost:.decimals$}");
    let trimmed = formatted.trim_end_matches('0').trim_end_matches('.');
    if trimmed == "0" || trimmed == "-0" {
        "$0.0000".to_string()
    } else {
        format!("${trimmed}")
    }
}

/// The quiet, visible portion of a finished turn's timing row. Bucket details
/// intentionally live in [`turn_time_breakdown`] instead of competing with the
/// completion clock, total duration, and cost.
pub(crate) fn turn_time_parts(clock: String, timing: Option<TurnTiming>) -> Vec<String> {
    let mut parts = vec![clock];
    if let Some(timing) = timing {
        parts.push(format_compact_span(timing.total_ms / 1000));
    }
    parts
}

pub(crate) fn turn_time_breakdown(timing: Option<TurnTiming>) -> Option<String> {
    timing.map(|timing| {
        let total = timing.total_ms / 1000;
        let tools = (timing.tool_ms / 1000).min(total);
        let ai = total - tools;
        [
            crate::tr!("chat.turn_ai", duration = format_span(ai)).into_owned(),
            crate::tr!("chat.turn_tools", duration = format_span(tools)).into_owned(),
        ]
        .join(" · ")
    })
}

#[derive(Clone)]
pub(crate) struct TurnTimeClause {
    pub(crate) text: String,
    pub(crate) selector: &'static str,
    pub(crate) warning: bool,
}

pub(crate) fn turn_time_clauses(
    clock: String,
    timing: Option<TurnTiming>,
    cost_usd: Option<f64>,
    served_model: Option<&str>,
    requested_model: Option<&str>,
) -> Vec<TurnTimeClause> {
    const TIMING_SELECTORS: [&str; 2] = ["turn-time-clock", "turn-time-total"];

    let mut clauses = turn_time_parts(clock, timing)
        .into_iter()
        .zip(TIMING_SELECTORS)
        .map(|(text, selector)| TurnTimeClause {
            text,
            selector,
            warning: false,
        })
        .collect::<Vec<_>>();
    if let Some(cost) = cost_usd {
        clauses.push(TurnTimeClause {
            text: format_cost_usd(cost),
            selector: "turn-time-cost",
            warning: false,
        });
    }
    if let Some(served) = divergent_served_model(served_model, requested_model) {
        #[cfg(target_arch = "wasm32")]
        let warning_mark = "!";
        #[cfg(not(target_arch = "wasm32"))]
        let warning_mark = "⚠";
        clauses.push(TurnTimeClause {
            text: format!("{warning_mark} {served}"),
            selector: "turn-time-model",
            warning: true,
        });
    }
    clauses
}

/// Count added / removed lines in a unified diff (ignoring the `+++`/`---`
/// file headers).
pub(crate) fn diff_stats(diff: Option<&str>) -> (u32, u32) {
    let Some(diff) = diff else {
        return (0, 0);
    };
    let mut added = 0;
    let mut removed = 0;
    let mut in_hunk = false;
    for line in diff.lines() {
        if line.starts_with("@@") {
            in_hunk = true;
        }
        if !in_hunk && (line.starts_with("+++") || line.starts_with("---")) {
            continue;
        }
        match line.as_bytes().first() {
            Some(b'+') => added += 1,
            Some(b'-') => removed += 1,
            _ => {}
        }
    }
    (added, removed)
}

/// One live Work Log row for an edited file: the workspace-relative display
/// path plus `+added` / `-deleted` counts. The counts are `None` when the entry
/// carries no diff — "+0 -0" would read as "this edit changed nothing".
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LiveEditRow {
    pub(crate) path: String,
    pub(crate) kind: agent::FileChangeKind,
    pub(crate) counts: Option<(u32, u32)>,
    pub(crate) diff: Option<String>,
}

/// The `+N` / `-N` a live edit row should display, if any.
///
/// A diff that carries no added or removed lines — absent, empty,
/// whitespace-only, or nothing but `+++`/`---` headers — has nothing truthful
/// to show: "+0 -0" reads as "this edit changed nothing". The finished-turn
/// CHANGED FILES card keeps its own `diff_stats` totals unchanged.
pub(crate) fn live_edit_counts(diff: Option<&str>) -> Option<(u32, u32)> {
    let (added, deleted) = diff_stats(Some(diff?));
    (added != 0 || deleted != 0).then_some((added, deleted))
}

/// Expand a file-change snapshot into one live row per file, so a single
/// multi-file entry names every file instead of collapsing to an "N files"
/// label. Paths use the same workspace-relative display as CHANGED FILES.
pub(crate) fn live_edit_rows(changes: &[FileChange], cwd: &Path) -> Vec<LiveEditRow> {
    changes
        .iter()
        .map(|change| LiveEditRow {
            path: crate::workspace_walk::relativize_to_workspace(&change.path, cwd),
            kind: change.kind,
            counts: live_edit_counts(change.diff.as_deref()),
            diff: change.diff.clone(),
        })
        .collect()
}

/// Maximum number of entries kept directly visible at the tail of a live
/// activity run. Older entries move into a separate collapsed Work Log; once
/// prose ends the run, the full run becomes that settled Work Log instead.
pub(crate) const LIVE_ACTIVITY_WINDOW: usize = 5;

pub(crate) fn partition_activity_run<'a>(
    activities: &'a [&'a TimelineEntry],
    live: bool,
) -> (&'a [&'a TimelineEntry], &'a [&'a TimelineEntry]) {
    let visible = if live {
        activities.len().min(LIVE_ACTIVITY_WINDOW)
    } else {
        0
    };
    activities.split_at(activities.len() - visible)
}

/// Format a unix-ms timestamp as a local 12-hour clock, e.g. "2:39 AM".
pub(crate) fn format_local_time(unix_ms: u64) -> String {
    use chrono::{Local, TimeZone as _};

    Local
        .timestamp_millis_opt(unix_ms as i64)
        .single()
        .map(|time| time.format("%-I:%M %p").to_string())
        .unwrap_or_default()
}

const START_HUB_PROJECT_LIMIT: usize = 6;

/// Projects shown by the empty-chat start hub, ordered by latest unarchived
/// thread activity. Projects without threads follow alphabetically.
pub(crate) fn start_hub_projects(
    projects: &[Project],
    sessions: &[SessionMeta],
) -> Vec<(Project, Option<u64>)> {
    let mut projects: Vec<(Project, Option<u64>)> = projects
        .iter()
        .cloned()
        .map(|project| {
            let last_activity = sessions
                .iter()
                .filter(|session| session.archived_at.is_none())
                .filter(|session| session.project_id.as_deref() == Some(project.id.as_str()))
                .map(|session| session.updated_at)
                .max();
            (project, last_activity)
        })
        .collect();
    projects.sort_by(|(project_a, activity_a), (project_b, activity_b)| {
        match (activity_a, activity_b) {
            (Some(activity_a), Some(activity_b)) => activity_b.cmp(activity_a).then_with(|| {
                project_a
                    .name
                    .to_lowercase()
                    .cmp(&project_b.name.to_lowercase())
            }),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => project_a
                .name
                .to_lowercase()
                .cmp(&project_b.name.to_lowercase()),
        }
    });
    projects.truncate(START_HUB_PROJECT_LIMIT);
    projects
}

/// Pre-measure this many full-window heights on each side of the chat viewport.
///
/// GPUI's list performs the expensive first layout for items in this band, so a
/// generous buffer keeps ordinary trackpad/wheel scrolling from discovering and
/// laying out a turn on the same frame in which it becomes visible. The chat
/// viewport is shorter than the full window, making this a conservative lower
/// bound in practice while the list itself remains bounded for huge histories.
const TIMELINE_OVERDRAW_VIEWPORTS: f32 = 4.;
const TIMELINE_MIN_OVERDRAW: f32 = 3072.;

pub(crate) fn timeline_overdraw(viewport_height: f32) -> f32 {
    (viewport_height.max(0.) * TIMELINE_OVERDRAW_VIEWPORTS).max(TIMELINE_MIN_OVERDRAW)
}

/// How to bring a mirrored [`MdState`] in line with the timeline's text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum MdSync {
    /// Already in sync.
    Noop,
    /// The text grew by an append.
    Push(String),
    /// The text changed in a way an append cannot express.
    Reset,
}

/// The pure delta/reset decision behind [`MdState::sync`].
pub(crate) fn md_sync(synced: &str, text: &str) -> MdSync {
    if synced == text {
        return MdSync::Noop;
    }
    match text.strip_prefix(synced) {
        Some(delta) if !delta.is_empty() => MdSync::Push(delta.to_string()),
        _ => MdSync::Reset,
    }
}

/// Return the part of a user entry that belongs in its message bubble.
pub(crate) fn user_visible_text(text: &str, context_len: Option<usize>) -> &str {
    context_len
        .filter(|len| *len <= text.len() && text.is_char_boundary(*len))
        .map_or(text, |len| &text[len..])
}

/// Encode plain text as markdown whose rendered text is still literal input.
pub(crate) fn plain_text_as_markdown(text: &str) -> String {
    let mut markdown = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    let mut at_line_start = true;
    let mut line_is_empty = true;

    while let Some(ch) = chars.next() {
        if ch == '\n' {
            let mut newline_count = 1;
            while chars.next_if_eq(&'\n').is_some() {
                newline_count += 1;
            }

            if newline_count == 1 && !line_is_empty && chars.peek().is_some() {
                // Encode a single newline as an explicit break so Markdown
                // cannot fold it into a space in display or selection.
                markdown.push_str("<br>");
            } else {
                markdown.extend(std::iter::repeat_n('\n', newline_count));
            }
            at_line_start = true;
            line_is_empty = true;
            continue;
        }

        line_is_empty = false;
        if at_line_start {
            match ch {
                ' ' => {
                    markdown.push_str("&#32;");
                    continue;
                }
                '\t' => {
                    markdown.push_str("&#9;");
                    continue;
                }
                _ => at_line_start = false,
            }
        }

        if ch.is_ascii_punctuation() {
            // Backslash-escaped brackets are LaTeX delimiters in Markdown.
            markdown.push_str(&format!("&#{};", ch as u32));
        } else {
            markdown.push(ch);
        }
    }

    markdown
}

/// The list virtualizes segments rather than turns, and an expanded Work Log's
/// activities rather than its run, so a turn with hundreds of tool calls and
/// interim messages costs the rows on screen, not the whole turn, every
/// frame. A turn without segments still owns one empty row for its trailer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TimelineRow {
    pub(crate) turn: usize,
    /// The entries the row renders, in timeline order across rows.
    pub(crate) entry_range: Range<usize>,
    pub(crate) entry_count: usize,
    pub(crate) part: RowPart,
    pub(crate) first_in_turn: bool,
    pub(crate) last_in_turn: bool,
    /// The turn's live work log: the run that opens on its own while the
    /// turn runs ([`live_activity_segment`]).
    pub(crate) live_activity: bool,
    /// The turn's final assistant text, the one that carries an action row.
    pub(crate) last_assistant: bool,
    /// Identity of the row's first entry: a run grows at its end, so the
    /// row stays itself while it streams.
    pub(crate) identity: u64,
    /// Identity of the newest entry alone. A history page can only add
    /// entries before a partially loaded turn, so this survives completion
    /// while `identity` does not.
    pub(crate) tail_identity: u64,
    pub(crate) content: u64,
}

/// Which part of its segment a row renders.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RowPart {
    Segment,
    /// The header of an expanded Work Log. It renders no entry of its own
    /// but reads the whole run, `run`, for its summary.
    WorkLogHeader {
        run: Range<usize>,
    },
    /// One folded activity under an expanded Work Log header.
    WorkLogActivity,
    /// The live window below an expanded Work Log's folded activities.
    WorkLogLive,
}

/// The rows of `turn` within `rows`, which are sorted by turn.
pub(crate) fn rows_of_turn(rows: &[TimelineRow], turn: usize) -> Range<usize> {
    let start = rows.partition_point(|row| row.turn < turn);
    let end = start + rows[start..].partition_point(|row| row.turn == turn);
    start..end
}

/// The row that renders the entry at `entry_index`. An entry no segment
/// covers (a pending steer between two runs) renders in its turn's trailer,
/// the turn's last row.
pub(crate) fn row_of_entry(rows: &[TimelineRow], entry_index: usize, turn: usize) -> Option<usize> {
    let candidate = rows.partition_point(|row| row.entry_range.end <= entry_index);
    if rows
        .get(candidate)
        .is_some_and(|row| row.entry_range.contains(&entry_index))
    {
        return Some(candidate);
    }
    rows_of_turn(rows, turn).last()
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TurnIndexMeta {
    start_ts: Option<u64>,
    end_ts: Option<u64>,
    running: bool,
    timing: Option<tcode_core::session::TurnTiming>,
    served_model: Option<String>,
    cost_usd: Option<u64>,
    status: Option<TurnStatus>,
}

impl From<&TurnMeta> for TurnIndexMeta {
    fn from(turn: &TurnMeta) -> Self {
        Self {
            start_ts: turn.start_ts,
            end_ts: turn.end_ts,
            running: turn.running,
            timing: turn.timing,
            served_model: turn.served_model.clone(),
            cost_usd: turn.cost_usd.map(f64::to_bits),
            status: turn.status,
        }
    }
}

impl TurnIndexMeta {
    fn matches(&self, turn: &TurnMeta) -> bool {
        self.start_ts == turn.start_ts
            && self.end_ts == turn.end_ts
            && self.running == turn.running
            && self.timing == turn.timing
            && self.served_model.as_deref() == turn.served_model.as_deref()
            && self.cost_usd == turn.cost_usd.map(f64::to_bits)
            && self.status == turn.status
    }
}

/// How the timeline being synced relates to the previously indexed one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TimelineContinuity {
    /// The same session with its whole history loaded: turns only grow at
    /// the end or change in place.
    Complete,
    /// The same session with earlier history still unloaded: a page can
    /// complete or merge the first turn's entries without replacing its row.
    PartialFirstTurn,
    /// A different session: every row is new.
    NewSession,
}

/// Stateful input snapshot for reusing indexed turns across store notifications.
#[derive(Debug, Default)]
pub(crate) struct TurnIndexCache {
    entries: Vec<Arc<TimelineEntry>>,
    turns: Vec<TurnIndexMeta>,
    expanded: HashSet<String>,
    #[cfg(test)]
    reindexed_turns: usize,
}

impl TurnIndexCache {
    pub(crate) fn sync(
        &mut self,
        items: &mut Vec<TimelineRow>,
        turns: &[TurnMeta],
        entries: &[Arc<TimelineEntry>],
        expanded: &HashSet<String>,
        continuity: TimelineContinuity,
    ) -> ListSync {
        let reset = continuity == TimelineContinuity::NewSession;
        let turn_count = turns
            .len()
            .max(entries.last().map_or(0, |entry| entry.turn + 1));
        let entry_divergence = self
            .entries
            .iter()
            .zip(entries)
            .position(|(old, new)| !Arc::ptr_eq(old, new))
            .unwrap_or(self.entries.len().min(entries.len()));
        let tail_replace = entries.len() == self.entries.len()
            && entry_divergence.checked_add(1) == Some(entries.len());
        let append = entry_divergence == self.entries.len() && entries.len() >= self.entries.len();
        let settings_changed = self.expanded != *expanded;
        let must_reset = reset
            || settings_changed
            || entries.len() < self.entries.len()
            || (!append && !tail_replace)
            || turns.len() < self.turns.len();

        let turn_divergence = self
            .turns
            .iter()
            .zip(turns)
            .position(|(old, new)| !old.matches(new))
            .unwrap_or(self.turns.len().min(turns.len()));
        let mut reindex_from = if must_reset { 0 } else { turn_count };
        if !must_reset {
            if entry_divergence < entries.len() {
                reindex_from = reindex_from.min(entries[entry_divergence].turn);
            }
            if entry_divergence < self.entries.len() {
                reindex_from = reindex_from.min(self.entries[entry_divergence].turn);
            }
            if turn_divergence < turns.len() {
                reindex_from = reindex_from.min(turn_divergence);
            }
        }

        let suffix = if reindex_from == 0 {
            index_rows(turns, entries, expanded)
        } else {
            if reindex_from < turn_count {
                index_rows_from(turns, entries, expanded, reindex_from)
            } else {
                Vec::new()
            }
        };
        // Rows of the turns before `reindex_from` are kept as they are.
        let kept = rows_of_turn(items, reindex_from).start;
        let item_count = kept + suffix.len();
        let sync = list_sync_with(items, item_count, continuity, |index| {
            if index < kept {
                &items[index]
            } else {
                &suffix[index - kept]
            }
        });
        items.truncate(kept);
        items.extend(suffix);

        #[cfg(test)]
        {
            self.reindexed_turns = turn_count.saturating_sub(reindex_from);
        }
        self.entries.truncate(entry_divergence);
        self.entries
            .extend(entries[entry_divergence..].iter().cloned());
        self.turns.truncate(turn_divergence);
        self.turns
            .extend(turns[turn_divergence..].iter().map(TurnIndexMeta::from));
        if self.expanded != *expanded {
            self.expanded.clone_from(expanded);
        }
        sync
    }

    #[cfg(test)]
    fn reindexed_turns(&self) -> usize {
        self.reindexed_turns
    }
}

/// Build the segment rows and their fingerprints for every turn.
///
/// Timeline entries are chronological, so all entries for a turn are adjacent.
/// The max entry turn keeps a temporary orphan bucket renderable if a provider
/// ever exposes an entry before its corresponding `TurnMeta`.
pub(crate) fn index_rows(
    turns: &[TurnMeta],
    entries: &[Arc<TimelineEntry>],
    expanded: &HashSet<String>,
) -> Vec<TimelineRow> {
    index_rows_from(turns, entries, expanded, 0)
}

fn index_rows_from(
    turns: &[TurnMeta],
    entries: &[Arc<TimelineEntry>],
    expanded: &HashSet<String>,
    first_turn: usize,
) -> Vec<TimelineRow> {
    debug_assert!(entries.windows(2).all(|pair| pair[0].turn <= pair[1].turn));

    let turn_count = turns
        .len()
        .max(entries.last().map_or(0, |entry| entry.turn + 1));
    let first_entry = entries.partition_point(|entry| entry.turn < first_turn);
    let mut turn_ranges = vec![entries.len()..entries.len(); turn_count.saturating_sub(first_turn)];
    for (index, entry) in entries.iter().enumerate().skip(first_entry) {
        let range = &mut turn_ranges[entry.turn - first_turn];
        if range.start == entries.len() {
            range.start = index;
        }
        range.end = index + 1;
    }

    let mut rows = Vec::new();
    for (offset, turn_range) in turn_ranges.into_iter().enumerate() {
        let index = first_turn + offset;
        let turn = turns.get(index);
        let running = turn.is_some_and(|turn| turn.running);
        let segmented = segment_entries(&entries[turn_range.clone()], running);
        let live_activity = live_activity_segment(&segmented.flow, running);
        let last_assistant = segmented
            .flow
            .iter()
            .rposition(|segment| matches!(segment, Segment::Assistant(_)));
        let mut segment_ranges = segmented
            .ranges
            .iter()
            .map(|range| turn_range.start + range.start..turn_range.start + range.end)
            .collect::<Vec<_>>();
        if segment_ranges.is_empty() {
            segment_ranges.push(turn_range.end..turn_range.end);
        }
        let last_segment = segment_ranges.len() - 1;
        for (segment, segment_range) in segment_ranges.into_iter().enumerate() {
            let live = live_activity == Some(segment);
            let final_assistant = last_assistant == Some(segment);
            let parts = segment_parts(
                entries,
                segmented.flow.get(segment),
                segment_range.clone(),
                expanded,
                index,
                live && running,
            );
            let last_part = parts.len() - 1;
            for (part_index, (part, entry_range)) in parts.into_iter().enumerate() {
                let first_in_turn = segment == 0 && part_index == 0;
                let last_in_turn = segment == last_segment && part_index == last_part;
                // A header renders its whole run, and a run is known by its
                // first entry whether or not its Work Log is expanded.
                let shape_range = match &part {
                    RowPart::WorkLogHeader { run } => run.clone(),
                    _ => entry_range.clone(),
                };
                let mut identity = DefaultHasher::new();
                let mut tail_identity = DefaultHasher::new();
                let mut content = DefaultHasher::new();
                if let Some(entry) = entries[shape_range.clone()].last() {
                    entry.id.hash(&mut tail_identity);
                }
                match &part {
                    RowPart::WorkLogActivity => {
                        ("worklog-activity", &entries[entry_range.end - 1].id).hash(&mut identity);
                    }
                    RowPart::WorkLogLive => {
                        ("worklog-live", &entries[segment_range.start].id).hash(&mut identity);
                        // The window slides over activities of the same shape.
                        entries[entry_range.start].id.hash(&mut content);
                    }
                    RowPart::Segment | RowPart::WorkLogHeader { .. } => {
                        if let Some(entry) = entries[shape_range.clone()].first() {
                            entry.id.hash(&mut identity);
                        }
                    }
                }
                std::mem::discriminant(&part).hash(&mut content);
                for entry in &entries[shape_range.clone()] {
                    std::mem::discriminant(&entry.content).hash(&mut content);
                    entry.ts.hash(&mut content);
                    hash_entry_shape(&entry.content, &mut content);
                    // A disclosure row (orchestrate context / callback) grows a tall
                    // scroll card when expanded, so its toggle state must change the
                    // row fingerprint or the list keeps the collapsed measurement.
                    for key in disclosure_keys(&entry.content, &entry.id) {
                        expanded.contains(&key).hash(&mut content);
                    }
                }
                if shape_range.is_empty() {
                    // An empty turn's only row has no entry to be known by.
                    ("empty-turn", index).hash(&mut identity);
                }
                (first_in_turn, last_in_turn, live, final_assistant).hash(&mut content);
                running.hash(&mut content);
                if last_in_turn {
                    // The trailer: pending steers float here while the turn
                    // runs, and the finished turn renders its breakdown.
                    for entry in &entries[turn_range.clone()] {
                        if let EntryContent::Steer { text, status, .. } = &entry.content {
                            entry.id.hash(&mut content);
                            text.len().hash(&mut content);
                            status.hash(&mut content);
                            for key in disclosure_keys(&entry.content, &entry.id) {
                                expanded.contains(&key).hash(&mut content);
                            }
                        }
                    }
                    entries[turn_range.clone()]
                        .last()
                        .and_then(|entry| entry.ts)
                        .hash(&mut content);
                    if let Some(turn) = turn {
                        turn.start_ts.hash(&mut content);
                        turn.end_ts.hash(&mut content);
                        turn.timing.hash(&mut content);
                        turn.served_model.hash(&mut content);
                        turn.cost_usd.map(f64::to_bits).hash(&mut content);
                        turn.status
                            .as_ref()
                            .map(std::mem::discriminant)
                            .hash(&mut content);
                    }
                }
                rows.push(TimelineRow {
                    turn: index,
                    entry_count: entry_range.len(),
                    entry_range,
                    part,
                    first_in_turn,
                    last_in_turn,
                    live_activity: live,
                    last_assistant: final_assistant,
                    identity: identity.finish(),
                    tail_identity: tail_identity.finish(),
                    content: content.finish(),
                });
            }
        }
    }
    rows
}

/// The rows one segment spanning `range` renders as. An expanded Work Log
/// with folded activities becomes its header, one row per folded activity and
/// the live window; every other segment is one row. Each folded activity
/// ends its part's entry range.
fn segment_parts(
    entries: &[Arc<TimelineEntry>],
    segment: Option<&Segment<'_>>,
    range: Range<usize>,
    expanded: &HashSet<String>,
    turn: usize,
    live_window: bool,
) -> Vec<(RowPart, Range<usize>)> {
    let whole = || vec![(RowPart::Segment, range.clone())];
    let Some(Segment::ActivityRun(activities)) = segment else {
        return whole();
    };
    if !expanded.contains(&work_log_key(turn, &activities[0].id)) {
        return whole();
    }
    let (folded, visible) = partition_activity_run(activities, live_window);
    if folded.is_empty() {
        return whole();
    }
    let mut parts = vec![(
        RowPart::WorkLogHeader { run: range.clone() },
        range.start..range.start,
    )];
    let mut start = range.start;
    for activity in folded {
        let end = start
            + entries[start..range.end]
                .iter()
                .position(|entry| std::ptr::eq(entry.as_ref(), *activity))
                .expect("a folded activity lies in its run")
            + 1;
        parts.push((RowPart::WorkLogActivity, start..end));
        start = end;
    }
    if !visible.is_empty() {
        parts.push((RowPart::WorkLogLive, start..range.end));
    }
    parts
}

/// The expansion key of the Work Log folding a run whose first activity is
/// `segment_id`.
pub(crate) fn work_log_key(turn: usize, segment_id: &str) -> String {
    format!("worklog-{turn}-{segment_id}")
}

/// The notice the host wrote as a user message, read past any injected context the turn
/// carried (the pull request instructions precede a notice like any other queued message).
pub(crate) fn user_notice(content: &EntryContent) -> Option<HostNotice> {
    let (text, _, context_len, _) = user_content(content)?;
    host_notice(user_visible_text(text, context_len))
}

/// The expansion key of each disclosure row a notice renders as, in order.
pub(crate) fn notice_keys(notice: &HostNotice, entry_id: &str) -> Vec<String> {
    match notice {
        HostNotice::Orchestrate(callbacks) => (0..callbacks.len())
            .map(|index| format!("orchestrate-callback-{entry_id}-{index}"))
            .collect(),
        HostNotice::PullRequest { .. } => vec![format!("pull-request-update-{entry_id}")],
        HostNotice::ReportReminder => vec![format!("report-reminder-{entry_id}")],
        HostNotice::Control(_) => vec![format!("control-notice-{entry_id}")],
    }
}

/// The per-entry expansion keys of a user message's disclosure rows: its injected context
/// (annotated with a `context_len`) and its notice. Empty for an ordinary user message,
/// which stays a plain bubble.
fn disclosure_keys(content: &EntryContent, entry_id: &str) -> Vec<String> {
    let mut keys = Vec::new();
    if user_content(content).is_some_and(|(_, _, context_len, _)| context_len.is_some()) {
        keys.push(format!("orchestrate-context-{entry_id}"));
    }
    if let Some(notice) = user_notice(content) {
        keys.extend(notice_keys(&notice, entry_id));
    }
    keys
}

/// Hash only data that can alter a turn's layout. Text lengths make streaming
/// updates O(number of entries) without repeatedly hashing growing markdown.
fn hash_entry_shape(content: &EntryContent, hash: &mut DefaultHasher) {
    if let EntryContent::Item(item) = content {
        std::mem::discriminant(item).hash(hash);
    }
    match content {
        EntryContent::Item(ItemContent::UserMessage {
            text,
            context_len,
            attachments,
        }) => {
            attachments.len().hash(hash);
            text.len().hash(hash);
            Option::<SteeringStatus>::None.hash(hash);
            context_len.hash(hash);
        }
        EntryContent::Steer {
            text,
            status,
            context_len,
            attachments,
        } => {
            attachments.len().hash(hash);
            text.len().hash(hash);
            status.hash(hash);
            context_len.hash(hash);
        }
        EntryContent::Item(ItemContent::AssistantMessage { text })
        | EntryContent::Item(ItemContent::Reasoning { text }) => {
            text.len().hash(hash);
        }
        EntryContent::Item(ItemContent::CommandExecution {
            command,
            output,
            exit_code,
            status,
        }) => {
            command.len().hash(hash);
            output.len().hash(hash);
            exit_code.hash(hash);
            std::mem::discriminant(status).hash(hash);
        }
        EntryContent::Item(ItemContent::FileChange { changes, status }) => {
            changes.len().hash(hash);
            for change in changes {
                change.path.len().hash(hash);
                change.diff.as_ref().map(String::len).hash(hash);
            }
            std::mem::discriminant(status).hash(hash);
        }
        EntryContent::Item(ItemContent::ToolCall {
            name,
            input,
            output,
            status,
            image_reads,
        }) => {
            image_reads.len().hash(hash);
            name.len().hash(hash);
            input.to_string().len().hash(hash);
            output.as_ref().map(String::len).hash(hash);
            std::mem::discriminant(status).hash(hash);
        }
        EntryContent::Item(ItemContent::Subagent {
            agent_type,
            description,
            status,
            summary,
            model,
            effort,
        }) => {
            agent_type.len().hash(hash);
            description.len().hash(hash);
            std::mem::discriminant(status).hash(hash);
            summary.as_ref().map(String::len).hash(hash);
            model.as_ref().map(String::len).hash(hash);
            effort.as_ref().map(String::len).hash(hash);
        }

        EntryContent::Error {
            message,
            limit_resets_at,
        } => {
            message.len().hash(hash);
            limit_resets_at.hash(hash);
        }
        EntryContent::ProviderStartError { error } => error.len().hash(hash),
        EntryContent::ProviderRelay {
            from_provider,
            to_provider,
            prompt,
            ..
        } => {
            from_provider.hash(hash);
            to_provider.hash(hash);
            prompt.as_ref().map(String::len).hash(hash);
        }
        EntryContent::ModelChanged { from, to, reason } => {
            from.hash(hash);
            to.hash(hash);
            reason.hash(hash);
        }
        EntryContent::ContextCompacted(_) => {}
        EntryContent::ContextWindowChanged { window } => window.hash(hash),
        EntryContent::ControlAction(action) => {
            action.actor_thread_id.hash(hash);
            action.action.hash(hash);
            action.detail.hash(hash);
        }
        EntryContent::Item(ItemContent::ImageRead { path, .. }) => path.hash(hash),
        EntryContent::Item(ItemContent::WebSearch { query }) => {
            "web_search".len().hash(hash);
            serde_json::json!({ "query": query })
                .to_string()
                .len()
                .hash(hash);
            Option::<usize>::None.hash(hash);
            std::mem::discriminant(&ItemStatus::Completed).hash(hash);
        }
        EntryContent::Item(ItemContent::Other {
            provider_kind,
            summary,
        }) => {
            provider_kind.len().hash(hash);
            serde_json::json!({ "summary": summary })
                .to_string()
                .len()
                .hash(hash);
            Option::<usize>::None.hash(hash);
            std::mem::discriminant(&ItemStatus::Completed).hash(hash);
        }
    }
}

/// Mutation to apply to the persistent [`ListState`] after a timeline sync.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ListSync {
    None,
    /// Rows arrived above every existing row: an earlier history page, or
    /// the records that complete the partial first turn.
    Prepend {
        count: usize,
        remeasure: Vec<usize>,
    },
    Reset {
        count: usize,
    },
    /// Rows inserted or replaced in place. `splices` are ranges of the old
    /// rows and the number of new rows standing in for each, ascending, so
    /// they apply back to front; `remeasure` indexes the new rows.
    /// `carried` are the spliced old ranges whose content continues, in
    /// order, in the rows standing in for them: an expanded Work Log's live
    /// window folding activities out into rows of their own. A reader resting
    /// in one keeps its pixel offset into the new rows.
    Incremental {
        splices: Vec<(Range<usize>, usize)>,
        remeasure: Vec<usize>,
        carried: Vec<Range<usize>>,
    },
}

/// Align the old rows with the new ones by identity, in order.
///
/// Rows keep their measured height wherever their identity survives; a row
/// whose content changed is remeasured in place. Rows that appear are
/// spliced in where they stand, which is how the list keeps the reader's
/// anchor across a steer landing mid-turn or a page arriving above. Rows
/// that vanish or trade places reset the list, except inside the partial
/// first turn, whose entries a page may merge or shift, and above every row
/// that survives: dropping the pages read above the tail removes rows there.
fn list_sync_with<'a>(
    old: &[TimelineRow],
    new_len: usize,
    continuity: TimelineContinuity,
    new_at: impl Fn(usize) -> &'a TimelineRow,
) -> ListSync {
    if continuity == TimelineContinuity::NewSession {
        return ListSync::Reset { count: new_len };
    }
    let partial_first_turn = continuity == TimelineContinuity::PartialFirstTurn;
    let mut old_pos = HashMap::with_capacity(old.len());
    for (index, row) in old.iter().enumerate() {
        if old_pos.insert(row.identity, index).is_some() {
            return ListSync::Reset { count: new_len };
        }
    }
    let mut new_pos = HashMap::with_capacity(new_len);
    for index in 0..new_len {
        if new_pos.insert(new_at(index).identity, index).is_some() {
            return ListSync::Reset { count: new_len };
        }
    }
    // The partial first turn's row keeps its place when a page completes
    // it: its newest entry is still its newest entry.
    let completes = |old_row: &TimelineRow, new_row: &TimelineRow| {
        partial_first_turn
            && old_row.turn == 0
            && old_row.entry_count > 0
            && old_row.tail_identity == new_row.tail_identity
            && new_row.entry_count >= old_row.entry_count
    };

    let mut splices: Vec<(Range<usize>, usize)> = Vec::new();
    let mut remeasure = Vec::new();
    let mut carried = Vec::new();
    let mut aligned = false;
    let (mut i, mut j) = (0, 0);
    while i < old.len() || j < new_len {
        if i < old.len() && j < new_len {
            let (old_row, new_row) = (&old[i], new_at(j));
            if old_row.identity == new_row.identity {
                aligned = true;
                if old_row.content != new_row.content || old_row.entry_count != new_row.entry_count
                {
                    remeasure.push(j);
                }
                i += 1;
                j += 1;
                continue;
            }
        }
        let old_survives = i < old.len() && new_pos.contains_key(&old[i].identity);
        let new_existed = j < new_len && old_pos.contains_key(&new_at(j).identity);
        if i < old.len() && !old_survives {
            // The completed first-turn row may sit below rows the page
            // added above it.
            if let Some(k) = (j..new_len).find(|&k| completes(&old[i], new_at(k))) {
                if k > j {
                    splices.push((i..i, k - j));
                }
                remeasure.push(k);
                i += 1;
                j = k + 1;
                continue;
            }
            // An empty turn's row goes when the turn gains entries; the
            // partial first turn's rows may merge as their records arrive;
            // an expanded Work Log's rows go when it collapses or its live
            // window settles.
            let replaceable = |row: &TimelineRow| {
                row.entry_count == 0
                    || partial_first_turn && row.turn == 0
                    || row.part != RowPart::Segment
            };
            if !replaceable(&old[i]) {
                let below_survives = old[i..]
                    .iter()
                    .any(|row| new_pos.contains_key(&row.identity));
                if aligned || j > 0 || !below_survives {
                    return ListSync::Reset { count: new_len };
                }
                let start = i;
                while !new_pos.contains_key(&old[i].identity) {
                    i += 1;
                }
                match splices.last_mut() {
                    Some((dropped, 0)) if dropped.end == start => dropped.end = i,
                    _ => splices.push((start..i, 0)),
                }
                continue;
            }
            let start = i;
            while i < old.len() && !new_pos.contains_key(&old[i].identity) && replaceable(&old[i]) {
                i += 1;
            }
            let inserted = j;
            while j < new_len && !old_pos.contains_key(&new_at(j).identity) {
                j += 1;
            }
            splices.push((start..i, j - inserted));
            if old[start].part == RowPart::WorkLogLive {
                carried.push(start..i);
            } else {
                remeasure.extend(inserted..j);
            }
        } else if j < new_len && !new_existed {
            let inserted = j;
            while j < new_len && !old_pos.contains_key(&new_at(j).identity) {
                j += 1;
            }
            // Activities folded out of a live window arrive right above it.
            if i < old.len()
                && old[i].part == RowPart::WorkLogLive
                && j < new_len
                && new_at(j).identity == old[i].identity
            {
                splices.push((i..i + 1, j - inserted + 1));
                carried.push(i..i + 1);
                i += 1;
                j += 1;
            } else {
                splices.push((i..i, j - inserted));
            }
        } else {
            // Both rows live on elsewhere: an order change.
            return ListSync::Reset { count: new_len };
        }
    }

    match splices.as_slice() {
        [] if remeasure.is_empty() => ListSync::None,
        [(range, count)] if range == &(0..0) && !old.is_empty() => ListSync::Prepend {
            count: *count,
            remeasure,
        },
        _ => ListSync::Incremental {
            splices,
            remeasure,
            carried,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent::{FileChange, FileChangeKind, ItemContent, ItemStatus, ProviderKind};

    /// A child's callback dispatched behind the pull request instructions is still the callback
    /// (a disclosure row), not the user's words (a bubble), and both of its rows have keys.
    #[test]
    fn a_callback_behind_injected_context_is_still_a_callback() {
        let context = "<pull_request_linking>\nlink it\n</pull_request_linking>\n\n";
        let callback = "[orchestrate] thread child-7 (\"Fix the build\") completed. tokens: input 5, output 3, total 8.\nDone.";
        let content = EntryContent::Item(ItemContent::UserMessage {
            text: format!("{context}{callback}"),
            context_len: Some(context.len()),
            attachments: Vec::new(),
        });
        let Some(HostNotice::Orchestrate(parsed)) = user_notice(&content) else {
            panic!("the visible text is the callback");
        };
        assert_eq!(parsed[0].child_id, "child-7");
        assert_eq!(parsed[0].body, "Done.");
        assert_eq!(
            disclosure_keys(&content, "u1"),
            ["orchestrate-context-u1", "orchestrate-callback-u1-0"]
        );
        let plain = EntryContent::Item(ItemContent::UserMessage {
            text: format!("{context}fix the build"),
            context_len: Some(context.len()),
            attachments: Vec::new(),
        });
        assert!(user_notice(&plain).is_none());
        assert_eq!(disclosure_keys(&plain, "u2"), ["orchestrate-context-u2"]);
    }
    use std::collections::HashSet;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use tcode_core::project::{Project, SessionMeta};
    use tcode_core::session::{EntryContent, SteeringStatus, TimelineEntry, TurnMeta, TurnTiming};

    /// A thread reopened after the reset has lost its queued resume: the
    /// card must offer to continue now, but only while the error still ends
    /// the thread and nothing is queued after it (a second click or the user's
    /// own message must not queue a duplicate), and must keep offering to
    /// schedule while the reset is ahead.
    #[test]
    fn limit_card_offers_continue_once_reset_passed() {
        let now = 1_800_000_000;
        assert_eq!(
            limit_resume_action(now - 60, None, now, true, true),
            LimitResumeAction::ContinueNow
        );
        assert_eq!(
            limit_resume_action(now - 60, None, now, false, true),
            LimitResumeAction::None
        );
        assert_eq!(
            limit_resume_action(now - 60, None, now, true, false),
            LimitResumeAction::None
        );
        assert_eq!(
            limit_resume_action(now + 60, None, now, true, true),
            LimitResumeAction::Schedule
        );
        assert_eq!(
            limit_resume_action(now - 60, Some(7), now, true, true),
            LimitResumeAction::Scheduled {
                queued_id: 7,
                remaining_secs: 0
            }
        );
    }

    const REAL_DIFF: &str = "--- a/src/foo.rs\n\
                             +++ b/src/foo.rs\n\
                             @@ -1,3 +1,4 @@\n\
                             \x20context\n\
                             +added one\n\
                             +added two\n\
                             -removed one\n";

    #[test]
    fn start_hub_orders_active_projects_then_alphabetical_empty_projects_and_caps_at_six() {
        let project = |id: &str, name: &str| Project {
            id: id.into(),
            name: name.into(),
            root: PathBuf::from(format!("/{id}")),
            quick_chat: false,
            icon_path: None,
            permission_defaults: Default::default(),
            created_at: 0,
        };
        let projects = vec![
            project("archived", "Archived only"),
            project("alpha", "Alpha"),
            project("recent", "Recent"),
            project("older", "Older"),
            project("middle", "Middle"),
            project("extra", "Extra"),
            project("zulu", "Zulu"),
        ];
        let session = |id: &str, project_id: &str, updated_at: u64, archived: bool| {
            let mut session =
                SessionMeta::new(ProviderKind::Codex, PathBuf::from("/project"), None);
            session.id = id.into();
            session.project_id = Some(project_id.into());
            session.updated_at = updated_at;
            session.archived_at = archived.then_some(updated_at);
            session
        };
        let sessions = vec![
            session("recent-thread", "recent", 50, false),
            session("middle-thread", "middle", 30, false),
            session("older-thread", "older", 20, false),
            session("extra-thread", "extra", 10, false),
            session("archived-thread", "archived", 100, true),
        ];

        let ordered = start_hub_projects(&projects, &sessions);
        let ids: Vec<&str> = ordered
            .iter()
            .map(|(project, _)| project.id.as_str())
            .collect();

        assert_eq!(
            ids,
            vec!["recent", "middle", "older", "extra", "alpha", "archived"]
        );
        assert_eq!(ordered[0].1, Some(50));
        assert_eq!(ordered[5].1, None);
    }

    fn entry(id: &str, content: EntryContent) -> Arc<TimelineEntry> {
        Arc::new(TimelineEntry {
            id: id.to_string(),
            content,
            ts: None,
            turn: 0,
        })
    }

    fn user_item(text: &str) -> EntryContent {
        EntryContent::Item(ItemContent::UserMessage {
            text: text.into(),
            context_len: None,
            attachments: Vec::new(),
        })
    }

    fn assistant(text: &str) -> EntryContent {
        EntryContent::Item(ItemContent::AssistantMessage { text: text.into() })
    }

    fn reasoning(text: &str) -> EntryContent {
        EntryContent::Item(ItemContent::Reasoning { text: text.into() })
    }

    #[test]
    fn latest_message_ids_find_the_newest_user_and_assistant_entries() {
        let entries = vec![
            entry("user-old", user_item("old")),
            entry("assistant-old", assistant("old")),
            command("command"),
            entry(
                "steer-new",
                EntryContent::Steer {
                    text: "new".into(),
                    status: SteeringStatus::Pending,
                    context_len: None,
                    attachments: Vec::new(),
                },
            ),
            entry("assistant-new", assistant("new")),
            entry("reasoning", reasoning("later but not a message")),
        ];

        assert_eq!(
            latest_message_ids(&entries),
            (Some("steer-new".into()), Some("assistant-new".into()))
        );
    }

    #[test]
    fn provider_start_error_is_localized_only_at_render_boundary() {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        let generic = EntryContent::Error {
            message: "generic\0原样".into(),
            limit_resets_at: None,
        };
        let provider_start = EntryContent::ProviderStartError {
            error: "spawn failed".into(),
        };

        crate::set_locale(crate::LANGUAGE_ENGLISH);
        assert_eq!(
            displayed_error_text(&generic).as_bytes(),
            b"generic\0\xe5\x8e\x9f\xe6\xa0\xb7"
        );
        assert_eq!(
            displayed_error_text(&provider_start),
            "Failed to start provider: spawn failed"
        );

        crate::set_locale(crate::LANGUAGE_SIMPLIFIED_CHINESE);
        assert_eq!(
            displayed_error_text(&generic).as_bytes(),
            b"generic\0\xe5\x8e\x9f\xe6\xa0\xb7"
        );
        assert_eq!(
            displayed_error_text(&provider_start),
            "启动提供商失败：spawn failed"
        );
        crate::set_locale(crate::LANGUAGE_ENGLISH);
    }

    fn command(id: &str) -> Arc<TimelineEntry> {
        entry(
            id,
            EntryContent::Item(ItemContent::CommandExecution {
                command: id.to_string(),
                output: String::new(),
                exit_code: Some(0),
                status: ItemStatus::Completed,
            }),
        )
    }

    fn at_turn(mut entry: Arc<TimelineEntry>, turn: usize) -> Arc<TimelineEntry> {
        Arc::make_mut(&mut entry).turn = turn;
        entry
    }

    #[test]
    fn turn_list_index_and_sync_cover_stream_append_truncate_and_session_switch() {
        #[derive(Clone)]
        struct Snapshot {
            turns: Vec<TurnMeta>,
            entries: Vec<Arc<TimelineEntry>>,
            expanded: HashSet<String>,
        }
        impl Snapshot {
            fn new(turns: usize, entries: Vec<Arc<TimelineEntry>>) -> Self {
                Self {
                    turns: vec![TurnMeta::default(); turns],
                    entries,
                    expanded: HashSet::new(),
                }
            }
        }
        struct Step {
            name: &'static str,
            snapshot: Snapshot,
            continuity: TimelineContinuity,
            expected: ListSync,
        }
        fn step(name: &'static str, snapshot: Snapshot, expected: ListSync) -> Step {
            Step {
                name,
                snapshot,
                expected,
                continuity: TimelineContinuity::Complete,
            }
        }
        struct Scenario {
            name: &'static str,
            initial: Snapshot,
            steps: Vec<Step>,
        }
        let incremental = |splices, remeasure| ListSync::Incremental {
            splices,
            remeasure,
            carried: vec![],
        };

        let initial = Snapshot::new(
            1,
            vec![
                entry("user-0", user_item("go")),
                entry("assistant-0", assistant("working")),
            ],
        );
        let mut command_added = initial.clone();
        command_added.entries.push(command("command-0"));
        let mut run_grows = command_added.clone();
        run_grows.entries.push(command("command-1"));
        let mut new_turn = run_grows.clone();
        new_turn.turns.push(TurnMeta::default());
        new_turn
            .entries
            .push(at_turn(entry("user-1", user_item("next")), 1));
        let mut tail_replaced = new_turn.clone();
        tail_replaced.entries[4] = at_turn(entry("user-1", user_item("next, updated")), 1);
        let mut context = tail_replaced.clone();
        context.entries[4] = at_turn(
            entry(
                "user-1",
                EntryContent::Item(ItemContent::UserMessage {
                    text: "context\nquestion".into(),
                    context_len: Some(8),
                    attachments: Vec::new(),
                }),
            ),
            1,
        );
        let mut expanded = context.clone();
        expanded
            .expanded
            .insert("orchestrate-context-user-1".into());
        let mut scenarios = vec![Scenario {
            name: "stream append, tail updates and reset",
            initial: initial.clone(),
            steps: vec![
                step(
                    "append command",
                    command_added,
                    incremental(vec![(2..2, 1)], vec![1]),
                ),
                step("grow activity run", run_grows, incremental(vec![], vec![2])),
                step(
                    "append turn",
                    new_turn,
                    incremental(vec![(3..3, 1)], vec![]),
                ),
                step("replace tail", tail_replaced, incremental(vec![], vec![3])),
                step("add disclosure", context, incremental(vec![], vec![3])),
                step("expand disclosure", expanded, incremental(vec![], vec![3])),
                step("truncate", initial.clone(), ListSync::Reset { count: 2 }),
                Step {
                    continuity: TimelineContinuity::NewSession,
                    ..step(
                        "equal-shaped new session",
                        initial,
                        ListSync::Reset { count: 2 },
                    )
                },
                Step {
                    continuity: TimelineContinuity::NewSession,
                    ..step(
                        "unrelated new session",
                        Snapshot::new(1, vec![entry("new-session", assistant("fresh"))]),
                        ListSync::Reset { count: 1 },
                    )
                },
                step(
                    "rewind to empty turn",
                    Snapshot::new(1, vec![]),
                    ListSync::Reset { count: 1 },
                ),
            ],
        }];

        let first = entry("first", assistant("First response"));
        scenarios.push(Scenario {
            name: "live empty turn append",
            initial: Snapshot::new(2, vec![first.clone()]),
            steps: vec![step(
                "replace empty and append empty",
                Snapshot::new(
                    3,
                    vec![
                        first,
                        at_turn(entry("second", assistant("Second response")), 1),
                    ],
                ),
                incremental(vec![(1..2, 2)], vec![1, 2]),
            )],
        });
        scenarios.push(Scenario {
            name: "complete partial first turn",
            initial: Snapshot::new(1, vec![entry("tail", assistant("partial answer"))]),
            steps: vec![Step {
                continuity: TimelineContinuity::PartialFirstTurn,
                ..step(
                    "prepend retained answer",
                    Snapshot::new(
                        2,
                        vec![
                            entry("earlier-user", user_item("earlier")),
                            entry("earlier-answer", assistant("done")),
                            at_turn(entry("user", user_item("question")), 1),
                            at_turn(entry("tail", assistant("partial answer")), 1),
                        ],
                    ),
                    ListSync::Prepend {
                        count: 3,
                        remeasure: vec![3],
                    },
                )
            }],
        });
        scenarios.push(Scenario {
            name: "refold partial history",
            initial: Snapshot::new(
                3,
                vec![
                    entry("a", assistant("a")),
                    at_turn(entry("b", assistant("b")), 1),
                    at_turn(entry("shifted", reasoning("moved")), 1),
                    at_turn(entry("c", assistant("c")), 2),
                ],
            ),
            steps: vec![Step {
                continuity: TimelineContinuity::PartialFirstTurn,
                ..step(
                    "move entries between loaded turns",
                    Snapshot::new(
                        3,
                        vec![
                            entry("z", user_item("z")),
                            entry("a", assistant("a")),
                            at_turn(entry("b", assistant("b")), 1),
                            at_turn(entry("shifted", reasoning("moved")), 2),
                            at_turn(entry("c", assistant("c")), 2),
                        ],
                    ),
                    ListSync::Prepend {
                        count: 1,
                        remeasure: vec![1, 2, 3, 4],
                    },
                )
            }],
        });
        scenarios.push(Scenario {
            name: "pages dropped above the tail",
            initial: Snapshot::new(
                4,
                vec![
                    entry("paged", assistant("an earlier page")),
                    at_turn(entry("paged-later", assistant("a later page")), 1),
                    at_turn(entry("kept-user", user_item("question")), 2),
                    at_turn(entry("kept", assistant("answer")), 2),
                    at_turn(entry("newest", assistant("latest")), 3),
                ],
            ),
            steps: vec![Step {
                continuity: TimelineContinuity::PartialFirstTurn,
                ..step(
                    "drop the pages above the tail's window",
                    Snapshot::new(
                        2,
                        vec![
                            entry("kept-user", user_item("question")),
                            entry("kept", assistant("answer")),
                            at_turn(entry("newest", assistant("latest")), 1),
                        ],
                    ),
                    incremental(vec![(0..2, 0)], vec![]),
                )
            }],
        });
        for (name, continuity, expected) in [
            (
                "merge inside partial turn",
                TimelineContinuity::PartialFirstTurn,
                incremental(vec![(1..2, 0)], vec![0]),
            ),
            (
                "replace completed turn",
                TimelineContinuity::Complete,
                ListSync::Reset { count: 2 },
            ),
        ] {
            scenarios.push(Scenario {
                name,
                initial: Snapshot::new(
                    2,
                    vec![
                        entry("pi-assistant-0:1", assistant("tail of the answer")),
                        command("call-1"),
                        at_turn(entry("user-1", user_item("next")), 1),
                    ],
                ),
                steps: vec![Step {
                    continuity,
                    ..step(
                        "merge reused placeholder",
                        Snapshot::new(
                            2,
                            vec![
                                entry("pi-assistant-0:1", assistant("the whole answer")),
                                at_turn(entry("user-1", user_item("next")), 1),
                            ],
                        ),
                        expected,
                    )
                }],
            });
        }
        for (name, item) in [
            (
                "subagent status",
                ItemContent::Subagent {
                    agent_type: "researcher".into(),
                    description: "Inspect the protocol".into(),
                    status: ItemStatus::InProgress,
                    summary: None,
                    model: None,
                    effort: None,
                },
            ),
            (
                "file-change status",
                ItemContent::FileChange {
                    changes: vec![FileChange {
                        path: "src/lib.rs".into(),
                        kind: FileChangeKind::Modify,
                        diff: Some(REAL_DIFF.into()),
                    }],
                    status: ItemStatus::InProgress,
                },
            ),
        ] {
            let before = Snapshot::new(1, vec![entry("activity", EntryContent::Item(item))]);
            let mut after = before.clone();
            match &mut Arc::make_mut(&mut after.entries[0]).content {
                EntryContent::Item(ItemContent::Subagent {
                    status, summary, ..
                }) => {
                    *status = ItemStatus::Completed;
                    *summary = Some("Found the event envelope".into());
                }
                EntryContent::Item(ItemContent::FileChange { status, .. }) => {
                    *status = ItemStatus::Completed
                }
                _ => unreachable!(),
            }
            scenarios.push(Scenario {
                name,
                initial: before,
                steps: vec![step(
                    "complete activity",
                    after,
                    incremental(vec![], vec![0]),
                )],
            });
        }
        let mut pending = Snapshot::new(
            1,
            vec![
                entry(
                    "steer",
                    EntryContent::Steer {
                        text: "redirect".into(),
                        status: SteeringStatus::Pending,
                        context_len: None,
                        attachments: Vec::new(),
                    },
                ),
                entry("assistant", assistant("working")),
            ],
        );
        pending.turns[0].running = true;
        let mut accepted = pending.clone();
        if let EntryContent::Steer { status, .. } =
            &mut Arc::make_mut(&mut accepted.entries[0]).content
        {
            *status = SteeringStatus::Accepted;
        }
        let mut reordered = accepted.clone();
        reordered.entries.swap(0, 1);
        scenarios.push(Scenario {
            name: "steer status and ordering",
            initial: pending,
            steps: vec![
                step(
                    "accepted steer takes recorded position",
                    accepted,
                    ListSync::Prepend {
                        count: 1,
                        remeasure: vec![1],
                    },
                ),
                step("reordered steer", reordered, ListSync::Reset { count: 2 }),
            ],
        });

        for scenario in scenarios {
            let mut cache = TurnIndexCache::default();
            let mut rows = Vec::new();
            let initial = scenario.initial;
            cache.sync(
                &mut rows,
                &initial.turns,
                &initial.entries,
                &initial.expanded,
                TimelineContinuity::Complete,
            );
            assert_eq!(
                rows,
                index_rows(&initial.turns, &initial.entries, &initial.expanded),
                "{}: initial",
                scenario.name
            );
            if scenario.name == "stream append, tail updates and reset" {
                assert_eq!(rows.len(), 2);
                assert_eq!(rows[0].entry_range, 0..1);
                assert_eq!(rows[1].entry_range, 1..2);
                assert!(rows[0].first_in_turn && !rows[0].last_in_turn);
                assert!(!rows[1].first_in_turn && rows[1].last_in_turn);
            }
            for step in scenario.steps {
                let input = step.snapshot;
                let sync = cache.sync(
                    &mut rows,
                    &input.turns,
                    &input.entries,
                    &input.expanded,
                    step.continuity,
                );
                assert_eq!(sync, step.expected, "{}: {}", scenario.name, step.name);
                assert_eq!(
                    rows,
                    index_rows(&input.turns, &input.entries, &input.expanded),
                    "{}: {}",
                    scenario.name,
                    step.name
                );
                match step.name {
                    "append command" => assert_eq!(rows[2].entry_range, 2..3),
                    "grow activity run" => {
                        assert_eq!(rows.len(), 3);
                        assert_eq!(rows[2].entry_range, 2..4);
                    }
                    "append turn" => {
                        assert_eq!(rows[2].entry_range, 2..4);
                        assert_eq!(rows[3].entry_range, 4..5);
                        assert_eq!(rows[3].turn, 1);
                    }
                    "replace empty and append empty" => {
                        assert_eq!(rows.len(), 3);
                        assert_eq!(rows[1].entry_range, 1..2);
                        assert_eq!(rows[2].entry_range, 2..2);
                    }
                    _ => {}
                }
            }
        }
    }

    #[test]
    fn replacing_the_tail_of_a_200_turn_timeline_reindexes_one_turn() {
        let turns = vec![TurnMeta::default(); 200];
        let mut entries = (0..200)
            .map(|turn| at_turn(entry(&format!("assistant-{turn}"), assistant("x")), turn))
            .collect::<Vec<_>>();
        let expanded = HashSet::new();
        let mut cache = TurnIndexCache::default();
        let mut incremental = Vec::new();
        cache.sync(
            &mut incremental,
            &turns,
            &entries,
            &expanded,
            TimelineContinuity::Complete,
        );

        entries[199] = at_turn(entry("assistant-199", assistant("streamed")), 199);
        cache.sync(
            &mut incremental,
            &turns,
            &entries,
            &expanded,
            TimelineContinuity::Complete,
        );

        assert_eq!(incremental, index_rows(&turns, &entries, &expanded));
        assert!(
            cache.reindexed_turns() <= 1,
            "tail replacement reindexed {} turns",
            cache.reindexed_turns()
        );
    }

    #[test]
    fn segmentation_preserves_message_order_and_groups_adjacent_activities() {
        let entries = [
            entry("user", user_item("go")),
            command("cmd-1"),
            command("cmd-2"),
            entry("assistant-1", assistant("first")),
            command("cmd-3"),
            entry("assistant-2", assistant("second")),
            entry(
                "error",
                EntryContent::Error {
                    message: "boom".into(),
                    limit_resets_at: None,
                },
            ),
        ];
        let segments = segment_entries(&entries, false).flow;

        assert_eq!(segments.len(), 6);
        assert!(matches!(segments[0], Segment::User(entry) if entry.id == "user"));
        assert!(matches!(
            &segments[1],
            Segment::ActivityRun(entries)
                if entries.iter().map(|entry| entry.id.as_str()).collect::<Vec<_>>()
                    == ["cmd-1", "cmd-2"]
        ));
        assert!(matches!(segments[2], Segment::Assistant(entry) if entry.id == "assistant-1"));
        assert!(matches!(
            &segments[3],
            Segment::ActivityRun(entries)
                if entries.iter().map(|entry| entry.id.as_str()).collect::<Vec<_>>() == ["cmd-3"]
        ));
        assert!(matches!(segments[4], Segment::Assistant(entry) if entry.id == "assistant-2"));
        assert!(matches!(segments[5], Segment::Error(entry) if entry.id == "error"));
        let entries = [
            command("cmd"),
            entry(
                "window",
                EntryContent::ContextWindowChanged { window: 500_000 },
            ),
        ];
        let segments = segment_entries(&entries, false).flow;

        assert!(matches!(
            segments.as_slice(),
            [Segment::ActivityRun(activities), Segment::ContextWindowChanged(entry)]
                if activities.len() == 1 && entry.id == "window"
        ));
        let image = |id: &str| {
            entry(
                id,
                EntryContent::Item(ItemContent::ImageRead {
                    path: format!("/tmp/{id}.png"),
                    image: None,
                }),
            )
        };
        let entries = [
            image("one"),
            entry(
                "two",
                EntryContent::Item(ItemContent::ToolCall {
                    name: "Read".into(),
                    input: serde_json::json!({"file_path":"two.png"}),
                    output: None,
                    status: ItemStatus::Completed,
                    image_reads: vec![agent::Attachment {
                        media_type: "image/png".into(),
                        data_base64: String::new(),
                        source_path: None,
                    }],
                }),
            ),
            command("cmd"),
            image("three"),
            entry("reply", assistant("done")),
        ];
        let segmented = segment_entries(&entries, false);
        assert!(
            matches!(segmented.flow.as_slice(), [Segment::Images(first), Segment::ActivityRun(_), Segment::Images(second), Segment::Assistant(_)]
            if first.iter().map(|entry| entry.id.as_str()).collect::<Vec<_>>() == ["one", "two"] && second[0].id == "three")
        );
        assert_eq!(segmented.ranges, vec![0..2, 2..3, 3..4, 4..5]);
        let segmented = segment_entries(&[], false);
        assert!(segmented.flow.is_empty());
        assert!(segmented.pending_steers.is_empty());
        let entries = [
            command("cmd-1"),
            entry(
                "edit",
                EntryContent::Item(ItemContent::FileChange {
                    changes: vec![],
                    status: ItemStatus::Completed,
                }),
            ),
            command("cmd-2"),
        ];
        let segments = segment_entries(&entries, false).flow;

        assert!(matches!(
            segments.as_slice(),
            [Segment::ActivityRun(run)]
                if run.iter().map(|entry| entry.id.as_str()).collect::<Vec<_>>()
                    == ["cmd-1", "edit", "cmd-2"]
        ));
    }

    #[test]
    fn pending_steers_float_after_live_flow_in_fifo_order_only_while_running() {
        let pending = |id: &str| {
            entry(
                id,
                EntryContent::Steer {
                    text: id.into(),
                    status: SteeringStatus::Pending,
                    context_len: None,
                    attachments: Vec::new(),
                },
            )
        };
        let entries = [
            entry("assistant-a", assistant("a")),
            pending("steer-a"),
            command("command"),
            pending("steer-b"),
            entry("assistant-b", assistant("b")),
        ];

        let live = segment_entries(&entries, true);
        assert_eq!(live.flow.len(), 3);
        assert!(matches!(live.flow[0], Segment::Assistant(entry) if entry.id == "assistant-a"));
        assert!(matches!(
            &live.flow[1],
            Segment::ActivityRun(run) if run.len() == 1 && run[0].id == "command"
        ));
        assert!(matches!(live.flow[2], Segment::Assistant(entry) if entry.id == "assistant-b"));
        assert_eq!(
            live.pending_steers
                .iter()
                .map(|entry| entry.id.as_str())
                .collect::<Vec<_>>(),
            ["steer-a", "steer-b"]
        );

        let idle = segment_entries(&entries, false);
        assert!(idle.pending_steers.is_empty());
        assert_eq!(idle.flow.len(), 5);
        assert!(matches!(idle.flow[1], Segment::User(entry) if entry.id == "steer-a"));
        assert!(matches!(idle.flow[3], Segment::User(entry) if entry.id == "steer-b"));
    }

    /// The conversation that motivated segment rows: one running turn with
    /// hundreds of tool runs and interim messages. A turn is not a row; each
    /// of its segments is, so the list can skip the ones off screen.
    #[test]
    fn a_long_running_turn_virtualizes_into_one_row_per_segment() {
        let turns = vec![TurnMeta {
            running: true,
            ..Default::default()
        }];
        let mut entries = vec![entry("user", user_item("go"))];
        for step in 0..200 {
            entries.push(command(&format!("cmd-{step}")));
            entries.push(entry(&format!("reasoning-{step}"), reasoning("thinking")));
            entries.push(entry(&format!("note-{step}"), assistant("interim")));
        }
        entries.push(command("cmd-final"));
        let mut cache = TurnIndexCache::default();
        let mut rows = Vec::new();
        cache.sync(
            &mut rows,
            &turns,
            &entries,
            &HashSet::new(),
            TimelineContinuity::Complete,
        );

        // The user bubble, then a work log and a note per step, then the
        // trailing live work log.
        assert_eq!(rows.len(), 1 + 2 * 200 + 1);
        assert!(rows.iter().all(|row| row.turn == 0));
        assert!(rows[0].first_in_turn && !rows[0].last_in_turn);
        assert_eq!(
            rows[1].entry_range,
            1..3,
            "a run and its reasoning share a row"
        );
        let live = rows.iter().filter(|row| row.live_activity).count();
        let last_assistant = rows.iter().position(|row| row.last_assistant);
        assert_eq!(live, 1);
        assert!(rows.last().unwrap().live_activity);
        assert!(rows.last().unwrap().last_in_turn);
        assert_eq!(last_assistant, Some(rows.len() - 2));
        assert_eq!(rows_of_turn(&rows, 0), 0..rows.len());
        assert_eq!(row_of_entry(&rows, 2, 0), Some(1));
        assert_eq!(row_of_entry(&rows, 3, 0), Some(2));

        // The next command grows the last row in place, so the rows on
        // screen keep their measured heights.
        entries.push(command("cmd-next"));
        let last_row = rows.len() - 1;
        let sync = cache.sync(
            &mut rows,
            &turns,
            &entries,
            &HashSet::new(),
            TimelineContinuity::Complete,
        );
        assert_eq!(
            sync,
            ListSync::Incremental {
                splices: vec![],
                remeasure: vec![last_row],
                carried: vec![],
            }
        );
    }

    /// An expanded Work Log of a long turn gives each folded activity its own
    /// row, and growing, settling or collapsing it moves rows in place
    /// instead of resetting the reader's scroll position.
    #[test]
    fn an_expanded_work_log_gives_each_folded_activity_a_row() {
        let activities = [
            command("cargo check"),
            file_change("edit", &["src/foo.rs"]),
            command("cargo test"),
            command("cargo clippy"),
            command("cargo fmt"),
            command("cargo nextest"),
        ];
        for (count, running, folded, live) in [
            (
                5,
                true,
                vec![],
                vec![
                    "cargo check",
                    "edit",
                    "cargo test",
                    "cargo clippy",
                    "cargo fmt",
                ],
            ),
            (
                6,
                true,
                vec!["cargo check"],
                vec![
                    "edit",
                    "cargo test",
                    "cargo clippy",
                    "cargo fmt",
                    "cargo nextest",
                ],
            ),
            (
                6,
                false,
                vec![
                    "cargo check",
                    "edit",
                    "cargo test",
                    "cargo clippy",
                    "cargo fmt",
                    "cargo nextest",
                ],
                vec![],
            ),
        ] {
            let entries = &activities[..count];
            let mut rows = Vec::new();
            TurnIndexCache::default().sync(
                &mut rows,
                &[TurnMeta {
                    running,
                    ..Default::default()
                }],
                entries,
                &HashSet::from([work_log_key(0, "cargo check")]),
                TimelineContinuity::Complete,
            );
            let ids = |part: fn(&RowPart) -> bool| {
                rows.iter()
                    .filter(|row| part(&row.part))
                    .flat_map(|row| &entries[row.entry_range.clone()])
                    .map(|entry| entry.id.as_str())
                    .collect::<Vec<_>>()
            };
            assert_eq!(
                ids(|part| matches!(part, RowPart::WorkLogActivity)),
                folded,
                "count {count}, running {running}"
            );
            assert_eq!(
                ids(|part| matches!(part, RowPart::Segment | RowPart::WorkLogLive)),
                live,
                "count {count}, running {running}"
            );
        }
        let running = vec![TurnMeta {
            running: true,
            ..Default::default()
        }];
        let mut entries = vec![entry("user", user_item("go"))];
        entries.extend((0..8).map(|step| command(&format!("cmd-{step}"))));
        let expanded = HashSet::from([work_log_key(0, "cmd-0")]);
        let mut cache = TurnIndexCache::default();
        let mut rows = Vec::new();
        cache.sync(
            &mut rows,
            &running,
            &entries,
            &expanded,
            TimelineContinuity::Complete,
        );
        let parts = rows.iter().map(|row| row.part.clone()).collect::<Vec<_>>();
        assert_eq!(
            parts,
            [
                RowPart::Segment,
                RowPart::WorkLogHeader { run: 1..9 },
                RowPart::WorkLogActivity,
                RowPart::WorkLogActivity,
                RowPart::WorkLogActivity,
                RowPart::WorkLogLive,
            ]
        );
        assert_eq!(rows[2].entry_range, 1..2);
        assert_eq!(rows[4].entry_range, 3..4);
        assert_eq!(rows[5].entry_range, 4..9, "the live window keeps five");
        assert!(rows[5].last_in_turn && rows[5].live_activity);
        assert_eq!(row_of_entry(&rows, 3, 0), Some(4));

        // The oldest live activity folds into a row of its own above the
        // live window; both carry the live window's content.
        entries.push(command("cmd-8"));
        let sync = cache.sync(
            &mut rows,
            &running,
            &entries,
            &expanded,
            TimelineContinuity::Complete,
        );
        assert!(matches!(
            sync,
            ListSync::Incremental { splices, remeasure, carried }
                if splices == [(5..6, 2)] && remeasure == [1] && carried.as_slice() == std::slice::from_ref(&(5..6))
        ));

        // Once the turn ends every activity is folded.
        let sync = cache.sync(
            &mut rows,
            &[TurnMeta::default()],
            &entries,
            &expanded,
            TimelineContinuity::Complete,
        );
        assert_eq!(rows.len(), 2 + 9);
        let finished_header = rows[1].identity;
        assert!(matches!(
            sync,
            ListSync::Incremental { splices, carried, .. }
                if splices == [(6..7, 5)] && carried.as_slice() == std::slice::from_ref(&(6..7))
        ));

        let sync = cache.sync(
            &mut rows,
            &[TurnMeta::default()],
            &entries,
            &HashSet::new(),
            TimelineContinuity::Complete,
        );
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[1].identity, finished_header);
        assert_eq!(
            sync,
            ListSync::Incremental {
                splices: vec![(2..11, 0)],
                remeasure: vec![1],
                carried: vec![],
            }
        );
    }

    #[test]
    fn reasoning_preserves_nonempty_history_and_coalesces_only_adjacent_empty_placeholders() {
        for running in [true, false] {
            for (entries, expected) in [
                (
                    vec![
                        entry("first", reasoning("first")),
                        entry("latest", reasoning("latest")),
                    ],
                    vec!["first", "latest"],
                ),
                (
                    vec![
                        entry("empty-1", reasoning("")),
                        entry("empty-2", reasoning("  \n")),
                        entry("visible", reasoning("visible")),
                        command("command"),
                        entry("empty-3", reasoning("")),
                        entry("empty-4", reasoning("")),
                    ],
                    vec!["visible", "command", "empty-4"],
                ),
                (
                    vec![
                        entry("reason", reasoning("thinking")),
                        command("later-command"),
                    ],
                    vec!["reason", "later-command"],
                ),
            ] {
                let segments = segment_entries(&entries, running).flow;
                assert!(matches!(segments.as_slice(), [Segment::ActivityRun(run)]
                    if run.iter().map(|entry| entry.id.as_str()).collect::<Vec<_>>() == expected));
            }
            let entries = [
                entry("reason", reasoning("thinking")),
                entry("assistant", assistant("answer")),
            ];
            let segments = segment_entries(&entries, running).flow;
            assert!(
                matches!(segments.as_slice(), [Segment::ActivityRun(run), Segment::Assistant(entry)]
                if run.len() == 1 && run[0].id == "reason" && entry.id == "assistant")
            );
        }
    }

    #[test]
    fn only_a_trailing_activity_run_is_live_in_a_running_turn() {
        let prose_tail = [command("cmd"), entry("assistant", assistant("answer"))];
        let segments = segment_entries(&prose_tail, true).flow;
        // Prose, then the bare turn-level indicator: no empty run is invented
        // to host it, and the earlier run has already settled.
        assert_eq!(segments.len(), 2);
        assert_eq!(live_activity_segment(&segments, true), None);
        // Once the turn ends, that same run is the final settled work log.
        assert_eq!(live_activity_segment(&segments, false), Some(0));

        let activity_tail = [entry("assistant", assistant("answer")), command("cmd")];
        let segments = segment_entries(&activity_tail, true).flow;
        assert_eq!(live_activity_segment(&segments, true), Some(1));

        let prose_only = [entry("assistant", assistant("answer"))];
        let segments = segment_entries(&prose_only, true).flow;
        assert_eq!(live_activity_segment(&segments, true), None);
    }

    fn file_change(id: &str, paths: &[&str]) -> Arc<TimelineEntry> {
        entry(
            id,
            EntryContent::Item(ItemContent::FileChange {
                changes: paths
                    .iter()
                    .map(|path| FileChange {
                        path: (*path).to_string(),
                        kind: FileChangeKind::Modify,
                        diff: None,
                    })
                    .collect(),
                status: ItemStatus::Completed,
            }),
        )
    }

    fn refs(entries: &[Arc<TimelineEntry>]) -> Vec<&TimelineEntry> {
        entries.iter().map(AsRef::as_ref).collect()
    }

    #[test]
    fn one_line_collapses_multiline_commands_into_a_single_spaced_line() {
        let cmd = "set pipe -e \"\n  cargo fmt --check\n  cargo clippy\n\"";
        assert_eq!(
            super::one_line(cmd),
            "set pipe -e \" cargo fmt --check cargo clippy \""
        );

        let long = "word ".repeat(500);
        assert_eq!(super::one_line(&long).chars().count(), 600);
    }

    #[test]
    fn break_marker_preview_marks_every_line_break_with_its_range() {
        let cmd = "set pipe -e \"\n  cargo fmt --check\n\n  cargo clippy\n\"";
        let (preview, markers) = super::one_line_with_break_markers(cmd);
        assert_eq!(
            preview,
            "set pipe -e \"\\ncargo fmt --check\\ncargo clippy\\n\""
        );
        assert_eq!(markers.len(), 3);
        for range in markers {
            assert_eq!(&preview[range], "\\n");
        }

        let (single, markers) = super::one_line_with_break_markers("cargo test");
        assert_eq!(single, "cargo test");
        assert!(markers.is_empty());
    }

    #[test]
    fn work_log_labels_localize_prioritized_counts_and_empty_fallbacks() {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        let all = WorkLogCounts {
            commands: 2,
            files: 3,
            tools: 1,
            subagents: 2,
        };
        let tools = WorkLogCounts {
            tools: 2,
            ..Default::default()
        };
        for (locale, full_label, tools_label, fallback) in [
            (
                crate::LANGUAGE_ENGLISH,
                "1 tool call · 3 edits · 2 commands",
                "2 tool calls",
                "1 activity",
            ),
            (
                crate::LANGUAGE_SIMPLIFIED_CHINESE,
                "1 次工具调用 · 3 处编辑 · 2 条命令",
                "2 次工具调用",
                "1 项活动",
            ),
        ] {
            crate::set_locale(locale);
            assert_eq!(work_log_capsule_label(&all, 9), full_label);
            assert_eq!(work_log_capsule_label(&tools, 2), tools_label);
            assert_eq!(work_log_capsule_label(&WorkLogCounts::default(), 0), "");
            assert_eq!(
                work_log_capsule_label(&WorkLogCounts::default(), 1),
                fallback
            );
        }
    }

    #[test]
    fn live_edit_rows_preserve_external_paths_and_report_only_observed_edits() {
        let cases = [
            (
                "/work/repo/src/foo.rs",
                Some(REAL_DIFF),
                "src/foo.rs",
                Some((2, 1)),
            ),
            (
                "/work/repo/crates/ui/src/chat.rs",
                None,
                "crates/ui/src/chat.rs",
                None,
            ),
            (
                "/elsewhere/vendor/bar.rs",
                Some("+only added\n"),
                "/elsewhere/vendor/bar.rs",
                Some((1, 0)),
            ),
            (
                "removed.rs",
                Some("-only removed\n"),
                "removed.rs",
                Some((0, 1)),
            ),
            (
                "literal-signs.txt",
                Some("@@ -1,1 +1,1 @@\n---literal\n+++literal\n"),
                "literal-signs.txt",
                Some((1, 1)),
            ),
            ("empty.rs", Some(""), "empty.rs", None),
            ("blank.rs", Some("   \n\t\n \n"), "blank.rs", None),
            ("headers.rs", Some("--- a/f\n+++ b/f\n"), "headers.rs", None),
            (
                "context.rs",
                Some("--- a/f\n+++ b/f\n@@ -1 +1 @@\n unchanged\n"),
                "context.rs",
                None,
            ),
        ];
        let changes = cases
            .iter()
            .map(|(path, diff, _, _)| FileChange {
                path: (*path).into(),
                kind: FileChangeKind::Modify,
                diff: diff.map(str::to_owned),
            })
            .collect::<Vec<_>>();
        let expected = cases
            .iter()
            .map(|(_, diff, path, counts)| LiveEditRow {
                path: (*path).into(),
                kind: FileChangeKind::Modify,
                counts: *counts,
                diff: diff.map(str::to_owned),
            })
            .collect::<Vec<_>>();
        assert_eq!(live_edit_rows(&changes, Path::new("/work/repo")), expected);
        assert_eq!(diff_stats(Some("--- a/f\n+++ b/f\n")), (0, 0));
        assert_eq!(diff_stats(Some(REAL_DIFF)), (2, 1));
    }

    #[test]
    fn finished_activity_runs_use_segment_scoped_counts() {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        let entries = [
            command("command-1"),
            file_change("files-1", &["src/shared.rs", "src/shared.rs"]),
            file_change("files-2", &["src/shared.rs"]),
            entry("assistant", assistant("intermediate output")),
            command("command-2"),
            command("command-3"),
        ];
        let segments = segment_entries(&entries, false).flow;
        let activity_indexes: Vec<usize> = segments
            .iter()
            .enumerate()
            .filter_map(|(index, segment)| {
                matches!(segment, Segment::ActivityRun(_)).then_some(index)
            })
            .collect();
        assert_eq!(activity_indexes.len(), 2);

        let counts = work_log_counts(&refs(&entries));
        assert_eq!(counts.commands, 3);
        assert_eq!(counts.files, 1);

        let labels = || {
            activity_indexes
                .iter()
                .map(|index| {
                    let Segment::ActivityRun(activities) = &segments[*index] else {
                        unreachable!();
                    };
                    let segment_counts = work_log_counts(activities);
                    work_log_capsule_label(&segment_counts, activities.len())
                })
                .collect::<Vec<_>>()
        };
        crate::set_locale(crate::LANGUAGE_ENGLISH);
        assert_eq!(labels(), ["1 edit · 1 command", "2 commands"]);
        crate::set_locale(crate::LANGUAGE_SIMPLIFIED_CHINESE);
        assert_eq!(labels(), ["1 处编辑 · 1 条命令", "2 条命令"]);
    }

    #[test]
    fn finished_time_clauses_format_cost_and_only_show_a_divergent_served_model() {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        crate::set_locale(crate::LANGUAGE_ENGLISH);

        let clauses = turn_time_clauses(
            "3:04 PM".into(),
            Some(TurnTiming::new(80_000, 35_000)),
            Some(0.12),
            Some("claude-opus-5"),
            Some("claude-fable-5"),
        );
        assert_eq!(
            clauses
                .iter()
                .map(|clause| (clause.selector, clause.text.as_str()))
                .collect::<Vec<_>>(),
            vec![
                ("turn-time-clock", "3:04 PM"),
                ("turn-time-total", "1m20s"),
                ("turn-time-cost", "$0.12"),
                ("turn-time-model", "⚠ claude-opus-5"),
            ]
        );

        let sub_cent = turn_time_clauses(
            "3:04 PM".into(),
            None,
            Some(0.004),
            Some("claude-fable-5"),
            Some("claude-fable-5"),
        );
        assert_eq!(
            sub_cent
                .iter()
                .map(|clause| (clause.selector, clause.text.as_str()))
                .collect::<Vec<_>>(),
            vec![("turn-time-clock", "3:04 PM"), ("turn-time-cost", "$0.004"),]
        );

        let missing_requested =
            turn_time_clauses("3:04 PM".into(), None, None, Some("served"), None);
        assert_eq!(missing_requested.len(), 1);
    }

    #[test]
    fn finished_time_rows_keep_compact_totals_and_localized_breakdowns() {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        for (locale, normal_tooltip, ai_only_tooltip) in [
            (
                crate::LANGUAGE_ENGLISH,
                "AI thinking & response 45s · Tool calls 35s",
                "AI thinking & response 8s · Tool calls 0s",
            ),
            (
                crate::LANGUAGE_SIMPLIFIED_CHINESE,
                "AI 思考与回答 45 秒 · 工具调用 35 秒",
                "AI 思考与回答 8 秒 · 工具调用 0 秒",
            ),
        ] {
            crate::set_locale(locale);
            for (timing, total, tooltip) in [
                (
                    TurnTiming::new(80_000, 35_000),
                    "1m20s",
                    Some(normal_tooltip),
                ),
                (TurnTiming::new(8_000, 0), "8s", Some(ai_only_tooltip)),
                (TurnTiming::new(10_500, 3_600), "10s", None),
                (TurnTiming::new(86_459_000, 84_600_000), "24h00m59s", None),
            ] {
                assert_eq!(
                    turn_time_parts("3:04 PM".into(), Some(timing)),
                    ["3:04 PM", total]
                );
                if let Some(tooltip) = tooltip {
                    assert_eq!(turn_time_breakdown(Some(timing)).as_deref(), Some(tooltip));
                }
            }
            assert_eq!(turn_time_parts("9:00 AM".into(), None), ["9:00 AM"]);
            assert_eq!(turn_time_breakdown(None), None);
        }
    }

    #[test]
    fn the_live_working_indicator_keeps_its_own_format() {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        crate::set_locale(crate::LANGUAGE_ENGLISH);
        assert_eq!(format_duration(3_600), "60m 00s");
        assert_eq!(format_duration(90_061), "1501m 01s");
        assert_eq!(format_span(3_600), "1h 00m 00s");
        assert_eq!(format_span(90_061), "25h 01m 01s");
        assert_eq!(format_span(59), "59s");
        assert_eq!(format_span(90), "1m 30s");
        assert_eq!(format_elapsed_deciseconds(0), "0.0s");
        assert_eq!(format_elapsed_deciseconds(12_399), "12.3s");
        assert_eq!(format_elapsed_deciseconds(65_299), "1m 5.2s");
    }
}
