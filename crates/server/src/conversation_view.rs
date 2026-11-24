//! Public projection of current conversations and imported history.
use serde_json::{Value, json};

pub fn select(value: &Value, keys: &[&str]) -> Value {
    let mut result = serde_json::Map::new();
    for key in keys {
        if let Some(value) = value.get(*key) {
            result.insert((*key).to_owned(), value.clone());
        }
    }
    Value::Object(result)
}

pub fn runtime(value: &Value) -> Value {
    select(
        value,
        &[
            "adapter_id",
            "instance_id",
            "credential_id",
            "credential_ref",
            "model",
            "effort",
            "reasoning_effort",
            "service_tier",
            "access_mode",
            "interaction_mode",
            "options",
            "permission_mode",
            "sandbox_mode",
            "updated_at",
        ],
    )
}

/// Page the public current-branch projection without changing the stored history.
pub fn page(
    snapshot: &mut Value,
    limit: usize,
    before: Option<i64>,
    after: Option<i64>,
    keep_running: bool,
) {
    let mut all = snapshot["messages"].as_array().cloned().unwrap_or_default();
    all.sort_by_key(|message| message["stream_seq"].as_i64().unwrap_or(0));
    let mut messages: Vec<_> = all
        .iter()
        .filter(|message| {
            let seq = message["stream_seq"].as_i64().unwrap_or(0);
            before.is_none_or(|cursor| seq < cursor) && after.is_none_or(|cursor| seq > cursor)
        })
        .cloned()
        .collect();
    if after.is_some() {
        messages.truncate(limit);
    } else if messages.len() > limit {
        messages.drain(..messages.len() - limit);
    }
    let oldest = messages.first().cloned().unwrap_or(Value::Null);
    let newest = messages.last().cloned().unwrap_or(Value::Null);
    let has_before = oldest["stream_seq"]
        .as_i64()
        .map_or(after.is_some(), |seq| {
            all.iter().any(|message| {
                message["stream_seq"]
                    .as_i64()
                    .is_some_and(|candidate| candidate < seq)
            })
        });
    let has_after = if before.is_some() || after.is_some() {
        newest["stream_seq"]
            .as_i64()
            .map_or(before.is_some(), |seq| {
                all.iter().any(|message| {
                    message["stream_seq"]
                        .as_i64()
                        .is_some_and(|candidate| candidate > seq)
                })
            })
    } else {
        false
    };
    let ids: std::collections::HashSet<_> = messages
        .iter()
        .filter_map(|message| message["turn_id"].as_str())
        .collect();
    let turns = snapshot["turns"].as_array().cloned().unwrap_or_default();
    let mut visible: Vec<_> = turns
        .iter()
        .filter(|turn| {
            turn["turn_id"].as_str().is_some_and(|id| ids.contains(id))
                || (keep_running
                    && !snapshot["state"]["running_turn_id"].is_null()
                    && turn["turn_id"] == snapshot["state"]["running_turn_id"])
        })
        .cloned()
        .collect();
    if keep_running && visible.is_empty() {
        visible = turns.into_iter().rev().take(limit).collect();
        visible.reverse();
    }
    snapshot["turns"] = json!(visible);
    snapshot["messages"] = json!(messages);
    snapshot["messages_page"] = json!({"limit":limit,"has_more_before":has_before,"has_more_after":has_after,
        "oldest_stream_seq":oldest["stream_seq"],"newest_stream_seq":newest["stream_seq"],
        "oldest_message_id":oldest["message_id"],"newest_message_id":newest["message_id"]});
}

