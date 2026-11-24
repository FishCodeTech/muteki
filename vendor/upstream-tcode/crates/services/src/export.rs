//! User-facing thread export formats.
//!
//! JSONL is the backup format: its first record carries the complete session
//! metadata and an explicit privacy policy, and all following bytes are the
//! native append-only event log. Attachment files are referenced by their
//! recorded paths, not copied into the export. No transcript content is
//! redacted automatically.
//!
//! Markdown is for reading, not restoration. It reuses the provider-relay
//! transcript renderer with its size limit disabled, then adds stable metadata
//! and attachment-reference sections. Provider secrets/settings and attachment
//! bytes are never included; recorded messages and tool summaries are not
//! redacted.

use std::fs;
use std::io;
use std::path::Path;

use agent::{ItemContent, PlanStepStatus};
use serde::{Deserialize, Serialize};
use tcode_core::project::SessionMeta;
use tcode_core::relay::render_relay_transcript;
use tcode_core::session::{EntryContent, StoredEvent, Timeline};
use tcode_protocol::ThreadExportFormat;

use crate::store::{SessionStore, parse_stored_line};

const EXPORT_VERSION: u32 = 1;

#[derive(Debug, Serialize, Deserialize)]
struct TcodeThreadHeader {
    #[serde(rename = "type")]
    kind: String,
    version: u32,
    meta: SessionMeta,
    attachments: String,
    redaction: String,
}

#[derive(Debug)]
pub(crate) struct TcodeThreadExport {
    pub meta: SessionMeta,
    pub event_log: Vec<u8>,
    pub events: Vec<StoredEvent>,
}

#[derive(Debug)]
pub(crate) enum ReadExportError {
    NotTcode,
    Invalid(String),
}

/// Render `meta` and its persisted event log into export bytes. Nothing is
/// written: the client that asked owns the destination.
pub fn render_thread(
    store: &SessionStore,
    meta: &SessionMeta,
    format: ThreadExportFormat,
) -> io::Result<Vec<u8>> {
    let event_log = store.read_event_log(&meta.id)?;
    match format {
        ThreadExportFormat::Jsonl => render_jsonl(meta, &event_log),
        ThreadExportFormat::Markdown => {
            let events = parse_event_log(&event_log, meta.provider).map_err(invalid_data)?;
            Ok(render_markdown(meta, &Timeline::fold_events(events)).into_bytes())
        }
    }
}

/// IANA type for a rendered export, so a client can hand the bytes to a
/// download or share sheet without re-deriving it from the extension.
pub fn export_mime(format: ThreadExportFormat) -> &'static str {
    match format {
        ThreadExportFormat::Jsonl => "application/x-ndjson",
        ThreadExportFormat::Markdown => "text/markdown",
    }
}

/// A file name for `title` that is legal on every client OS we ship to, so the
/// client never has to re-sanitize a host-authored title.
pub fn export_file_name(title: &str, format: ThreadExportFormat) -> String {
    let extension = match format {
        ThreadExportFormat::Jsonl => "jsonl",
        ThreadExportFormat::Markdown => "md",
    };
    let sanitized: String = title
        .chars()
        .map(|character| match character {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '-',
            character if character.is_control() => '-',
            character => character,
        })
        .collect();
    let sanitized = sanitized.trim().trim_matches('.');
    let stem: String = if sanitized.is_empty() {
        "thread".into()
    } else {
        sanitized.chars().take(120).collect()
    };
    format!("{stem}.{extension}")
}

fn render_jsonl(meta: &SessionMeta, event_log: &[u8]) -> io::Result<Vec<u8>> {
    let header = TcodeThreadHeader {
        kind: "tcode_thread".into(),
        version: EXPORT_VERSION,
        meta: meta.clone(),
        attachments: "references_only".into(),
        redaction: "none".into(),
    };
    let mut output = serde_json::to_vec(&header).map_err(invalid_data)?;
    output.push(b'\n');
    output.extend_from_slice(event_log);
    if !event_log.is_empty() && !event_log.ends_with(b"\n") {
        output.push(b'\n');
    }
    Ok(output)
}

