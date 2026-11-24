use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;

use agent::{ItemContent, ItemStatus, ThreadItem};
use serde_json::Value;

use super::{ConvertedEntry, ConvertedThread, file_modified_ms, timestamp_ms};

pub(super) fn convert(path: &Path, external_id: &str) -> Result<Option<ConvertedThread>, String> {
    let file =
        File::open(path).map_err(|err| format!("failed to open {}: {err}", path.display()))?;
    let fallback_ts = file_modified_ms(path);
    let mut entries = Vec::new();
    let mut pending: HashMap<String, usize> = HashMap::new();
    let mut parsed_lines = 0_usize;
    let mut first_ts = None;
    let mut last_ts = None;
    let mut first_user = None;
    let mut next_id = 0_u64;

    for line in BufReader::new(file).lines() {
        let Ok(line) = line else { continue };
        let Ok(value) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        parsed_lines += 1;
        let ts = value
            .get("timestamp")
            .and_then(timestamp_ms)
            .unwrap_or(fallback_ts);
        first_ts.get_or_insert(ts);
        last_ts = Some(ts);
        if should_skip(&value) {
            continue;
        }
        match value.get("type").and_then(Value::as_str) {
            Some("user") => {
                let Some(content) = value.pointer("/message/content") else {
                    continue;
                };
                for block in content_blocks(content) {
                    match block {
                        UserBlock::Text(text) if !is_user_noise(&text) => {
                            if first_user.is_none() {
                                first_user = Some(text.clone());
                            }
                            next_id += 1;
                            entries.push(ConvertedEntry::Item {
                                ts,
                                item: ThreadItem {
                                    id: format!("imported-user-{next_id}"),
                                    parent_item_id: None,
                                    content: ItemContent::UserMessage {
                                        text,
                                        context_len: None,
                                        attachments: Vec::new(),
                                    },
                                },
                            });
                        }
                        UserBlock::ToolResult {
                            id,
                            output,
                            failed,
                            content,
                        } => {
                            let Some(index) = pending.remove(&id) else {
                                continue;
                            };
                            if let Some(ConvertedEntry::Item { item, .. }) = entries.get_mut(index)
                                && let ItemContent::ToolCall {
                                    output: item_output,
                                    status,
                                    name,
                                    image_reads,
                                    ..
                                } = &mut item.content
                            {
                                if !failed {
                                    *image_reads = agent::tool_result_images(name, &content);
                                }
                                *item_output = Some(output);
                                *status = if failed {
                                    ItemStatus::Failed
                                } else {
                                    ItemStatus::Completed
                                };
                            }
                        }
                        UserBlock::Text(_) => {}
                    }
                }
            }
            Some("assistant") => {
                let Some(blocks) = value.pointer("/message/content").and_then(Value::as_array)
                else {
                    continue;
                };
                for block in blocks {
                    let Some(kind) = block.get("type").and_then(Value::as_str) else {
                        continue;
                    };
                    let content = match kind {
                        "text" => block.get("text").and_then(Value::as_str).map(|text| {
                            ItemContent::AssistantMessage {
                                text: text.to_string(),
                            }
                        }),
                        "thinking" => block.get("thinking").and_then(Value::as_str).map(|text| {
                            ItemContent::Reasoning {
                                text: text.to_string(),
                            }
                        }),
                        "tool_use" => {
                            let id = block.get("id").and_then(Value::as_str).unwrap_or_default();
                            let name = block.get("name").and_then(Value::as_str).unwrap_or("tool");
                            let input = block.get("input").cloned().unwrap_or(Value::Null);
                            Some(ItemContent::ToolCall {
                                presentation: None,
                                image_reads: Vec::new(),
                                name: name.to_string(),
                                input,
                                output: None,
                                status: ItemStatus::InProgress,
                            })
                            .inspect(|_| {
                                if !id.is_empty() {
                                    pending.insert(id.to_string(), entries.len());
                                }
                            })
                        }
                        _ => None,
                    };
                    if let Some(content) = content {
                        next_id += 1;
                        let id = block
                            .get("id")
                            .and_then(Value::as_str)
                            .map(str::to_string)
                            .unwrap_or_else(|| format!("imported-assistant-{next_id}"));
                        entries.push(ConvertedEntry::Item {
                            ts,
                            item: ThreadItem {
                                id,
                                parent_item_id: None,
                                content,
                            },
                        });
                    }
                }
            }
            Some("system")
                if value.get("subtype").and_then(Value::as_str) == Some("compact_boundary") =>
            {
                entries.push(ConvertedEntry::Compacted { ts });
            }
            _ => {}
        }
    }
    if parsed_lines == 0 {
        return Err(format!(
            "{} contains no parseable JSON lines",
            path.display()
        ));
    }
    for index in pending.into_values() {
        if let Some(ConvertedEntry::Item { item, .. }) = entries.get_mut(index)
            && let ItemContent::ToolCall { status, .. } = &mut item.content
        {
            *status = ItemStatus::Completed;
        }
    }
    let created_ms = first_ts.unwrap_or(fallback_ts);
    Ok(Some(ConvertedThread {
        external_id: external_id.to_string(),
        created_ms,
        updated_ms: last_ts.unwrap_or(created_ms),
        first_user,
        entries,
    }))
}

fn should_skip(value: &Value) -> bool {
    matches!(
        value.get("type").and_then(Value::as_str),
        Some("queue-operation" | "summary" | "file-history-snapshot")
    ) || value.get("attachment").is_some()
        || value.get("isSidechain").and_then(Value::as_bool) == Some(true)
        || value.get("isMeta").and_then(Value::as_bool) == Some(true)
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

enum UserBlock {
    Text(String),
    ToolResult {
        id: String,
        output: String,
        failed: bool,
        content: Value,
    },
}

fn content_blocks(content: &Value) -> Vec<UserBlock> {
    if let Some(text) = content.as_str() {
        return vec![UserBlock::Text(text.to_string())];
    }
    content
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|block| match block.get("type").and_then(Value::as_str) {
            Some("text") => block
                .get("text")
                .and_then(Value::as_str)
                .map(|text| UserBlock::Text(text.to_string())),
            Some("tool_result") => Some(UserBlock::ToolResult {
                content: block.get("content").cloned().unwrap_or(Value::Null),
                id: block
                    .get("tool_use_id")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                output: readable_content(block.get("content").unwrap_or(&Value::Null)),
                failed: block
                    .get("is_error")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
            }),
            _ => None,
        })
        .collect()
}

fn readable_content(value: &Value) -> String {
    if let Some(text) = value.as_str() {
        return text.to_string();
    }
    value
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|block| block.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("\n")
}
