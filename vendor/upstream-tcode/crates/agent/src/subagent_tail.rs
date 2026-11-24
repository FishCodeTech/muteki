//! Polling reader and canonical mapper for Claude subagent transcript JSONL.

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::claude::{is_agent_tool, one_line_summary, spawned_subagent};
use crate::{AgentEvent, ItemContent, ItemStatus, ThreadItem};

enum ChildTool {
    Tool {
        name: String,
        input: Value,
    },
    /// An `Agent` call inside the subagent: a nested subagent whose own
    /// transcript arrives keyed by this tool_use id.
    Subagent {
        content: ItemContent,
        background: bool,
    },
}

/// Stateful mapper for one subagent transcript. Tool calls are retained until
/// their matching result arrives so completion snapshots keep the original input.
pub(crate) struct TranscriptMapper {
    parent_id: String,
    tools: HashMap<String, ChildTool>,
    next_user_id: u64,
    /// Latest model/effort seen on an assistant record, until taken.
    model: Option<(Option<String>, Option<String>)>,
}

impl TranscriptMapper {
    pub(crate) fn new(parent_id: impl Into<String>) -> Self {
        Self {
            parent_id: parent_id.into(),
            tools: HashMap::new(),
            next_user_id: 0,
            model: None,
        }
    }

    pub(crate) fn map_value(&mut self, value: &Value) -> Vec<AgentEvent> {
        if should_skip(value) {
            return Vec::new();
        }
        if let Some(seen) = model_and_effort(value) {
            self.model = Some(seen);
        }
        match value.get("type").and_then(Value::as_str) {
            Some("assistant") => self.map_assistant(value),
            Some("user") => self.map_user(value),
            _ => Vec::new(),
        }
    }

    fn map_assistant(&mut self, value: &Value) -> Vec<AgentEvent> {
        let Some(blocks) = value.pointer("/message/content").and_then(Value::as_array) else {
            return Vec::new();
        };
        let message_id = value
            .pointer("/message/id")
            .or_else(|| value.get("uuid"))
            .and_then(Value::as_str)
            .unwrap_or("assistant");
        let mut events = Vec::new();
        for (index, block) in blocks.iter().enumerate() {
            match block.get("type").and_then(Value::as_str) {
                Some("text") => {
                    let text = block
                        .get("text")
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    if !text.is_empty() {
                        events.push(AgentEvent::ItemCompleted(self.item(
                            format!("{message_id}:{index}"),
                            ItemContent::AssistantMessage { text: text.into() },
                        )));
                    }
                }
                Some("thinking") => {
                    let text = block
                        .get("thinking")
                        .or_else(|| block.get("text"))
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    if !text.is_empty() {
                        events.push(AgentEvent::ItemCompleted(self.item(
                            format!("{message_id}:{index}"),
                            ItemContent::Reasoning { text: text.into() },
                        )));
                    }
                }
                Some("tool_use") => {
                    let Some(id) = block.get("id").and_then(Value::as_str) else {
                        continue;
                    };
                    let name = block
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or("tool")
                        .to_owned();
                    let input = block.get("input").cloned().unwrap_or(Value::Null);
                    if is_agent_tool(&name.to_lowercase()) {
                        let content = spawned_subagent(&input);
                        let background = input
                            .get("run_in_background")
                            .and_then(Value::as_bool)
                            .unwrap_or(false);
                        self.tools.insert(
                            id.to_owned(),
                            ChildTool::Subagent {
                                content: content.clone(),
                                background,
                            },
                        );
                        events.push(AgentEvent::ItemStarted(self.nested_subagent(id, content)));
                        continue;
                    }
                    self.tools.insert(
                        id.to_owned(),
                        ChildTool::Tool {
                            name: name.clone(),
                            input: input.clone(),
                        },
                    );
                    events.push(AgentEvent::ItemStarted(self.item(
                        id,
                        ItemContent::ToolCall {
                            presentation: None,
                            image_reads: Vec::new(),
                            name,
                            input,
                            output: None,
                            status: ItemStatus::InProgress,
                        },
                    )));
                }
                _ => {}
            }
        }
        events
    }

