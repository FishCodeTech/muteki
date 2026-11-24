//! Adapt the committed Muteki event stream to the pinned upstream control index.
use crate::{
    AppState, chat, control_content,
    control_index::{EventLog, IndexedLog},
};
use serde_json::{Value, json};
use tcode_control_agent::{AgentEvent, ItemContent, ItemStatus, ThreadItem};
use tcode_control_core::session::{MessageOrigin, StoredEvent};

fn timestamp(value: &Value) -> Option<u64> {
    chrono::DateTime::parse_from_rfc3339(value.as_str()?)
        .ok()
        .and_then(|date| u64::try_from(date.timestamp_millis()).ok())
}
fn push(
    log: &mut EventLog,
    row: u64,
    ts: Option<u64>,
    event: AgentEvent,
    origin: Option<MessageOrigin>,
) {
    log.rows.push(row);
    log.records.push(StoredEvent {
        origin,
        author: None,
        ts,
        event,
        elided: None,
    });
    log.next_row = log.next_row.max(row + 1);
}

pub fn records(s: &AppState, id: &str) -> Result<(Value, EventLog, Vec<String>), Value> {
    records_with_checkpoint(s, id, &|| Ok(()))
}

pub(crate) fn records_with_checkpoint(
    s: &AppState,
    id: &str,
    checkpoint: &impl Fn() -> Result<(), Value>,
) -> Result<(Value, EventLog, Vec<String>), Value> {
    checkpoint()?;
    let doc = chat::load(s, id).map_err(|(_, body)| body.0["error"].clone())?;
    let events = control_content::all_events_with_checkpoint(s, id, checkpoint)?;
    let mut log = EventLog {
        records: Vec::new(),
        rows: Vec::new(),
        next_row: 0,
    };
    let mut notes = Vec::new();
    let native_turns: std::collections::HashSet<&str> = events
        .iter()
        .filter(|row| row["event_type"] == "core.agent.event")
        .filter_map(|row| row["payload"]["turn_id"].as_str())
        .collect();
    let wholly_native = !native_turns.is_empty()
        && doc["turns"].as_array().into_iter().flatten().all(|turn| {
            turn["turn_id"]
                .as_str()
                .is_some_and(|id| native_turns.contains(id))
        });
    if wholly_native {
        for row in &events {
            checkpoint()?;
            let position = row["stream_seq"].as_u64().ok_or_else(||json!({"code":"control.log_invalid","message":"Event has no stable stream position"}))?;
            let ts = timestamp(&row["occurred_at"]);
            if matches!(row["event_type"].as_str(), Some("core.turn.started" | "core.turn.steered")) {
                let turn = &row["payload"]["turn_id"];
                if let Some(message) = doc["messages"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .find(|message| message["role"] == "user" && &message["turn_id"] == turn
                        && if row["event_type"] == "core.turn.steered" {
                            message["message_id"] == format!("msg-steer-{}", row["payload"]["client_message_id"].as_str().unwrap_or(""))
                        } else { message["kind"] != "steer" })
                {
                    let text = message["text"].as_str().unwrap_or("").to_owned();
                    let item = ThreadItem {
                        id: message["message_id"].as_str().unwrap_or("").to_owned(),
                        parent_item_id: None,
                        content: ItemContent::UserMessage {
                            text,
                            attachments: message["attachments"]
                                .as_array()
                                .into_iter()
                                .flatten()
                                .filter_map(Value::as_str)
                                .map(str::to_owned)
                                .collect(),
                        },
                    };
                    let origin = match message["origin"].as_str() {
                        Some("agent") => MessageOrigin::Agent,
                        Some("server") => MessageOrigin::Server,
                        Some("human") => MessageOrigin::Human,
                        _ => MessageOrigin::legacy(
                            doc["thread"]["state"]["lineage"]["parent_thread_id"].is_string(),
                        ),
                    };
                    push(
                        &mut log,
                        position,
                        ts,
                        AgentEvent::ItemCompleted(item),
                        Some(origin),
                    );
                }
            } else if row["event_type"] == "core.agent.event" {
                let mut raw = &row["payload"]["event"];
                while raw["type"] == "origin" && !raw["agent_id"].is_string() {
                    raw = &raw["event"];
                }
                if matches!(
                    raw["type"].as_str(),
                    Some(
                        "process_started"
                            | "runtime_capabilities"
                            | "control_delivery"
                            | "turn_accepted"
                            | "usage_detail"
                            | "runtime_operation_result"
                            | "origin"
                    )
                ) {
                    continue;
                }
                let mapped = match raw["type"].as_str() {
                    Some("turn_failed") => {
                        json!({"type":"provider_start_failed","error":raw["message"]})
                    }
                    Some("continuation_started") => {
                        json!({"type":"turn_started","turn_id":raw["turn_id"]})
                    }
                    Some("rewind_failed") if raw["failure"].is_null() => {
                        let mut event = raw.clone();
                        event["failure"] = json!({"reason":"other","detail":raw["error"]});
                        event
                    }
                    _ => raw.clone(),
                };
                let event = match serde_json::from_value::<AgentEvent>(mapped) {
                    Ok(event) => event,
                    Err(error) => {
                        notes.push(format!("record {position}: {error}"));
                        AgentEvent::UnreadableRecord {
                            line: raw.to_string(),
                        }
                    }
                };
                if !doc["thread"]["state"]["lineage"]["native_subagent"].is_string() && matches!(
                    &event,
                    AgentEvent::ItemCompleted(ThreadItem {
                        content: ItemContent::UserMessage { .. },
                        ..
                    }) | AgentEvent::ItemStarted(ThreadItem {
                        content: ItemContent::UserMessage { .. },
                        ..
                    }) | AgentEvent::ItemUpdated(ThreadItem {
                        content: ItemContent::UserMessage { .. },
                        ..
                    })
                ) {
                    continue;
                }
                push(&mut log, position, ts, event, None);
            }
            log.next_row = log.next_row.max(position + 1);
        }
    } else {
        let items = control_content::index_records(s, id, &doc, &events, checkpoint)?;
        notes.push("Legacy Muteki transcript: entries retain their stored text; native-only event metadata is unavailable.".into());
        for item in items {
            checkpoint()?;
            let content = match item.kind.as_str() {
                "user_message" => ItemContent::UserMessage {
                    text: item.content,
                    attachments: Vec::new(),
                },
                "assistant_message" => ItemContent::AssistantMessage {
                    text: item.content,
                    summarized: false,
                },
                "reasoning" => ItemContent::Reasoning { text: item.content },
                _ => ItemContent::ToolCall {
                    image_reads: Vec::new(),
                    name: item.title,
                    input: Value::Null,
                    output: Some(item.content),
                    status: match item.status.as_str() {
                        "failed" => ItemStatus::Failed,
                        "running" | "pending" => ItemStatus::InProgress,
                        "cancelled" => ItemStatus::Interrupted,
                        _ => ItemStatus::Completed,
                    },
                    family: None,
                },
            };
            push(
                &mut log,
                item.seq as u64,
                timestamp(&json!(item.created_at)),
                AgentEvent::ItemCompleted(ThreadItem {
                    id: format!("legacy:{}", item.seq),
                    parent_item_id: None,
                    content,
                }),
                None,
            );
        }
    }
    Ok((doc, log, notes))
}

pub fn load(s: &AppState, id: &str) -> Result<(Value, IndexedLog, Vec<String>), Value> {
    let (doc, log, notes) = records(s, id)?;
    Ok((doc, IndexedLog::build(&log), notes))
}