fn statistics<'a>(turns: impl Iterator<Item = &'a Value>) -> Value {
    fn millis(value: &Value) -> Option<i64> {
        chrono::DateTime::parse_from_rfc3339(value.as_str()?).ok().map(|t| t.timestamp_millis())
    }
    fn elapsed(start: &Value, end: &Value) -> Option<i64> {
        Some((millis(end)? - millis(start)?).max(0))
    }
    let turns: Vec<_> = turns.map(|turn| {
        let mut turn = select(turn, &["usage", "started_at", "completed_at", "first_token_at"]);
        let mut usage = json!({});
        crate::usage_history::capture(&mut usage, &turn["usage"]);
        for key in ["input_tokens", "output_tokens", "cache_read_tokens", "reasoning_tokens"] {
            if !usage[key].is_null() { turn["usage"][key] = usage[key].clone(); }
        }
        turn
    }).collect();
    let mut result = json!({"turn_count":turns.len(),"step_count":0});
    for key in ["input_tokens", "output_tokens", "cache_read_tokens", "reasoning_tokens"] {
        let values: Vec<_> = turns.iter().filter_map(|t| t["usage"][key].as_f64()).collect();
        if !values.is_empty() { result[key] = json!(values.iter().sum::<f64>()); }
    }
    result["token_coverage"] = json!(if result["input_tokens"].is_null() && result["output_tokens"].is_null() {
        "missing"
    } else if turns.iter().all(|t| t["usage"]["input_tokens"].is_number() && t["usage"]["output_tokens"].is_number()) {
        "complete"
    } else { "partial" });
    let durations: Vec<_> = turns.iter().filter_map(|t| elapsed(&t["started_at"], &t["completed_at"])).collect();
    if !durations.is_empty() { result["total_duration_ms"] = json!(durations.iter().sum::<i64>()); }
    let ttfts: Vec<_> = turns.iter().filter_map(|t| elapsed(&t["started_at"], &t["first_token_at"])).collect();
    if !ttfts.is_empty() { result["average_ttft_ms"] = json!(ttfts.iter().sum::<i64>() / ttfts.len() as i64); }
    if let (Some(input), Some(cache)) = (result["input_tokens"].as_f64(), result["cache_read_tokens"].as_f64()) {
        if input > 0.0 { result["cache_hit_rate"] = json!(cache / input); }
    }
    if let Some(turn) = turns.last() {
        let mut latest = turn["usage"].as_object().cloned().unwrap_or_default();
        if let Some(duration) = elapsed(&turn["started_at"], &turn["completed_at"]) {
            latest.insert("total_duration_ms".into(), json!(duration));
            if duration > 0 && let Some(output) = turn["usage"]["output_tokens"].as_f64() {
                latest.insert("end_to_end_tokens_per_second".into(), json!(output * 1000.0 / duration as f64));
            }
        }
        if let Some(ttft) = elapsed(&turn["started_at"], &turn["first_token_at"]) { latest.insert("ttft_ms".into(), json!(ttft)); }
        result["latest_turn"] = json!(latest);
    }
    result
}

pub fn snapshot(value: &Value) -> Value {
    project(value, None)
}

pub fn timeline(value: &Value, limit: usize) -> Value {
    project(value, Some(limit))
}

