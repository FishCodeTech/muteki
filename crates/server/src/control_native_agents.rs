//! Read-only threads for the native provider's delegated transcripts.
use crate::{AppState, chat};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::sync::Arc;
use agent::{AgentEvent, ItemContent};
use tcode_core::session::{EntryContent, StoredEvent, Timeline};

pub async fn mirror(s: &Arc<AppState>, parent: &str, event: &Value) -> Result<(), Value> {
    let mut raw = &event["payload"]["event"];
    while raw["type"] == "origin" && raw["agent_id"].is_null() {
        raw = &raw["event"];
    }
    let (agent, source) = if raw["type"] == "origin" {
        let Some(agent) = raw["agent_id"].as_str() else {
            return Ok(());
        };
        (agent.to_owned(), raw["event"].clone())
    } else if raw["type"] == "native_agent_update" {
        let Some(agent) = raw["patch"]["agent_id"].as_str() else { return Ok(()) };
        (agent.to_owned(), raw.clone())
    } else if let Some(parent) = raw["parent_item_id"].as_str() {
        (parent.to_owned(), raw.clone())
    } else if raw["content"]["kind"] == "subagent" {
        let Some(id) = raw["id"].as_str() else {
            return Ok(());
        };
        (id.to_owned(), raw.clone())
    } else {
        return Ok(());
    };
    let sequence = event["stream_seq"]
        .as_u64()
        .ok_or_else(|| json!({"message":"native event has no stream position"}))?;
    let mirror = format!("native-{:x}", Sha256::digest(format!("{parent}:{agent}")));
    let _guard = s.chat.active.lock().await;
    let mut lead = chat::load(s, parent).map_err(|(_, body)| body.0["error"].clone())?;
    if lead["thread"]["state"]["lineage"]["native_subagent"].is_string() {
        return Ok(());
    }
    let agent_row = lead["thread"]["state"]["agents"]["agents"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|row| row["agent_id"] == agent)
        .cloned()
        .unwrap_or(Value::Null);
    if agent_row.is_null() {
        return Ok(());
    }
    let owner = agent_row["parent_id"].as_str()
        .map(|agent| format!("native-{:x}", Sha256::digest(format!("{parent}:{agent}"))))
        .unwrap_or_else(|| parent.to_owned());
    let owner_depth = if owner == parent {
        lead["thread"]["state"]["lineage"]["depth"].as_u64().unwrap_or(0)
    } else {
        let owner_doc = chat::load(s, &owner).map_err(|(_, body)| body.0["error"].clone())?;
        owner_doc["thread"]["state"]["lineage"]["depth"].as_u64().unwrap_or(0)
    };
    let mut mirror_runtime = lead["runtime"].clone();
    mirror_runtime["model"] = agent_row["model"].clone();
    mirror_runtime["effort"] = agent_row["effort"].clone();
    mirror_runtime["reasoning_effort"] = agent_row["effort"].clone();
    mirror_runtime["options"] = json!({});
    if agent_row["adapter_id"].is_string() {
        mirror_runtime["adapter_id"] = agent_row["adapter_id"].clone();
    }
    let mut doc = if s
        .store
        .get(chat::COLLECTION, &mirror)
        .map_err(|e| json!({"message":e.to_string()}))?
        .is_some()
    {
        chat::load(s, &mirror).map_err(|(_, body)| body.0["error"].clone())?
    } else {
        let body = json!({"thread_id":mirror,"command_id":format!("mirror:{parent}:{agent}"),"title":agent_row["title"].as_str().or_else(||agent_row["name"].as_str()).unwrap_or(&agent),"project_id":lead["thread"]["project_id"],"workspace_id":lead["thread"]["workspace_id"],"runtime":mirror_runtime});
        let (_, mut doc, _) = chat::prepare_created_thread(s, &body, "conversation")
            .map_err(|(_, body)| body.0["error"].clone())?;
        doc["thread"]["state"]["lineage"] = json!({"origin":"runtime","native_subagent":agent,"parent_thread_id":owner,"root_thread_id":lead["thread"]["state"]["lineage"]["root_thread_id"].as_str().unwrap_or(parent),"depth":owner_depth+1,"read_only":true});
        doc["native_mirror_events"] = json!([]);
        doc
    };
    if doc["native_mirror_position"]
        .as_u64()
        .is_some_and(|seen| seen >= sequence)
    {
        return Ok(());
    }
    doc["runtime"] = mirror_runtime;
    let mut native = source;
    if native.get("parent_item_id").is_some() {
        native["parent_item_id"] = Value::Null;
    }
    let lifecycle = if native["content"]["kind"] == "subagent" {
        native["content"]["status"].as_str()
    } else if native["type"] == "native_agent_update" {
        native["patch"]["status"].as_str()
    } else { None };
    let mirror_running = doc["thread"]["state"]["running_turn_id"].is_string();
    let records = doc["native_mirror_events"].as_array_mut().unwrap();
    if matches!(lifecycle, Some("running" | "in_progress")) && !mirror_running {
        records.push(json!({"event":{"type":"turn_started","turn_id":agent},"ts":event["occurred_at"]}));
    }
    records.push(json!({"event":native,"ts":event["occurred_at"]}));
    {
        let status = match lifecycle {
            Some("completed") => Some("completed"),
            Some("failed" | "declined") => Some("failed"),
            Some("interrupted" | "cancelled") => Some("interrupted"),
            _ => None,
        };
        if let Some(status) = status {
            records.push(json!({"event":{"type":"turn_completed","turn_id":agent,"status":status,"usage":null},"ts":event["occurred_at"]}));
        }
    }
    doc["native_mirror_position"] = json!(sequence);
    let records = doc["native_mirror_events"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|row| {
            Some(StoredEvent {
                event: serde_json::from_value::<AgentEvent>(row["event"].clone()).unwrap_or_else(
                    |_| AgentEvent::UnreadableRecord {
                        line: row["event"].to_string(),
                    },
                ),
                ts: crate::control_state::millis(&row["ts"]),
                origin: None,
                author: None,
                elided: None,
            })
        })
        .collect::<Vec<_>>();
    let timeline = Timeline::fold_events(records);
    let at = event["occurred_at"].clone();
    let turns=timeline.turns.iter().enumerate().map(|(index,turn)|json!({"turn_id":format!("{mirror}:{index}"),"seq":index+1,"status":if turn.running{"running"}else{match turn.status{Some(agent::TurnStatus::Failed)=>"failed",Some(agent::TurnStatus::Interrupted)=>"interrupted",_=>"completed"}},"created_at":turn.start_ts.and_then(|ms|chrono::DateTime::from_timestamp_millis(ms as i64)).map(|at|at.to_rfc3339()),"runtime_turn_id":turn.provider_turn_id,"text":"","runtime_snapshot":doc["runtime"]})).collect::<Vec<_>>();
    let mut messages = Vec::new();
    let mut tools = Vec::new();
    for (index, entry) in timeline.entries.iter().enumerate() {
        let turn = format!("{mirror}:{}", entry.turn);
        let created = entry
            .ts
            .and_then(|ms| chrono::DateTime::from_timestamp_millis(ms as i64))
            .map(|at| at.to_rfc3339());
        if let EntryContent::Item(content) = &entry.content {
            match content{
            ItemContent::UserMessage{text,..}|ItemContent::AssistantMessage{text,..}|ItemContent::Reasoning{text}=>{
                let role=match content{ItemContent::UserMessage{..}=>"user",ItemContent::Reasoning{..}=>"reasoning",_=>"assistant"};
                messages.push(json!({"message_id":entry.id,"turn_id":turn,"role":role,"origin":"agent","text":text,"stream_seq":index+1,"created_at":created}));
            },
            _=>tools.push(json!({"tool_id":entry.id,"turn_id":turn,"name":serde_json::to_value(content).ok().and_then(|v|v["name"].as_str().map(str::to_owned)).unwrap_or_else(||"native".into()),"arguments":content,"status":match serde_json::to_value(content).ok().and_then(|v|v["status"].as_str().map(str::to_owned)).as_deref(){Some("in_progress")=>"running",Some("failed")=>"failed",Some("interrupted")=>"cancelled",_=>"completed"},"output_summary":serde_json::to_value(content).ok().and_then(|v|v["output"].as_str().map(str::to_owned)),"created_at":created})),
        }
        }
    }
    doc["messages"] = json!(messages);
    doc["turns"] = json!(turns);
    doc["tools"] = json!(tools);
    doc["runtime_epoch"] = json!(s.chat.epoch());
    doc["thread"]["updated_at"] = at;
    doc["thread"]["state"]["status"] = json!(if agent_row["status"] == "unknown" {
        "unknown"
    } else if timeline.turn_running {
        "running"
    } else {
        "idle"
    });
    doc["thread"]["state"]["running_turn_id"] = if timeline.turn_running {
        doc["turns"]
            .as_array()
            .and_then(|turns| turns.last())
            .map(|turn| turn["turn_id"].clone())
            .unwrap_or(Value::Null)
    } else {
        Value::Null
    };
    let turn = doc["thread"]["state"]["running_turn_id"].clone();
    chat::emit(
        s,
        &mirror,
        &mut doc,
        "core.agent.event",
        json!({"turn_id":turn,"event":native}),
    );
    let resources = &event["payload"]["resources"];
    if let Some(files) = resources.as_array().filter(|files| !files.is_empty())
        && let Some(entry) = timeline.entries.iter().find(|entry| native["id"] == entry.id)
        && let Ok(mut native_event) = serde_json::from_value::<AgentEvent>(native.clone())
        && let Some(tool) = crate::tool_resources::native_tool_payload(
            &mut native_event, &format!("{mirror}:{}", entry.turn), resources.clone())
    {
        let artifacts = files.iter().filter_map(|file| {
            let id = file["artifact_id"].as_str()?;
            if crate::files::attachment_metadata(&doc, id).is_some() { return None; }
            crate::files::attachment_metadata(&lead, id).cloned()
        }).collect();
        crate::tool_resources::append_artifacts(&mut doc, &tool["turn_id"], artifacts);
        chat::emit(s, &mirror, &mut doc, "core.tool.completed", tool);
    }
    chat::save(s, &mirror, &mut doc).map_err(|(_, body)| body.0["error"].clone())?;
    if let Some(row) = lead["thread"]["state"]["agents"]["agents"]
        .as_array_mut()
        .and_then(|rows| rows.iter_mut().find(|row| row["agent_id"] == agent))
    {
        let mut changed = row["thread_id"] != mirror;
        row["thread_id"] = json!(mirror);
        if native["content"]["kind"] == "subagent" {
            for (source, target) in [
                ("agent_type", "title"),
                ("model", "model"),
                ("effort", "effort"),
            ] {
                if let Some(value) = native["content"][source]
                    .as_str()
                    .filter(|value| !value.is_empty())
                {
                    changed |= row[target] != value;
                    row[target] = json!(value);
                }
            }
            if let Some(status) = native["content"]["status"].as_str() {
                let status = match status {
                    "in_progress" => "running",
                    "interrupted" => "cancelled",
                    status => status,
                };
                changed |= row["status"] != status;
                row["status"] = json!(status);
            }
        }
        if changed {
            chat::save(s, parent, &mut lead).map_err(|(_, body)| body.0["error"].clone())?;
        }
    }
    Ok(())
}

/// Older native sessions already have committed provider events; build their
/// read-only pages from those records when this feature is first enabled.
pub async fn recover(s: &Arc<AppState>) -> Result<(), Value> {
    for doc in s
        .store
        .list(chat::COLLECTION)
        .map_err(|error| json!({"message":error.to_string()}))?
    {
        if doc["thread"]["state"]["lineage"]["native_subagent"].is_string() {
            continue;
        }
        if !doc["thread"]["state"]["agents"]["agents"]
            .as_array()
            .is_some_and(|agents| !agents.is_empty())
        {
            continue;
        }
        let Some(id) = doc["thread"]["thread_id"].as_str() else {
            continue;
        };
        for event in crate::control_content::all_events(s, id)?
            .into_iter()
            .filter(|event| event["event_type"] == "core.agent.event")
        {
            mirror(s, id, &event).await?;
        }
    }
    Ok(())
}