/// Read and validate a tcode JSONL export. `NotTcode` lets the external-history
/// importer retain support for older T3-authored Claude transcripts.
pub(crate) fn read_tcode_export(path: &Path) -> Result<TcodeThreadExport, ReadExportError> {
    let bytes = fs::read(path).map_err(|error| ReadExportError::Invalid(error.to_string()))?;
    let (header, event_log) = match bytes.iter().position(|byte| *byte == b'\n') {
        Some(index) => (&bytes[..index], bytes[index + 1..].to_vec()),
        None => (bytes.as_slice(), Vec::new()),
    };
    let value: serde_json::Value =
        serde_json::from_slice(header).map_err(|_| ReadExportError::NotTcode)?;
    if value.get("type").and_then(serde_json::Value::as_str) != Some("tcode_thread") {
        return Err(ReadExportError::NotTcode);
    }
    let header: TcodeThreadHeader = serde_json::from_value(value)
        .map_err(|error| ReadExportError::Invalid(error.to_string()))?;
    if header.version != EXPORT_VERSION {
        return Err(ReadExportError::Invalid(format!(
            "unsupported Tcode export version {}",
            header.version
        )));
    }
    let events =
        parse_event_log(&event_log, header.meta.provider).map_err(ReadExportError::Invalid)?;
    Ok(TcodeThreadExport {
        meta: header.meta,
        event_log,
        events,
    })
}

fn parse_event_log(
    bytes: &[u8],
    current_provider: agent::ProviderKind,
) -> Result<Vec<StoredEvent>, String> {
    let text = std::str::from_utf8(bytes).map_err(|error| error.to_string())?;
    let mut provider = current_provider;
    let mut events = Vec::new();
    for line in text.lines().filter(|line| !line.trim().is_empty()).rev() {
        let record = parse_stored_line(line).map_err(|error| error.to_string())?;
        provider = record.previous_provider().unwrap_or(provider);
        events.push(record.into_stored(Some(provider)));
    }
    events.reverse();
    Ok(events)
}

/// Render a complete folded timeline without the relay renderer's handoff size
/// cap. The relay walker already defines stable turn/message/tool/plan order;
/// export supplements the metadata and attachment behavior it intentionally
/// omits for provider handoff.
pub fn render_markdown(meta: &SessionMeta, timeline: &Timeline) -> String {
    let model = meta.model.as_deref().unwrap_or("provider default");
    let mut output = format!(
        "# {}\n\n- Format: Tcode Markdown export v1\n- Session ID: `{}`\n- Provider/model: {} / {}\n- Workspace: `{}`\n- Created: {} (Unix seconds)\n- Updated: {} (Unix seconds)\n- Attachments: referenced by recorded local path; file bytes are not embedded.\n- Privacy: no automatic redaction; recorded messages and tool summaries may contain sensitive data. Provider secrets and settings are not included.\n\n",
        meta.title,
        meta.id,
        meta.provider.display_name(),
        model,
        meta.cwd.display(),
        meta.created_at,
        meta.updated_at,
    );
    let transcript = render_relay_transcript(
        timeline,
        &meta.cwd,
        meta.provider,
        meta.model.as_deref(),
        usize::MAX,
    );
    if transcript.is_empty() {
        output.push_str("## Conversation\n\n_No completed turns._\n\n");
        render_incomplete_timeline(&mut output, timeline);
    } else {
        output.push_str(&transcript.replacen("# Conversation relay", "## Conversation", 1));
    }

    let attachments = attachment_references(timeline);
    if !attachments.is_empty() {
        output.push_str("\n## Attachment references\n\n");
        for path in attachments {
            output.push_str("- `");
            output.push_str(&path.replace('`', "\\`"));
            output.push_str("`\n");
        }
    }
    if !output.ends_with('\n') {
        output.push('\n');
    }
    output
}

fn render_incomplete_timeline(output: &mut String, timeline: &Timeline) {
    for turn in 0..timeline.turns.len() {
        output.push_str(&format!("### Turn {} (incomplete)\n\n", turn + 1));
        for entry in timeline.entries.iter().filter(|entry| entry.turn == turn) {
            match &entry.content {
                EntryContent::Item(ItemContent::UserMessage {
                    text, context_len, ..
                })
                | EntryContent::Steer {
                    text, context_len, ..
                } => {
                    let visible = context_len.and_then(|len| text.get(len..)).unwrap_or(text);
                    output.push_str("#### User\n\n");
                    output.push_str(visible);
                    output.push_str("\n\n");
                }
                EntryContent::Item(ItemContent::AssistantMessage { text }) => {
                    output.push_str("#### Assistant\n\n");
                    output.push_str(text);
                    output.push_str("\n\n");
                }
                EntryContent::Item(ItemContent::Reasoning { .. }) => {}
                activity => output.push_str(&format!("- Activity: `{activity:?}`\n")),
            }
        }
        output.push('\n');
    }
    if !timeline.plan_steps.is_empty() {
        output.push_str("### Current todo state\n\n");
        if let Some(explanation) = timeline.plan_explanation.as_deref() {
            output.push_str(explanation);
            output.push_str("\n\n");
        }
        for step in &timeline.plan_steps {
            let (marker, suffix) = match step.status {
                PlanStepStatus::Completed => ("x", ""),
                PlanStepStatus::InProgress => (" ", " (in progress)"),
                PlanStepStatus::Pending => (" ", ""),
            };
            output.push_str(&format!("- [{marker}] {}{suffix}\n", step.step));
        }
    }
}