fn project(value: &Value, limit: Option<usize>) -> Value {
    let raw_turns = value["turns"].as_array().map(Vec::as_slice).unwrap_or(&[]);
    let superseded_ids: std::collections::HashSet<_> = raw_turns.iter()
        .filter(|turn| turn["status"] == "superseded")
        .filter_map(|turn| turn["turn_id"].as_str())
        .collect();
    let superseded_message = |row: &Value| {
        row["turn_id"].as_str().is_some_and(|id| superseded_ids.contains(id))
    };
    let rows = value["messages"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    let mut projected = crate::chat_core::fold_messages(rows);
    projected.extend(
        rows.iter()
            .filter(|row| {
                matches!(
                    row["kind"].as_str(),
                    Some("imported_tool" | "imported_record")
                ) && !matches!(row["role"].as_str(), Some("user" | "assistant"))
            })
            .cloned(),
    );
    projected.sort_by_key(|row| row["stream_seq"].as_i64().unwrap_or(0));
    let has_more_before = if let Some(limit) = limit {
        projected.retain(|message| !superseded_message(message));
        let skip = projected.len().saturating_sub(limit);
        projected.drain(..skip);
        skip > 0
    } else {
        false
    };
    let messages: Vec<_> = projected
        .iter()
        .map(|m| {
            select(
                m,
                &[
                    "message_id",
                    "turn_id",
                    "role",
                    "source_provider",
                    "source_role",
                    "source_content",
                    "kind",
                    "text",
                    "attachments",
                    "capability_refs",
                    "status",
                    "phase",
                    "stream_seq",
                    "created_at",
                    "historical",
                    "source_message_id",
                    "source_thread_id",
                ],
            )
        })
        .collect();
    let message_turn_ids: std::collections::HashSet<_> = messages.iter()
        .filter_map(|row| row["turn_id"].as_str()).collect();
    let running_turn = value["thread"]["state"]["running_turn_id"].as_str();
    let visible = |turn: &&Value| {
        let id = turn["turn_id"].as_str();
        turn["status"] != "superseded" && id.is_some_and(|id| {
            message_turn_ids.contains(id) || Some(id) == running_turn
        })
    };
    let turns: Vec<_> = match limit {
        None => raw_turns.iter().collect(),
        Some(limit) => {
            let mut visible: Vec<_> = raw_turns.iter().filter(visible).collect();
            if visible.is_empty() {
                visible = raw_turns.iter().rev()
                    .filter(|turn| turn["status"] != "superseded")
                    .take(limit).collect();
                visible.reverse();
            }
            visible
        }
    };
    let summary = statistics(raw_turns.iter().filter(|turn| turn["status"] != "superseded"));
    let turns: Vec<_> = turns.into_iter().map(|turn| {
            let mut result = select(
                turn,
                &[
                    "turn_id",
                    "seq",
                    "kind",
                    "status",
                    "original_status",
                    "text",
                    "attachments",
                    "interaction_mode",
                    "process_recorded",
                    "thread_disposition",
                    "retry_of_turn_id",
                    "origin_turn_id",
                    "source_turn_id",
                    "source_thread_id",
                    "superseded_by_turn_id",
                    "error",
                    "usage",
                    "capability_refs",
                    "created_at",
                    "started_at",
                    "first_token_at",
                    "completed_at",
                    "historical",
                ],
            );
            if let Some(delivery) = turn.get("context_delivery") {
                if limit.is_none() {
                    result["context_delivery"] = delivery.clone();
                } else if delivery.as_array().is_some_and(|parts| !parts.is_empty()) {
                    result["has_context_delivery"] = json!(true);
                }
            }
            if turn["runtime_snapshot"].is_object() {
                result["runtime_snapshot"] = runtime(&turn["runtime_snapshot"]);
            }
            result
        })
        .collect();
    let (superseded_turns, turns): (Vec<_>, Vec<_>) =
        turns.into_iter().partition(|t| t["status"] == "superseded");
    let (superseded_messages, messages): (Vec<_>, Vec<_>) = messages.into_iter().partition(superseded_message);
    let artifacts: Vec<_> = value["artifacts"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or(&[])
        .iter()
        .filter(|a| {
            !a["turn_id"]
                .as_str()
                .is_some_and(|id| superseded_ids.contains(id))
        })
        .map(|a| {
            let mut item = select(
                a,
                &[
                    "artifact_id",
                    "sha256",
                    "name",
                    "kind",
                    "media_type",
                    "size",
                    "turn_id",
                    "created_at",
                    "event_id",
                    "stream_seq",
                    "historical",
                ],
            );
            if item["artifact_id"].as_str().is_none() && item["sha256"].is_string() {
                item["needs_reselect"] = json!(true);
            }
            item
        })
        .collect();
    let tools: Vec<_> = value["tools"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or(&[])
        .iter()
        .filter(|t| {
            let id = t["turn_id"].as_str();
            if limit.is_some() {
                id.is_some() && id == running_turn && t["status"] == "running"
            } else {
                id.is_none_or(|id| !superseded_ids.contains(id))
            }
        })
        .map(|t| {
            if limit.is_some() {
                return select(t, &["tool_id", "turn_id", "name", "status"]);
            }
            select(
                t,
                &[
                    "tool_id",
                    "turn_id",
                    "name",
                    "status",
                    "output_summary",
                    "output_complete",
                    "stream_seq",
                    "historical",
                ],
            )
        })
        .collect();
    let queue: Vec<_> = value["queue"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|item| {
            let mut row = select(
                item,
                &[
                    "queue_id",
                    "thread_id",
                    "client_message_id",
                    "not_before",
                    "origin",
                    "text",
                    "attachments",
                    "capability_refs",
                    "position",
                    "status",
                    "error",
                    "created_at",
                    "updated_at",
                ],
            );
            row["runtime"] = runtime(&item["runtime"]);
            row
        })
        .collect();
    let mut thread = value["thread"].clone();
    let mut state = thread["state"].clone();
    state["queue_count"] = json!(queue.len());
    for (key, default) in [
        ("queue_paused", json!(false)),
        ("queue_pause_reason", json!("")),
        ("queue_revision", json!(0)),
        ("queue_failed_item_id", Value::Null),
    ] {
        if state.get(key).is_none() {
            state[key] = default;
        }
    }
    if let Some(object) = thread.as_object_mut() {
        object.remove("state");
    }
    let oldest = messages.first().cloned().unwrap_or(Value::Null);
    let newest = messages.last().cloned().unwrap_or(Value::Null);
    json!({
        "thread":thread,"state":state,"runtime":runtime(&value["runtime"]),
        "statistics":summary,
        "context_window":value["context_window"],"agent_session":value["agent_session"],
        "messages":messages,"turns":turns,"superseded_messages":superseded_messages,"superseded_turns":superseded_turns,"artifacts":artifacts,"tools":tools,"queue":queue,
        "messages_page":{"limit":limit.unwrap_or(messages.len()),"has_more_before":has_more_before,"has_more_after":false,
            "oldest_stream_seq":oldest["stream_seq"],"newest_stream_seq":newest["stream_seq"],
            "oldest_message_id":oldest["message_id"],"newest_message_id":newest["message_id"]},
        "watermark":messages.iter().filter_map(|m|m["stream_seq"].as_u64()).max().unwrap_or(0)
            .max(value["thread"]["state"]["head_stream_seq"].as_u64().unwrap_or(0))
            .max(value["event_seq"].as_u64().unwrap_or(0)),
        "event_watermark":value["event_watermark"].as_u64().unwrap_or(0)
    })
}
