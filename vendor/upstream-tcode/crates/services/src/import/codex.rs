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
    let mut lines = BufReader::new(file).lines();
    let first = lines
        .next()
        .ok_or_else(|| format!("{} is empty", path.display()))?
        .map_err(|err| format!("failed to read {}: {err}", path.display()))?;
    let meta: Value = serde_json::from_str(&first).map_err(|err| {
        format!(
            "invalid Codex session metadata in {}: {err}",
            path.display()
        )
    })?;
    if meta.get("type").and_then(Value::as_str) != Some("session_meta") {
        return Err(format!(
            "{} does not start with session_meta",
            path.display()
        ));
    }
    if meta.pointer("/payload/originator").and_then(Value::as_str) == Some("tcode") {
        return Ok(None);
    }

    let first_meta_ts = meta
        .get("timestamp")
        .and_then(timestamp_ms)
        .unwrap_or(fallback_ts);
    let created_ms = first_meta_ts;
    let mut updated_ms = first_meta_ts;
    let mut first_user = None;
    let mut entries = Vec::new();
    let mut pending = HashMap::<String, usize>::new();
    let mut next_id = 0_u64;

    for line in lines {
        let Ok(line) = line else { continue };
        let Ok(value) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let ts = value
            .get("timestamp")
            .and_then(timestamp_ms)
            .unwrap_or(updated_ms);
        updated_ms = ts;
        match value.get("type").and_then(Value::as_str) {
            Some("response_item") => {
                let Some(payload) = value.get("payload") else {
                    continue;
                };
                let Some(kind) = payload.get("type").and_then(Value::as_str) else {
                    continue;
                };
                match kind {
                    "message" => {
                        let role = payload.get("role").and_then(Value::as_str);
                        if role == Some("developer") {
                            continue;
                        }
                        let block_kind = if role == Some("user") {
                            "input_text"
                        } else {
                            "output_text"
                        };
                        for text in message_texts(payload, block_kind) {
                            if role == Some("user") && is_harness_text(&text) {
                                continue;
                            }
                            let content = if role == Some("user") {
                                first_user.get_or_insert_with(|| text.clone());
                                ItemContent::UserMessage {
                                    text,
                                    context_len: None,
                                    attachments: Vec::new(),
                                }
                            } else if role == Some("assistant") {
                                ItemContent::AssistantMessage { text }
                            } else {
                                continue;
                            };
                            push_item(&mut entries, ts, &mut next_id, content, None);
                        }
                    }
                    "agent_message" => {
                        if let Some(text) = payload
                            .get("message")
                            .and_then(Value::as_str)
                            .or_else(|| payload.get("text").and_then(Value::as_str))
                        {
                            push_item(
                                &mut entries,
                                ts,
                                &mut next_id,
                                ItemContent::AssistantMessage {
                                    text: text.to_string(),
                                },
                                None,
                            );
                        }
                    }
                    "reasoning" => {
                        let text = payload
                            .get("summary")
                            .and_then(Value::as_array)
                            .into_iter()
                            .flatten()
                            .filter_map(|part| part.get("text").and_then(Value::as_str))
                            .collect::<Vec<_>>()
                            .join("\n");
                        if !text.is_empty() {
                            push_item(
                                &mut entries,
                                ts,
                                &mut next_id,
                                ItemContent::Reasoning { text },
                                None,
                            );
                        }
                    }
                    "function_call" | "custom_tool_call" => {
                        let call_id = payload
                            .get("call_id")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string();
                        let arguments = payload
                            .get("arguments")
                            .or_else(|| payload.get("input"))
                            .cloned()
                            .unwrap_or(Value::Null);
                        let input = arguments
                            .as_str()
                            .and_then(|text| serde_json::from_str(text).ok())
                            .unwrap_or(arguments);
                        let content = ItemContent::ToolCall {
                            presentation: None,
                            image_reads: Vec::new(),
                            name: payload
                                .get("name")
                                .and_then(Value::as_str)
                                .unwrap_or("tool")
                                .to_string(),
                            input,
                            output: None,
                            status: ItemStatus::InProgress,
                        };
                        let index = entries.len();
                        push_item(&mut entries, ts, &mut next_id, content, Some(&call_id));
                        if !call_id.is_empty() {
                            pending.insert(call_id, index);
                        }
                    }
                    "function_call_output" | "custom_tool_call_output" => {
                        let Some(call_id) = payload.get("call_id").and_then(Value::as_str) else {
                            continue;
                        };
                        let Some(index) = pending.remove(call_id) else {
                            continue;
                        };
                        if let Some(ConvertedEntry::Item { item, .. }) = entries.get_mut(index)
                            && let ItemContent::ToolCall {
                                name,
                                image_reads,
                                output,
                                status,
                                ..
                            } = &mut item.content
                        {
                            let raw = payload.get("output").unwrap_or(&Value::Null);
                            let parsed = raw
                                .as_str()
                                .and_then(|text| serde_json::from_str::<Value>(text).ok());
                            let result = parsed.as_ref().unwrap_or(raw);
                            if result.get("isError").and_then(Value::as_bool) != Some(true) {
                                *image_reads = agent::tool_result_images(
                                    name,
                                    result.get("content").unwrap_or(result),
                                );
                            }
                            *output = Some(readable_output(
                                payload.get("output").unwrap_or(&Value::Null),
                            ));
                            *status = ItemStatus::Completed;
                        }
                    }
                    "local_shell_call" => {
                        let command = payload
                            .get("command")
                            .map(readable_output)
                            .unwrap_or_default();
                        push_item(
                            &mut entries,
                            ts,
                            &mut next_id,
                            ItemContent::CommandExecution {
                                command,
                                output: payload
                                    .get("output")
                                    .map(readable_output)
                                    .unwrap_or_default(),
                                exit_code: payload
                                    .get("exit_code")
                                    .and_then(Value::as_i64)
                                    .and_then(|code| i32::try_from(code).ok()),
                                status: ItemStatus::Completed,
                            },
                            payload.get("id").and_then(Value::as_str),
                        );
                    }
                    _ => {}
                }
            }
            Some("event_msg")
                if value.pointer("/payload/type").and_then(Value::as_str)
                    == Some("context_compacted") =>
            {
                entries.push(ConvertedEntry::Compacted { ts });
            }
            _ => {}
        }
    }
    for index in pending.into_values() {
        if let Some(ConvertedEntry::Item { item, .. }) = entries.get_mut(index)
            && let ItemContent::ToolCall { status, .. } = &mut item.content
        {
            *status = ItemStatus::Completed;
        }
    }
    Ok(Some(ConvertedThread {
        external_id: external_id.to_string(),
        created_ms,
        updated_ms,
        first_user,
        entries,
    }))
}