    fn map_user(&mut self, value: &Value) -> Vec<AgentEvent> {
        let Some(content) = value.pointer("/message/content") else {
            return Vec::new();
        };
        let blocks: Vec<&Value> = match content.as_array() {
            Some(blocks) => blocks.iter().collect(),
            None => vec![content],
        };
        let mut events = Vec::new();
        for block in blocks {
            if let Some(text) = block.as_str().or_else(|| {
                (block.get("type").and_then(Value::as_str) == Some("text"))
                    .then(|| block.get("text").and_then(Value::as_str))
                    .flatten()
            }) {
                if is_user_noise(text) {
                    continue;
                }
                self.next_user_id += 1;
                events.push(AgentEvent::ItemCompleted(self.item(
                    format!("user-{}", self.next_user_id),
                    ItemContent::UserMessage {
                        text: text.into(),
                        context_len: None,
                        attachments: Vec::new(),
                    },
                )));
                continue;
            }
            if block.get("type").and_then(Value::as_str) != Some("tool_result") {
                continue;
            }
            let Some(id) = block.get("tool_use_id").and_then(Value::as_str) else {
                continue;
            };
            let Some(tool) = self.tools.remove(id) else {
                continue;
            };
            let failed = block
                .get("is_error")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let output = content_text(block.get("content"));
            let status = if failed {
                ItemStatus::Failed
            } else {
                ItemStatus::Completed
            };
            match tool {
                ChildTool::Tool { name, input } => {
                    let image_reads = if failed {
                        Vec::new()
                    } else {
                        crate::tool_result_images(
                            &name,
                            block.get("content").unwrap_or(&Value::Null),
                        )
                    };
                    events.push(AgentEvent::ItemCompleted(self.item(
                        id,
                        ItemContent::ToolCall {
                            presentation: None,
                            image_reads,
                            name,
                            input,
                            output: Some(output),
                            status,
                        },
                    )));
                }
                ChildTool::Subagent {
                    content,
                    background,
                } => {
                    // A background launch only acknowledges the spawn; the
                    // nested subagent settles through its own task_notification.
                    if background || launch_acknowledged(value, &output) {
                        continue;
                    }
                    let ItemContent::Subagent {
                        agent_type,
                        description,
                        model,
                        effort,
                        ..
                    } = content
                    else {
                        continue;
                    };
                    events.push(AgentEvent::ItemCompleted(self.nested_subagent(
                        id,
                        ItemContent::Subagent {
                            agent_type,
                            description,
                            status,
                            summary: (!output.trim().is_empty()).then(|| one_line_summary(&output)),
                            model,
                            effort,
                        },
                    )));
                }
            }
        }
        events
    }

    fn item(&self, raw_id: impl std::fmt::Display, content: ItemContent) -> ThreadItem {
        ThreadItem {
            id: format!("{}:{raw_id}", self.parent_id),
            parent_item_id: Some(self.parent_id.clone()),
            content,
        }
    }

    /// A nested spawn keeps its bare tool_use id: the grandchild's transcript
    /// lines name it as their `parent_tool_use_id`.
    fn nested_subagent(&self, tool_use_id: &str, content: ItemContent) -> ThreadItem {
        ThreadItem {
            id: tool_use_id.to_owned(),
            parent_item_id: Some(self.parent_id.clone()),
            content,
        }
    }
}

/// Whether a nested Agent result only reports an asynchronous launch. Claude
/// writes `toolUseResult.status` for top-level agents but omits it from
/// subagent transcripts, where the acknowledgement text is the only marker.
fn launch_acknowledged(value: &Value, output: &str) -> bool {
    ["/tool_use_result/status", "/toolUseResult/status"]
        .iter()
        .any(|pointer| value.pointer(pointer).and_then(Value::as_str) == Some("async_launched"))
        || output.starts_with("Async agent launched successfully")
}

/// Incremental reader used both by the polling task and deterministic tests.
pub(crate) struct TailReader {
    path: PathBuf,
    offset: u64,
    mapper: TranscriptMapper,
}