fn attachment_references(timeline: &Timeline) -> Vec<String> {
    let mut paths = Vec::new();
    for entry in &timeline.entries {
        match &entry.content {
            EntryContent::Item(ItemContent::UserMessage { attachments, .. })
            | EntryContent::Steer { attachments, .. } => paths.extend(attachments.iter().cloned()),
            _ => {}
        }
    }
    paths
}

fn invalid_data(error: impl ToString) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error.to_string())
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::path::PathBuf;

    use agent::{
        AgentEvent, ItemContent, ItemStatus, PlanStep, PlanStepStatus, ProviderKind, ThreadItem,
        TurnStatus,
    };
    use tcode_core::project::Project;
    use tcode_protocol::{ExternalThread, SourceTool};

    use super::*;
    use crate::import::{ImportOutcome, import_thread};

    fn temp_root(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!("tcode-export-{label}-{}", uuid::Uuid::new_v4()))
    }

    fn item(id: &str, content: ItemContent) -> AgentEvent {
        AgentEvent::ItemCompleted(ThreadItem {
            id: id.into(),
            parent_item_id: None,
            content,
        })
    }

    fn fixture(store: &SessionStore) -> SessionMeta {
        let mut meta = SessionMeta::new(
            ProviderKind::ClaudeCode,
            PathBuf::from("/work/project"),
            Some("opus".into()),
        );
        meta.id = "session-export-1".into();
        meta.title = "Export fixture".into();
        meta.created_at = 100;
        meta.updated_at = 200;
        let events = [
            AgentEvent::ItemCompleted(ThreadItem {
                id: "user-1".into(),
                parent_item_id: None,
                content: ItemContent::UserMessage {
                    text: "Inspect the project".into(),
                    context_len: None,
                    attachments: vec!["/work/project/screenshot.png".into()],
                },
            }),
            AgentEvent::TurnStarted {
                turn_id: "turn-1".into(),
            },
            item(
                "tool-1",
                ItemContent::ToolCall {
                    presentation: None,
                    image_reads: Vec::new(),
                    name: "read_file".into(),
                    input: serde_json::json!({"path": "src/main.rs"}),
                    output: Some("read 12 lines".into()),
                    status: ItemStatus::Completed,
                },
            ),
            AgentEvent::PlanUpdated {
                turn_id: Some("turn-1".into()),
                explanation: Some("Implementation order".into()),
                steps: vec![
                    PlanStep {
                        step: "Inspect".into(),
                        status: PlanStepStatus::Completed,
                    },
                    PlanStep {
                        step: "Implement".into(),
                        status: PlanStepStatus::InProgress,
                    },
                ],
            },
            item(
                "assistant-1",
                ItemContent::AssistantMessage {
                    text: "The project is ready.".into(),
                },
            ),
            AgentEvent::TurnCompleted {
                turn_id: "turn-1".into(),
                status: TurnStatus::Completed,
                usage: None,
            },
        ];
        for (index, event) in events.iter().enumerate() {
            store
                .append_event(&meta.id, 1_000 + index as u64, event)
                .unwrap();
        }
        store.upsert_meta(&meta).unwrap();
        meta
    }

    #[test]
    fn legacy_plan_and_approval_log_loads_and_exports() {
        let root = temp_root("legacy-plan");
        fs::create_dir_all(&root).unwrap();
        let mut meta =
            serde_json::to_value(SessionMeta::new(ProviderKind::Codex, root.clone(), None))
                .unwrap();
        meta["id"] = serde_json::json!("legacy-plan");
        meta["interaction_mode"] = serde_json::json!("plan");
        meta["approval_mode"] = serde_json::json!("full_access");
        fs::write(
            root.join("sessions.json"),
            serde_json::to_vec(&serde_json::json!([meta])).unwrap(),
        )
        .unwrap();
        let log = br##"{"type":"turn_started","turn_id":"old-turn"}
{"ts":2,"event":{"type":"proposed_plan_delta","item_id":"old-plan","text":"# Draft"}}
{"ts":3,"event":{"type":"proposed_plan","item_id":"old-plan","markdown":"# Stored plan\n\nKeep this content."}}
{"ts":4,"event":{"type":"plan_resolved","item_id":"old-plan","resolution":"implemented"}}
{"ts":5,"event":{"type":"approval_resolved","request_id":"old-once","decision":"approve"}}
{"ts":6,"event":{"type":"approval_resolved","request_id":"old-session","decision":"approve_for_session"}}
{"ts":7,"event":{"type":"approval_resolved","request_id":"old-denied","decision":"deny"}}
{"ts":8,"event":{"type":"turn_completed","turn_id":"old-turn","status":"completed","usage":null}}
"##;
        fs::write(root.join("legacy-plan.jsonl"), log).unwrap();
        let store = SessionStore::open_at(root.clone()).unwrap();
        store
            .migrate(|_| {}, &std::sync::atomic::AtomicBool::new(false))
            .unwrap();
        let meta = store.load_index().unwrap().pop().unwrap();
        assert!(
            serde_json::to_value(&meta)
                .unwrap()
                .get("interaction_mode")
                .is_none()
        );
        let events = store.read_events(&meta.id).unwrap();
        assert_eq!(events.len(), 8);
        let decisions = events
            .iter()
            .filter_map(|stored| match &stored.event {
                AgentEvent::ApprovalResolved { decision, .. } => Some(decision.clone()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            decisions,
            [
                agent::ApprovalDecision::Option("legacy:approve".into()),
                agent::ApprovalDecision::Option("legacy:approve_for_session".into()),
                agent::ApprovalDecision::Option("legacy:deny".into()),
            ]
        );
        assert!(
            serde_json::to_value(&meta)
                .unwrap()
                .get("approval_mode")
                .is_none()
        );
        let timeline = Timeline::fold_events(events);
        let text: Vec<_> = timeline
            .entries
            .iter()
            .filter_map(|entry| match &entry.content {
                EntryContent::Item(ItemContent::AssistantMessage { text }) => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(text, ["# Stored plan\n\nKeep this content."]);
        let markdown =
            String::from_utf8(render_thread(&store, &meta, ThreadExportFormat::Markdown).unwrap())
                .unwrap();
        assert!(markdown.contains("# Stored plan\n\nKeep this content."));
        assert!(!markdown.contains("# Draft"));
        let export = render_thread(&store, &meta, ThreadExportFormat::Jsonl).unwrap();
        let (_, exported_log) =
            export.split_at(export.iter().position(|byte| *byte == b'\n').unwrap() + 1);
        assert_eq!(exported_log, log);
        let path = root.join("export.jsonl");
        fs::write(&path, export).unwrap();
        assert_eq!(read_tcode_export(&path).unwrap().events.len(), 8);
        drop(store);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn jsonl_export_import_round_trips_folded_timeline() {
        let source_root = temp_root("source");
        let destination_root = temp_root("destination");
        let source = SessionStore::open_at(source_root.clone()).unwrap();
        let destination = SessionStore::open_at(destination_root.clone()).unwrap();
        let meta = fixture(&source);
        let export_path = source_root.join("thread.jsonl");
        fs::write(
            &export_path,
            render_thread(&source, &meta, ThreadExportFormat::Jsonl).unwrap(),
        )
        .unwrap();
        let exported = fs::read_to_string(&export_path).unwrap();
        let header: serde_json::Value =
            serde_json::from_str(exported.lines().next().unwrap()).unwrap();
        assert_eq!(header["type"], "tcode_thread");
        assert_eq!(header["version"], 1);
        assert_eq!(header["attachments"], "references_only");
        assert_eq!(header["redaction"], "none");
        assert_eq!(
            exported.split_once('\n').unwrap().1.as_bytes(),
            source.read_event_log(&meta.id).unwrap()
        );

        let project = Project::from_root(PathBuf::from("/restored/project"));
        destination.upsert_project(&project).unwrap();
        let thread = ExternalThread {
            source: SourceTool::T3Code,
            file: export_path,
            external_id: "tcode:session-export-1".into(),
            title_hint: None,
            last_active_ms: 0,
        };
        assert_eq!(
            import_thread(&destination, &project, &thread, &mut HashSet::new()),
            ImportOutcome::Imported
        );

        let restored = destination.load_index().unwrap().pop().unwrap();
        assert_ne!(restored.id, meta.id);
        assert_eq!(restored.cwd, project.root);
        assert_eq!(restored.project_id.as_deref(), Some(project.id.as_str()));
        assert_eq!(
            restored.imported_from.as_deref(),
            Some("tcode:session-export-1")
        );
        assert_eq!(
            destination.read_event_log(&restored.id).unwrap(),
            source.read_event_log(&meta.id).unwrap()
        );
        let source_timeline = Timeline::fold_events(source.read_events(&meta.id).unwrap());
        let restored_timeline =
            Timeline::fold_events(destination.read_events(&restored.id).unwrap());
        let summarize = |timeline: &Timeline| {
            timeline
                .entries
                .iter()
                .map(|entry| (entry.turn, format!("{:?}", entry.content)))
                .collect::<Vec<_>>()
        };
        assert_eq!(summarize(&source_timeline), summarize(&restored_timeline));
        assert_eq!(source_timeline.plan_steps, restored_timeline.plan_steps);
        assert_eq!(
            source_timeline.plan_explanation,
            restored_timeline.plan_explanation
        );
        assert_eq!(source_timeline.turns.len(), restored_timeline.turns.len());

        let _ = fs::remove_dir_all(source_root);
        let _ = fs::remove_dir_all(destination_root);
    }

    #[test]
    fn markdown_is_deterministic_and_matches_fixture_snapshot() {
        let root = temp_root("markdown");
        let store = SessionStore::open_at(root.clone()).unwrap();
        let meta = fixture(&store);
        let rendered = render_thread(&store, &meta, ThreadExportFormat::Markdown).unwrap();
        assert_eq!(
            String::from_utf8(rendered).unwrap(),
            concat!(
                "# Export fixture\n\n",
                "- Format: Tcode Markdown export v1\n",
                "- Session ID: `session-export-1`\n",
                "- Provider/model: Claude Code / opus\n",
                "- Workspace: `/work/project`\n",
                "- Created: 100 (Unix seconds)\n",
                "- Updated: 200 (Unix seconds)\n",
                "- Attachments: referenced by recorded local path; file bytes are not embedded.\n",
                "- Privacy: no automatic redaction; recorded messages and tool summaries may contain sensitive data. Provider secrets and settings are not included.\n\n",
                "## Conversation\n\n",
                "- Project: `/work/project`\n",
                "- Original provider/model: Claude Code / opus\n",
                "- Completed turns: 1\n\n",
                "## Turn 1\n\n",
                "### User\n\n",
                "Inspect the project\n\n",
                "- `read_file` — src/main.rs — read 12 lines\n",
                "### Assistant\n\n",
                "The project is ready.\n\n",
                "### Current todo state\n\n",
                "Implementation order\n\n",
                "- [x] Inspect\n",
                "- [ ] Implement (in progress)\n\n",
                "---\n",
                "This is where the previous agent left off.\n\n",
                "## Attachment references\n\n",
                "- `/work/project/screenshot.png`\n",
            )
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn markdown_keeps_the_full_tool_heavy_timeline() {
        let mut events = vec![item(
            "user",
            ItemContent::UserMessage {
                text: "Run every check".into(),
                context_len: None,
                attachments: Vec::new(),
            },
        )];
        for index in 0..512 {
            events.push(item(
                &format!("tool-{index}"),
                ItemContent::ToolCall {
                    presentation: None,
                    image_reads: Vec::new(),
                    name: format!("check-{index}"),
                    input: serde_json::json!({"path": format!("file-{index}.rs")}),
                    output: Some(format!("result-{index}")),
                    status: ItemStatus::Completed,
                },
            ));
        }
        events.push(AgentEvent::TurnCompleted {
            turn_id: "turn".into(),
            status: TurnStatus::Completed,
            usage: None,
        });
        let timeline = Timeline::fold_events(events);
        let mut meta = SessionMeta::new(ProviderKind::Codex, PathBuf::from("/work"), None);
        meta.title = "Tool-heavy".into();

        let markdown = render_markdown(&meta, &timeline);
        assert!(markdown.contains("`check-0` — file-0.rs — result-0"));
        assert!(markdown.contains("`check-511` — file-511.rs — result-511"));
        assert!(!markdown.contains("earlier turns elided"));
    }
}
