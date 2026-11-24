//! Import conversation history from supported external coding agents.

mod claude;
mod codex;
mod scan;

use std::collections::HashSet;
use std::path::PathBuf;

use agent::{AgentEvent, ProviderKind, ResumeCursor, ThreadItem, TurnStatus};
use serde_json::json;

use crate::export::{ReadExportError, TcodeThreadExport, read_tcode_export};
use crate::store::{Mutation, SessionStore};
use tcode_core::project::{Project, SessionMeta};
pub use tcode_protocol::{ExternalThread, RecentDir, SourceTool};

#[derive(Debug, Clone)]
pub struct ExternalRoots {
    pub claude_projects: PathBuf,
    pub claude_desktop_meta: PathBuf,
    pub codex_session_roots: Vec<PathBuf>,
}

impl ExternalRoots {
    pub fn detect() -> Self {
        let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
        let codex_home = std::env::var_os("CODEX_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".codex"));
        Self {
            claude_projects: home.join(".claude/projects"),
            claude_desktop_meta: home
                .join("Library/Application Support/Claude/claude-code-sessions"),
            codex_session_roots: vec![
                codex_home.join("sessions"),
                codex_home.join("archived_sessions"),
            ],
        }
    }
}

pub use scan::scan_recent_dirs;

pub fn existing_external_ids(metas: &[SessionMeta]) -> HashSet<String> {
    let mut ids = HashSet::new();
    for meta in metas {
        if let Some(id) = &meta.imported_from {
            ids.insert(id.clone());
        }
        let Some(cursor) = &meta.resume_cursor else {
            continue;
        };
        if let Some(id) = cursor.0.get("session_id").and_then(|value| value.as_str()) {
            ids.insert(format!("claude:{id}"));
        }
        if let Some(id) = cursor.0.get("thread_id").and_then(|value| value.as_str()) {
            ids.insert(format!("codex:{id}"));
        }
    }
    ids
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImportOutcome {
    Imported,
    SkippedDuplicate,
    SkippedEmpty,
    Failed(String),
}

#[derive(Debug)]
struct ConvertedThread {
    external_id: String,
    created_ms: u64,
    updated_ms: u64,
    first_user: Option<String>,
    entries: Vec<ConvertedEntry>,
}

#[derive(Debug)]
enum ConvertedEntry {
    Item { ts: u64, item: ThreadItem },
    Compacted { ts: u64 },
}

impl ConvertedThread {
    fn item_count(&self) -> usize {
        self.entries
            .iter()
            .filter(|entry| matches!(entry, ConvertedEntry::Item { .. }))
            .count()
    }
}

pub fn import_thread(
    store: &SessionStore,
    project: &Project,
    thread: &ExternalThread,
    existing: &mut HashSet<String>,
) -> ImportOutcome {
    if existing.contains(&thread.external_id) {
        return ImportOutcome::SkippedDuplicate;
    }

    if thread.source == SourceTool::T3Code {
        match read_tcode_export(&thread.file) {
            Ok(export) => return import_tcode_thread(store, project, export, existing),
            Err(ReadExportError::NotTcode) => {
                // Older T3 releases persisted Claude's native transcript and
                // are still handled by the Claude converter below.
            }
            Err(ReadExportError::Invalid(error)) => return ImportOutcome::Failed(error),
        }
    }

    let converted = match thread.source {
        SourceTool::ClaudeCode
        | SourceTool::ClaudeDesktop
        | SourceTool::T3Code
        | SourceTool::Unknown => claude::convert(&thread.file, &thread.external_id),
        SourceTool::CodexCli | SourceTool::CodexDesktop => {
            codex::convert(&thread.file, &thread.external_id)
        }
    };
    let converted = match converted {
        Ok(Some(converted)) => converted,
        Ok(None) => return ImportOutcome::SkippedEmpty,
        Err(err) => return ImportOutcome::Failed(err),
    };
    if converted.item_count() == 0 {
        return ImportOutcome::SkippedEmpty;
    }

    let (provider, cursor) = if converted.external_id.starts_with("claude:") {
        let id = converted.external_id.trim_start_matches("claude:");
        (
            ProviderKind::ClaudeCode,
            ResumeCursor(json!({ "session_id": id })),
        )
    } else {
        let id = converted.external_id.trim_start_matches("codex:");
        (
            ProviderKind::Codex,
            ResumeCursor(json!({ "thread_id": id })),
        )
    };

    // Harness-injected user messages (`<task-notification>`, `<system-reminder>`,
    // …) make terrible titles: prefer the first human-looking message.
    let title_user = converted
        .entries
        .iter()
        .find_map(|entry| match entry {
            ConvertedEntry::Item { item, .. } => match &item.content {
                agent::ItemContent::UserMessage { text, .. }
                    if !text.trim_start().starts_with('<') =>
                {
                    Some(text.clone())
                }
                _ => None,
            },
            ConvertedEntry::Compacted { .. } => None,
        })
        .or(converted.first_user.clone());

    let mut meta = SessionMeta::new(provider, project.root.clone(), None);
    meta.project_id = Some(project.id.clone());
    meta.title = thread
        .title_hint
        .clone()
        .or_else(|| title_user.as_deref().map(title_from_text))
        .unwrap_or_else(|| format!("Imported {} thread", thread.source.display_name()));
    meta.created_at = converted.created_ms / 1000;
    meta.updated_at = converted.updated_ms / 1000;
    meta.imported_from = Some(converted.external_id.clone());
    meta.resume_cursor = Some(cursor.clone());

    let mut events = vec![(
        converted.created_ms,
        AgentEvent::SessionStarted {
            provider_session_id: converted
                .external_id
                .split_once(':')
                .map_or_else(|| converted.external_id.clone(), |(_, id)| id.to_string()),
            resume: cursor,
            model: None,
        },
    )];
    let mut open_turn: Option<String> = None;
    let mut turn_number = 0_u64;
    for entry in converted.entries {
        match entry {
            ConvertedEntry::Item { ts, item } => {
                if matches!(item.content, agent::ItemContent::UserMessage { .. }) {
                    if let Some(turn_id) = open_turn.take() {
                        events.push((
                            ts,
                            AgentEvent::TurnCompleted {
                                turn_id,
                                status: TurnStatus::Completed,
                                usage: None,
                            },
                        ));
                    }
                    turn_number += 1;
                    let turn_id = format!("imported-turn-{turn_number}");
                    events.push((ts, AgentEvent::ItemCompleted(item)));
                    events.push((
                        ts,
                        AgentEvent::TurnStarted {
                            turn_id: turn_id.clone(),
                        },
                    ));
                    open_turn = Some(turn_id);
                } else {
                    events.push((ts, AgentEvent::ItemCompleted(item)));
                }
            }
            ConvertedEntry::Compacted { ts } => {
                events.push((ts, AgentEvent::ContextCompacted(Default::default())));
            }
        }
    }
    if let Some(turn_id) = open_turn {
        events.push((
            converted.updated_ms,
            AgentEvent::TurnCompleted {
                turn_id,
                status: TurnStatus::Completed,
                usage: None,
            },
        ));
    }

    // The transcript and its meta commit together, so a failure never leaves
    // an orphan log behind.
    let mut mutations = Vec::with_capacity(events.len() + 1);
    for (ts, event) in &events {
        match Mutation::append_event(&meta.id, *ts, event) {
            Ok(mutation) => mutations.push(mutation),
            Err(err) => {
                return ImportOutcome::Failed(format!("failed to encode imported events: {err}"));
            }
        }
    }
    mutations.push(Mutation::upsert_meta(meta));
    if let Err(err) = store.apply(&mutations) {
        return ImportOutcome::Failed(format!("failed to write imported session: {err}"));
    }
    existing.insert(converted.external_id);
    ImportOutcome::Imported
}

fn import_tcode_thread(
    store: &SessionStore,
    project: &Project,
    export: TcodeThreadExport,
    existing: &mut HashSet<String>,
) -> ImportOutcome {
    if export.events.is_empty() {
        return ImportOutcome::SkippedEmpty;
    }
    let external_id = format!("tcode:{}", export.meta.id);
    if existing.contains(&external_id) {
        return ImportOutcome::SkippedDuplicate;
    }

    let mut meta = export.meta;
    let fresh = SessionMeta::new(meta.provider, project.root.clone(), meta.model.clone());
    meta.id = fresh.id;
    meta.cwd = project.root.clone();
    meta.project_id = Some(project.id.clone());
    meta.archived_at = None;
    meta.worktree = None;
    meta.parent_session_id = None;
    meta.imported_from = Some(external_id.clone());

    let mutations = [
        Mutation::replace_event_log(&meta.id, export.event_log),
        Mutation::upsert_meta(meta),
    ];
    if let Err(error) = store.apply(&mutations) {
        return ImportOutcome::Failed(format!("failed to write imported session: {error}"));
    }
    existing.insert(external_id);
    ImportOutcome::Imported
}

fn title_from_text(text: &str) -> String {
    let first_line = text.lines().next().unwrap_or_default().trim();
    let mut title: String = first_line.chars().take(64).collect();
    if first_line.chars().count() > 64 {
        title.push('…');
    }
    title
}

fn timestamp_ms(value: &serde_json::Value) -> Option<u64> {
    let text = value.as_str()?;
    chrono::DateTime::parse_from_rfc3339(text)
        .ok()
        .and_then(|timestamp| u64::try_from(timestamp.timestamp_millis()).ok())
}

fn file_modified_ms(path: &std::path::Path) -> u64 {
    std::fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .ok()
        .and_then(|modified| modified.duration_since(std::time::UNIX_EPOCH).ok())
        .and_then(|duration| u64::try_from(duration.as_millis()).ok())
        .unwrap_or(0)
}

fn collect_files(
    root: &std::path::Path,
    predicate: &impl Fn(&std::path::Path) -> bool,
) -> Vec<PathBuf> {
    fn visit(
        dir: &std::path::Path,
        predicate: &impl Fn(&std::path::Path) -> bool,
        files: &mut Vec<PathBuf>,
    ) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            // Not followed through symlinks: a link back to an ancestor would
            // make the walk branch without end.
            if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                visit(&path, predicate, files);
            } else if predicate(&path) {
                files.push(path);
            }
        }
    }
    let mut files = Vec::new();
    visit(root, predicate, &mut files);
    files
}

#[cfg(test)]
mod tests;
