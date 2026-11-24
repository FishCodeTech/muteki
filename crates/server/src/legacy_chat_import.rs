//! Read-only legacy CHAT snapshot migration. This imports metadata, never work.
use crate::{history_import, store::Store};
use anyhow::{Context, Result};
use serde::Serialize;
use serde_json::{Value, json};
use std::{collections::BTreeMap, path::Path};

#[derive(Default, Debug, Serialize)]
pub struct Summary {
    pub threads: usize,
    pub turns: usize,
    pub messages: usize,
    pub tools: usize,
    pub artifacts: usize,
    pub projects: usize,
    pub workspaces: usize,
    pub skipped: usize,
    pub preserved_records: usize,
}

fn text<'a>(v: &'a Value, key: &str) -> &'a str {
    v.get(key).and_then(Value::as_str).unwrap_or("")
}
fn historical(mut v: Value) -> Value {
    v["historical"] = json!(true);
    v["import_source"] = json!("legacy_platform");
    v
}
fn preserve(
    store: &Store,
    table: &str,
    id: &str,
    row: &Value,
    summary: &mut Summary,
) -> Result<()> {
    if store.put_if_absent("history_chat_records", &format!("{table}:{id}"), row)? {
        summary.preserved_records += 1;
    }
    Ok(())
}
fn payload(row: &Value) -> Result<Value> {
    let mut p = row
        .get("payload")
        .filter(|v| v.is_object())
        .cloned()
        .context("Legacy CHAT payload must be an object")?;
    for key in [
        "thread_id",
        "turn_id",
        "message_id",
        "project_id",
        "workspace_id",
        "seq",
        "stream_seq",
        "status",
        "role",
        "command_id",
        "idempotency_key",
    ] {
        if let Some(value) = row.get(key) {
            p[key] = value.clone();
        }
    }
    Ok(p)
}
fn dormant_state(mut state: Value, thread_id: &str) -> Value {
    if !state.is_object() {
        state = json!({});
    }
    let original = state.get("status").cloned().unwrap_or(Value::Null);
    let archived = original == "archived";
    state["thread_id"] = json!(thread_id);
    state["original_status"] = original;
    state["status"] = json!(if archived { "archived" } else { "idle" });
    for key in [
        "running_turn_id",
        "agent_session_id",
        "pending_approval",
        "pending_user_input",
        "pending_handoff",
        "quota_hold",
        "quota_resume",
        "queue_failed_item_id",
    ] {
        state[key] = Value::Null;
    }
    state["pending_approvals"] = json!({});
    state["queue_count"] = json!(0);
    state["queue_paused"] = json!(false);
    state["queue_pause_reason"] = json!("");
    state["history_rebuild_pending"] = json!(false);
    state["history_recovery_required"] = json!(false);
    let head = state["head_stream_seq"].as_u64().unwrap_or(0);
    let read = state["read_stream_seq"].as_u64().unwrap_or(0);
    state["unread"] = json!(head > read);
    historical(state)
}
fn dormant_turn(mut turn: Value) -> Value {
    turn["original_status"] = turn["status"].clone();
    if matches!(
        text(&turn, "status"),
        "queued" | "running" | "pending" | "waiting" | "starting"
    ) {
        turn["status"] = json!("interrupted");
        turn["import_interruption_reason"] =
            json!("Historical execution was not resumed during migration");
    }
    historical(turn)
}
fn tool_event(tools: &mut BTreeMap<(String, String), Value>, event: &Value) {
    let p = &event["payload"];
    let turn_id = text(p, "turn_id");
    let call_id = ["tool_call_id", "call_id", "invocation_id", "id"]
        .iter()
        .find_map(|key| p.get(key).and_then(Value::as_str).filter(|s| !s.is_empty()))
        .unwrap_or_else(|| text(event, "event_id"));
    let tool = tools.entry((turn_id.to_owned(), call_id.to_owned())).or_insert_with(|| historical(json!({
        "tool_id":call_id,"turn_id":p["turn_id"],"name":"unknown","status":"running",
        "output":"","output_complete":false,"source_events":[],"stream_seq":event["stream_seq"]
    })));
    for key in ["tool_name", "name", "tool"] {
        if let Some(name) = p.get(key).and_then(Value::as_str).filter(|s| !s.is_empty()) {
            tool["name"] = json!(name);
            break;
        }
    }
    for key in ["arguments", "input", "args"] {
        if let Some(v) = p.get(key) {
            tool["arguments"] = v.clone();
            break;
        }
    }
    let event_type = text(event, "event_type");
    let terminal = matches!(event_type, "core.tool.completed" | "core.tool.failed");
    if let Some(output) = ["output", "result"]
        .iter()
        .find_map(|k| p.get(k).filter(|v| !v.is_null()))
    {
        tool["output"] = output.clone();
        tool["output_complete"] = json!(terminal);
    } else if let Some(chunk) = p.get("chunk").and_then(Value::as_str) {
        if let Some(output) = tool["output"].as_str() {
            tool["output"] = json!(format!("{output}{chunk}"));
        }
    }
    for key in ["summary", "error"] {
        if let Some(v) = p.get(key) {
            tool[key] = v.clone();
        }
    }
    let outcome = ["status", "result_status", "outcome"]
        .iter()
        .find_map(|k| p.get(k).and_then(Value::as_str))
        .unwrap_or("")
        .to_lowercase();
    let exit = p
        .get("exit_code")
        .or_else(|| p.get("exitCode"))
        .filter(|v| !v.is_null());
    let error = p.get("error").is_some_and(|v| match v {
        Value::Null => false,
        Value::Bool(v) => *v,
        Value::String(v) => !v.is_empty(),
        Value::Object(v) => !v.is_empty(),
        Value::Array(v) => !v.is_empty(),
        _ => true,
    });
    let status = if matches!(outcome.as_str(), "declined" | "denied" | "rejected") {
        Some("declined")
    } else if matches!(
        outcome.as_str(),
        "cancelled" | "canceled" | "aborted" | "interrupted"
    ) {
        Some("cancelled")
    } else if matches!(outcome.as_str(), "failed" | "error")
        || event_type == "core.tool.failed"
        || error
        || p["is_error"] == true
        || p["isError"] == true
        || exit.is_some_and(|v| v.as_i64() != Some(0))
    {
        Some("failed")
    } else if tool["status"] == "declined" {
        None
    } else if event_type == "core.tool.completed"
        || matches!(
            outcome.as_str(),
            "completed" | "success" | "ok" | "succeeded"
        )
    {
        Some("completed")
    } else {
        None
    };
    if let Some(status) = status {
        tool["status"] = json!(status);
    }
    tool["source_events"].as_array_mut().unwrap().push(json!({"event_id":event["event_id"],"stream_seq":event["stream_seq"],"event_type":event["event_type"]}));
}