fn push_item(
    entries: &mut Vec<ConvertedEntry>,
    ts: u64,
    next_id: &mut u64,
    content: ItemContent,
    id: Option<&str>,
) {
    *next_id += 1;
    entries.push(ConvertedEntry::Item {
        ts,
        item: ThreadItem {
            id: id
                .filter(|id| !id.is_empty())
                .map(str::to_string)
                .unwrap_or_else(|| format!("imported-codex-{next_id}")),
            parent_item_id: None,
            content,
        },
    });
}

fn message_texts(payload: &Value, kind: &str) -> Vec<String> {
    payload
        .get("content")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|block| block.get("type").and_then(Value::as_str) == Some(kind))
        .filter_map(|block| {
            block
                .get("text")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .collect()
}

fn is_harness_text(text: &str) -> bool {
    let text = text.trim_start();
    text.starts_with('<')
        && ["<environment_context", "<user_instructions", "<permissions"]
            .iter()
            .any(|prefix| text.starts_with(prefix))
}

fn readable_output(value: &Value) -> String {
    if let Some(text) = value.as_str() {
        if let Ok(nested) = serde_json::from_str::<Value>(text) {
            return readable_output(&nested);
        }
        return text.to_string();
    }
    if let Some(text) = value.get("output").and_then(Value::as_str) {
        return text.to_string();
    }
    serde_json::to_string_pretty(value).unwrap_or_default()
}