impl TailReader {
    pub(crate) fn new(path: PathBuf, parent_id: impl Into<String>) -> Self {
        Self {
            path,
            offset: 0,
            mapper: TranscriptMapper::new(parent_id),
        }
    }

    /// Model/effort observed since the last call, if any record carried them.
    pub(crate) fn take_model(&mut self) -> Option<(Option<String>, Option<String>)> {
        self.mapper.model.take()
    }

    pub(crate) fn read_appended(&mut self) -> std::io::Result<Vec<AgentEvent>> {
        let mut file = File::open(&self.path)?;
        if file.metadata()?.len() < self.offset {
            self.offset = 0;
        }
        file.seek(SeekFrom::Start(self.offset))?;
        let mut reader = BufReader::new(file);
        let mut events = Vec::new();
        loop {
            let start = self.offset;
            let mut line = String::new();
            let bytes = reader.read_line(&mut line)?;
            if bytes == 0 {
                break;
            }
            if !line.ends_with('\n') {
                self.offset = start;
                break;
            }
            self.offset += bytes as u64;
            if let Ok(value) = serde_json::from_str::<Value>(&line) {
                events.extend(self.mapper.map_value(&value));
            }
        }
        Ok(events)
    }
}

pub(crate) fn read_subagent_spawn(
    claude_dir: &Path,
    session_id: &str,
    task_id: &str,
) -> Option<ThreadItem> {
    let session_dir = find_named_dir(&claude_dir.join("projects"), session_id, 5)?;
    let name = format!("agent-{task_id}.meta.json");
    let path = std::fs::read_dir(session_dir.join("subagents"))
        .ok()?
        .flatten()
        .map(|entry| entry.path())
        .find(|path| path.file_name().is_some_and(|file| file == name.as_str()))?;
    let meta: Value = serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()?;
    Some(ThreadItem {
        id: meta.get("toolUseId")?.as_str()?.to_owned(),
        parent_item_id: None,
        content: ItemContent::Subagent {
            agent_type: meta.get("agentType")?.as_str()?.to_owned(),
            description: meta.get("description")?.as_str()?.to_owned(),
            status: ItemStatus::Completed,
            summary: None,
            model: None,
            effort: None,
        },
    })
}

pub(crate) fn find_transcript(
    claude_dir: &Path,
    session_id: &str,
    task_id: &str,
    tool_use_id: &str,
) -> Option<PathBuf> {
    let session_dir = find_named_dir(&claude_dir.join("projects"), session_id, 5)?;
    let task_path = session_dir.join("tasks").join(format!("{task_id}.output"));
    if task_path.is_file() {
        return Some(task_path);
    }
    let subagents = session_dir.join("subagents");
    for entry in std::fs::read_dir(subagents).ok()?.flatten() {
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("json")
            || !path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.ends_with(".meta.json"))
        {
            continue;
        }
        let Ok(meta) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(meta) = serde_json::from_str::<Value>(&meta) else {
            continue;
        };
        if meta.get("toolUseId").and_then(Value::as_str) == Some(tool_use_id) {
            let transcript = path.with_file_name(
                path.file_name()
                    .and_then(|name| name.to_str())?
                    .trim_end_matches(".meta.json")
                    .to_owned()
                    + ".jsonl",
            );
            return transcript.is_file().then_some(transcript);
        }
    }
    None
}

fn find_named_dir(root: &Path, name: &str, depth: usize) -> Option<PathBuf> {
    if depth == 0 {
        return None;
    }
    for entry in std::fs::read_dir(root).ok()?.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        if path.file_name().and_then(|part| part.to_str()) == Some(name) {
            return Some(path);
        }
        if let Some(found) = find_named_dir(&path, name, depth - 1) {
            return Some(found);
        }
    }
    None
}

/// `(model, effort)` from an assistant record: `message.model` and the
/// top-level `effort` that Claude writes to transcripts.
pub(crate) fn model_and_effort(value: &Value) -> Option<(Option<String>, Option<String>)> {
    if value.get("type").and_then(Value::as_str) != Some("assistant") {
        return None;
    }
    let text = |value: Option<&Value>| {
        value
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty())
            .map(str::to_owned)
    };
    let model = text(value.pointer("/message/model"));
    let effort = text(value.get("effort"));
    (model.is_some() || effort.is_some()).then_some((model, effort))
}