pub fn run(state_dir: &Path, store: &Store) -> Result<Summary> {
    let source = state_dir.join("legacy/_platform/platform.db");
    let conn =
        history_import::read_db(&source).context("Open copied legacy CHAT snapshot read-only")?;
    let mut summary = Summary::default();
    let mut threads = BTreeMap::<String, Value>::new();
    for (table, key) in [
        ("threads", "thread_id"),
        ("conv_turns", "turn_id"),
        ("conv_messages", "message_id"),
        ("conv_thread_state", "thread_id"),
        ("conv_thread_runtime", "thread_id"),
        ("projects", "project_id"),
        ("workspaces", "workspace_id"),
        ("artifacts", "sha256"),
    ] {
        for row in history_import::table_rows(&conn, table)? {
            let id = text(&row, key);
            anyhow::ensure!(
                !id.is_empty(),
                "Legacy CHAT record is missing its identifier"
            );
            preserve(store, table, id, &row, &mut summary)?;
            let mut p = payload(&row)?;
            if table == "workspaces" && p.get("settings").is_none_or(Value::is_null) {
                p["settings"] = json!({});
            }
            match table {
                "threads" => {
                    threads.insert(id.to_owned(), historical(json!({"thread":p,"runtime":{},"messages":[],"turns":[],"tools":[],"artifacts":[],"event_seq":0})));
                }
                "projects" | "workspaces" => {
                    let compatible = if table == "projects" {
                        p["name"].is_string() && p["settings"].is_object()
                    } else {
                        p["root_path"].is_string() && p["kind"].is_string()
                    };
                    if compatible && store.put_if_absent(table, id, &historical(p))? {
                        if table == "projects" {
                            summary.projects += 1;
                        } else {
                            summary.workspaces += 1;
                        }
                    }
                }
                "artifacts" => {
                    store.put_if_absent("history_chat_artifacts", id, &p)?;
                }
                _ => {}
            }
        }
    }
    for (table, field) in [
        ("conv_thread_state", "state"),
        ("conv_thread_runtime", "runtime"),
        ("conv_turns", "turns"),
        ("conv_messages", "messages"),
    ] {
        for row in history_import::table_rows(&conn, table)? {
            let tid = text(&row, "thread_id");
            let Some(doc) = threads.get_mut(tid) else {
                continue;
            };
            let p = payload(&row)?;
            match field {
                "state" => doc["thread"]["state"] = dormant_state(p, tid),
                "runtime" => doc["runtime"] = p,
                "turns" => doc["turns"].as_array_mut().unwrap().push(dormant_turn(p)),
                "messages" => doc["messages"].as_array_mut().unwrap().push(historical(p)),
                _ => unreachable!(),
            }
        }
    }
    let mut tools = BTreeMap::<String, BTreeMap<(String, String), Value>>::new();
    let mut stmt = conn.prepare("SELECT event_id,aggregate_id,stream_seq,payload FROM domain_events WHERE aggregate_type='thread' AND (event_type LIKE 'core.tool.%' OR event_type IN ('core.artifact.attached','core.artifact.created','core.turn.started')) ORDER BY seq")?;
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        let eid: String = row.get(0)?;
        let tid: String = row.get(1)?;
        let seq: u64 = row.get(2)?;
        let raw: String = row.get(3)?;
        let event: Value = serde_json::from_str(&raw).context("Decode historical CHAT event")?;
        preserve(store, "domain_events", &eid, &event, &mut summary)?;
        let Some(doc) = threads.get_mut(&tid) else {
            continue;
        };
        doc["event_seq"] = json!(doc["event_seq"].as_u64().unwrap_or(0).max(seq));
        match text(&event, "event_type") {
            "core.artifact.attached" | "core.artifact.created" => {
                let mut artifact = historical(event["payload"].clone());
                artifact["event_id"] = json!(eid);
                artifact["stream_seq"] = json!(seq);
                doc["artifacts"].as_array_mut().unwrap().push(artifact);
            }
            "core.turn.started" => {
                let p = &event["payload"];
                if let Some(turn) = doc["turns"]
                    .as_array_mut()
                    .unwrap()
                    .iter_mut()
                    .find(|t| t["turn_id"] == p["turn_id"])
                {
                    if p["runtime"].is_object() {
                        if !turn["runtime_sources"].is_array() {
                            turn["runtime_sources"] = json!([]);
                        }
                        turn["runtime_sources"]
                            .as_array_mut()
                            .unwrap()
                            .push(json!({"event_id":eid,"stream_seq":seq,"runtime":p["runtime"]}));
                    }
                }
            }
            kind if kind.starts_with("core.tool.") => {
                tool_event(tools.entry(tid).or_default(), &event)
            }
            _ => {}
        }
    }
    let mut heads = conn.prepare("SELECT aggregate_id,MAX(stream_seq) FROM domain_events WHERE aggregate_type='thread' GROUP BY aggregate_id")?;
    for row in heads.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, u64>(1)?)))? {
        let (tid, head) = row?;
        if let Some(doc) = threads.get_mut(&tid) {
            doc["event_seq"] = json!(head);
        }
    }
    for (tid, mut doc) in threads {
        if !doc["thread"]["state"].is_object() {
            doc["thread"]["state"] = dormant_state(json!({}), &tid);
        }
        doc["thread"]["historical"] = json!(true);
        doc["thread"]["archived"] = json!(doc["thread"]["state"]["status"] == "archived");
        doc["turns"]
            .as_array_mut()
            .unwrap()
            .sort_by_key(|t| t["seq"].as_u64().unwrap_or(0));
        doc["messages"]
            .as_array_mut()
            .unwrap()
            .sort_by_key(|m| m["stream_seq"].as_u64().unwrap_or(0));
        let mut tool_rows: Vec<Value> = tools
            .remove(&tid)
            .unwrap_or_default()
            .into_values()
            .collect();
        for tool in &mut tool_rows {
            tool["original_status"] = tool["status"].clone();
            if tool["status"] == "running" {
                tool["status"] = json!("interrupted");
            }
            tool["output_summary"] = tool
                .get("summary")
                .cloned()
                .unwrap_or_else(|| tool["output"].clone());
        }
        tool_rows.sort_by_key(|t| t["stream_seq"].as_u64().unwrap_or(0));
        doc["tools"] = json!(tool_rows);
        for artifact in doc["artifacts"].as_array_mut().unwrap() {
            let Some(digest) = artifact["sha256"].as_str().map(str::to_owned) else { continue; };
            let source = state_dir.join("legacy/conversation_artifacts").join(&digest);
            if !source.is_file() { artifact["migration_error"] = json!("Original attachment bytes unavailable"); continue; }
            let reference = store.get("legacy_attachment_refs", &digest)?.and_then(|v| v.as_str().map(str::to_owned)).unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
            uuid::Uuid::parse_str(&reference)?;
            let target = state_dir.join("uploads").join(&reference);
            let bytes = std::fs::read(&source)?;
            use sha2::{Digest, Sha256};
            anyhow::ensure!(format!("{:x}", Sha256::digest(&bytes)) == digest, "Legacy attachment digest mismatch");
            if target.exists() { anyhow::ensure!(std::fs::read(&target)? == bytes, "Imported attachment conflict"); }
            else {
                use std::io::Write;
                let mut staged = tempfile::NamedTempFile::new_in(state_dir.join("uploads"))?;
                staged.write_all(&bytes)?; staged.as_file().sync_all()?; staged.persist(&target)?;
            }
            store.put("legacy_attachment_refs", &digest, &json!(reference))?;
            store.put("attachment_integrity", &reference, &json!({"sha256":digest,"size":bytes.len(),"basis":"legacy-import"}))?;
            artifact["artifact_id"] = json!(reference);
            artifact["legacy_sha256"] = json!(digest);
        }

        if store.put_if_absent("conversation_threads", &tid, &doc)? {
            summary.threads += 1;
            summary.turns += doc["turns"].as_array().unwrap().len();
            summary.messages += doc["messages"].as_array().unwrap().len();
            summary.tools += doc["tools"].as_array().unwrap().len();
            summary.artifacts += doc["artifacts"].as_array().unwrap().len();
        } else {
            summary.skipped += 1;
        }
    }
    upgrade_attachment_references(store)?;
    Ok(summary)
}