fn should_skip(value: &Value) -> bool {
    matches!(
        value.get("type").and_then(Value::as_str),
        Some("queue-operation" | "summary" | "file-history-snapshot")
    ) || value.get("attachment").is_some()
        || value.get("isMeta").and_then(Value::as_bool) == Some(true)
        || value.get("isSynthetic").and_then(Value::as_bool) == Some(true)
}

fn is_user_noise(text: &str) -> bool {
    let text = text.trim_start();
    [
        "<command-message>",
        "<command-name>",
        "<local-command-stdout>",
        "Caveat: The messages below",
    ]
    .iter()
    .any(|prefix| text.starts_with(prefix))
}

fn content_text(value: Option<&Value>) -> String {
    match value {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(blocks)) => blocks
            .iter()
            .filter_map(|block| block.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        Some(value) => value.to_string(),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::*;

    #[test]
    fn tail_reader_maps_only_new_complete_lines() {
        let path = std::env::temp_dir().join(format!(
            "tcode-subagent-tail-{}-{}.jsonl",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let mut file = File::create(&path).unwrap();
        writeln!(file, r#"{{"type":"user","message":{{"content":"do it"}}}}"#).unwrap();
        file.flush().unwrap();

        let mut tail = TailReader::new(path.clone(), "spawn-1");
        let first = tail.read_appended().unwrap();
        assert_eq!(first.len(), 1);
        assert!(matches!(
            &first[0],
            AgentEvent::ItemCompleted(ThreadItem { parent_item_id: Some(parent), content: ItemContent::UserMessage { text, .. }, .. })
                if parent == "spawn-1" && text == "do it"
        ));

        write!(file, r#"{{"type":"assistant","message":{{"id":"m1","content":[{{"type":"text","text":"working"}}]}}}}"#).unwrap();
        file.flush().unwrap();
        assert!(
            tail.read_appended().unwrap().is_empty(),
            "partial record must wait for LF"
        );
        writeln!(file).unwrap();
        file.flush().unwrap();
        let second = tail.read_appended().unwrap();
        assert_eq!(second.len(), 1);
        assert!(matches!(
            &second[0],
            AgentEvent::ItemCompleted(ThreadItem { id, content: ItemContent::AssistantMessage { text }, .. })
                if id == "spawn-1:m1:0" && text == "working"
        ));
        assert!(tail.read_appended().unwrap().is_empty());
        let _ = std::fs::remove_file(path);
    }

    /// Claude's Read of an image yields the image block and then a synthetic
    /// user text with the display scale; the child shows the image on its
    /// tool call and nothing as a user message.
    #[test]
    fn image_read_shows_the_image_and_not_the_synthetic_scale_note() {
        let mut mapper = TranscriptMapper::new("spawn-1");
        mapper.map_value(&serde_json::json!({"type":"assistant","message":{"id":"m1","content":[{"type":"tool_use","id":"read","name":"Read","input":{"file_path":"/tmp/shot.png"}}]}}));
        let completed = mapper.map_value(&serde_json::json!({"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"read","content":[{"type":"image","source":{"type":"base64","media_type":"image/png","data":"AQID"}}]}]}}));
        assert!(
            matches!(
                &completed[..],
                [AgentEvent::ItemCompleted(ThreadItem { content: ItemContent::ToolCall { image_reads, .. }, .. })]
                    if image_reads.len() == 1 && image_reads[0].media_type == "image/png"
            ),
            "{completed:?}"
        );
        let note = mapper.map_value(&serde_json::json!({"type":"user","isSynthetic":true,"message":{"content":[{"type":"text","text":"[Image: original 2400x1602, displayed at 2000x1335. Multiply coordinates by 1.20 to map to original image.]"}]}}));
        assert!(note.is_empty(), "{note:?}");
    }
}