/// Upgrade imported turn references after the verified bytes have acquired
/// opaque IDs. Preserve message text, runtime state and user edits.
pub fn upgrade_attachment_references(store: &Store) -> Result<()> {
    for mut doc in store.list("conversation_threads")? {
        if doc["historical"] != true || doc["import_source"] != "legacy_platform" { continue; }
        let references: BTreeMap<String, String> = doc["artifacts"].as_array().into_iter().flatten().filter_map(|artifact| {
            let digest = artifact["legacy_sha256"].as_str()?;
            let reference = artifact["artifact_id"].as_str()?;
            if artifact["sha256"] != digest || uuid::Uuid::parse_str(reference).is_err() { return None; }
            Some((digest.to_owned(), reference.to_owned()))
        }).collect();
        let mut changed = false;
        for turn in doc["turns"].as_array_mut().into_iter().flatten() {
            for attachment in turn["attachments"].as_array_mut().into_iter().flatten() {
                if let Some(reference) = attachment.as_str().and_then(|digest| references.get(digest)) {
                    *attachment = json!(reference); changed = true;
                }
            }
        }
        if changed {
            let id = doc["thread"]["thread_id"].as_str().context("Imported thread identity missing")?.to_owned();
            store.put("conversation_threads", &id, &doc)?;
        }
    }
    Ok(())
}
