//! Run the source Pi Decide extension in the selected Worker container.
//! The extension owns the draft; the host still validates its committed
//! operations against the current shared graph before applying them.

use super::planner_artifacts as legacy_artifacts;

use crate::{
    AppState,
    agent_runtime::credentials,
    native_chat,
    runs::{
        PermissiveU64,
        container_exec::{self, WorkerSpec},
        graph::SharedGraph,
        session_dir,
    },
};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    fs,
    io::{BufRead, BufReader, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

fn compact_trace(text: &str, max_chars: usize) -> String {
    text.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(max_chars)
        .collect()
}

fn historical_step_trace(session_root: &Path, intent_id: &str) -> Value {
    if Path::new(intent_id)
        .file_name()
        .and_then(|name| name.to_str())
        != Some(intent_id)
    {
        return Value::Null;
    }
    let path = session_root
        .join(".muteki-agent-state/step_traces")
        .join(format!("{intent_id}.json"));
    fs::read_to_string(path)
        .ok()
        .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
        .filter(Value::is_object)
        .unwrap_or(Value::Null)
}

fn regular_artifact_text(session_root: &Path, filename: &str) -> Option<String> {
    let arts_dir = session_root.join("arts");
    if !fs::symlink_metadata(&arts_dir).ok()?.file_type().is_dir() {
        return None;
    }
    let path = arts_dir.join(filename);
    if !fs::symlink_metadata(&path).ok()?.file_type().is_file() {
        return None;
    }
    fs::read_to_string(path).ok()
}

fn historical_artifact_output(session_root: &Path, artifact_id: &str) -> String {
    if artifact_id.strip_prefix("art-").is_some_and(|suffix| {
        !suffix.is_empty() && suffix.bytes().all(|byte| byte.is_ascii_digit())
    }) {
        let Some(raw) = regular_artifact_text(session_root, &format!("{artifact_id}.json")) else {
            return String::new();
        };
        let Ok(value) = serde_json::from_str::<Value>(&raw) else {
            return String::new();
        };
        let output = value["content"]
            .as_str()
            .or_else(|| value["result"]["condensed"].as_str())
            .or_else(|| value["result"].as_str())
            .unwrap_or("");
        return compact_trace(output, 420);
    }
    if artifact_id.len() != 12 || !artifact_id.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return String::new();
    }
    let arts_dir = session_root.join("arts");
    if !fs::symlink_metadata(&arts_dir).is_ok_and(|metadata| metadata.file_type().is_dir()) {
        return String::new();
    }
    let Ok(entries) = fs::read_dir(arts_dir) else {
        return String::new();
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name
            .strip_prefix(artifact_id)
            .is_some_and(|suffix| suffix.starts_with('.'))
            && entry.file_type().is_ok_and(|kind| kind.is_file())
            && let Ok(raw) = fs::read_to_string(entry.path())
        {
            return compact_trace(&raw, 420);
        }
    }
    String::new()
}

fn historical_artifact_command(session_root: &Path, artifact_id: &str) -> String {
    if !artifact_id.strip_prefix("art-").is_some_and(|suffix| {
        !suffix.is_empty() && suffix.bytes().all(|byte| byte.is_ascii_digit())
    }) {
        return String::new();
    }
    regular_artifact_text(session_root, &format!("{artifact_id}.json"))
        .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
        .and_then(|value| {
            value["command"]
                .as_str()
                .map(|command| compact_trace(command, 360))
        })
        .unwrap_or_default()
}

fn historical_observation_traces(
    graph_events: &[Value],
    session_root: &Path,
    intent_id: &str,
) -> Vec<Value> {
    let mut traces = Vec::new();
    for event in graph_events.iter().filter(|event| {
        event["kind"] == "observation_added" && event["payload"]["intent_id"] == intent_id
    }) {
        let payload = &event["payload"];
        let references = payload["evidence_provenance"]["artifact_refs"].as_array();
        if let Some(references) = references.filter(|refs| !refs.is_empty()) {
            for reference in references {
                let artifact_id = reference["artifact_id"].as_str().unwrap_or("");
                let output = historical_artifact_output(session_root, artifact_id);
                let command = reference["command"]
                    .as_str()
                    .filter(|s| !s.is_empty())
                    .map(|s| compact_trace(s, 360))
                    .unwrap_or_else(|| historical_artifact_command(session_root, artifact_id));
                traces.push(json!({
                    "tool":"bash",
                    "command":command,
                    "output":if output.is_empty() { "(empty output)" } else { &output },
                }));
            }
        } else if let Some(artifact_id) =
            payload["artifact_id"].as_str().filter(|id| !id.is_empty())
        {
            let output = historical_artifact_output(session_root, artifact_id);
            let command = payload["command"]
                .as_str()
                .filter(|s| !s.is_empty())
                .map(|s| compact_trace(s, 360))
                .unwrap_or_else(|| historical_artifact_command(session_root, artifact_id));
            traces.push(json!({
                "tool":"bash",
                "command":command,
                "output":if output.is_empty() { "(empty output)" } else { &output },
            }));
        }
    }
    traces
        .into_iter()
        .rev()
        .take(15)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect()
}

fn historical_pi_progress(session_root: &Path, worker_id: &str) -> Value {
    if worker_id.is_empty()
        || Path::new(worker_id)
            .file_name()
            .and_then(|name| name.to_str())
            != Some(worker_id)
    {
        return Value::Null;
    }
    let agent_dir = session_root.join(".muteki-agent-state").join(worker_id);
    let sessions = agent_dir.join("sessions");
    let search_root = if sessions.is_dir() {
        &sessions
    } else {
        &agent_dir
    };
    if !search_root.is_dir() {
        return Value::Null;
    }
    let latest = walkdir::WalkDir::new(search_root)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|entry| {
            entry.file_type().is_file()
                && entry.path().extension().is_some_and(|ext| ext == "jsonl")
        })
        .filter_map(|entry| {
            Some((
                entry.metadata().ok()?.modified().ok()?,
                entry.path().to_path_buf(),
            ))
        })
        .max_by_key(|(modified, _)| *modified)
        .map(|(_, path)| path);
    let Some(path) = latest else {
        return Value::Null;
    };
    let Ok(raw) = fs::read_to_string(path) else {
        return Value::Null;
    };
    let mut order = Vec::new();
    let mut calls = HashMap::<String, Value>::new();
    let mut latest_note = String::new();
    let mut started_at = 0.0;
    for line in raw.lines() {
        let Ok(row) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if !row.is_object() {
            continue;
        }
        if started_at <= 0.0 {
            if let Some(ts) = row["timestamp"].as_f64().filter(|ts| *ts > 0.0) {
                started_at = if ts > 1e11 { ts / 1000.0 } else { ts };
            } else if let Some(ts) = row["timestamp"].as_str() {
                started_at = chrono::DateTime::parse_from_rfc3339(ts)
                    .map(|value| value.timestamp_millis() as f64 / 1000.0)
                    .unwrap_or(0.0);
            }
        }
        if row["type"] != "message" {
            continue;
        }
        let message = &row["message"];
        match message["role"].as_str() {
            Some("assistant") => {
                for block in message["content"].as_array().into_iter().flatten() {
                    match block["type"].as_str() {
                        Some("text" | "thinking") => {
                            let text = block["text"]
                                .as_str()
                                .or_else(|| block["thinking"].as_str())
                                .unwrap_or("");
                            if !text.trim().is_empty() {
                                latest_note = compact_trace(text, 360);
                            }
                        }
                        Some("toolCall") => {
                            let call_id = block["id"]
                                .as_str()
                                .map(str::to_owned)
                                .unwrap_or_else(|| format!("call_{}", order.len() + 1));
                            let args = &block["arguments"];
                            let command = args["command"]
                                .as_str()
                                .or_else(|| args["path"].as_str())
                                .or_else(|| args["title"].as_str())
                                .map(str::to_owned)
                                .unwrap_or_else(|| {
                                    if args.is_null() {
                                        String::new()
                                    } else {
                                        args.to_string()
                                    }
                                });
                            order.push(call_id.clone());
                            calls.insert(
                                call_id,
                                json!({
                                "tool":block["name"].as_str().unwrap_or("tool"),
                                "command":compact_trace(&command, 240),
                                "output":"(running...)"}),
                            );
                        }
                        _ => {}
                    }
                }
            }
            Some("toolResult") => {
                let call_id = message["toolCallId"].as_str().unwrap_or("");
                let output = if let Some(blocks) = message["content"].as_array() {
                    blocks
                        .iter()
                        .filter(|block| block["type"] == "text")
                        .filter_map(|block| block["text"].as_str())
                        .collect::<Vec<_>>()
                        .join(" ")
                } else {
                    message["content"].as_str().unwrap_or("").to_owned()
                };
                if let Some(call) = calls.get_mut(call_id) {
                    let output = compact_trace(&output, 280);
                    call["output"] = json!(if output.is_empty() {
                        "(empty output)"
                    } else {
                        &output
                    });
                }
            }
            _ => {}
        }
    }
    let traces: Vec<Value> = order
        .iter()
        .rev()
        .take(6)
        .rev()
        .filter_map(|id| calls.get(id).cloned())
        .collect();
    json!({"started_ts":started_at,"tool_calls":order.len(),
        "recent_traces":traces,"latest_note":latest_note})
}

struct PermissiveOptStr<'a>(Option<std::borrow::Cow<'a, str>>);

impl<'de> serde::Deserialize<'de> for PermissiveOptStr<'de> {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visitor;
        impl<'de> serde::de::Visitor<'de> for Visitor {
            type Value = PermissiveOptStr<'de>;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("any JSON value")
            }
            fn visit_borrowed_str<E: serde::de::Error>(
                self,
                v: &'de str,
            ) -> Result<Self::Value, E> {
                Ok(PermissiveOptStr(Some(std::borrow::Cow::Borrowed(v))))
            }
            fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<Self::Value, E> {
                Ok(PermissiveOptStr(Some(std::borrow::Cow::Owned(v.to_owned()))))
            }
            fn visit_string<E: serde::de::Error>(self, v: String) -> Result<Self::Value, E> {
                Ok(PermissiveOptStr(Some(std::borrow::Cow::Owned(v))))
            }
            fn visit_u64<E: serde::de::Error>(self, _: u64) -> Result<Self::Value, E> {
                Ok(PermissiveOptStr(None))
            }
            fn visit_i64<E: serde::de::Error>(self, _: i64) -> Result<Self::Value, E> {
                Ok(PermissiveOptStr(None))
            }
            fn visit_f64<E: serde::de::Error>(self, _: f64) -> Result<Self::Value, E> {
                Ok(PermissiveOptStr(None))
            }
            fn visit_bool<E: serde::de::Error>(self, _: bool) -> Result<Self::Value, E> {
                Ok(PermissiveOptStr(None))
            }
            fn visit_none<E: serde::de::Error>(self) -> Result<Self::Value, E> {
                Ok(PermissiveOptStr(None))
            }
            fn visit_unit<E: serde::de::Error>(self) -> Result<Self::Value, E> {
                Ok(PermissiveOptStr(None))
            }
            fn visit_seq<A: serde::de::SeqAccess<'de>>(
                self,
                mut seq: A,
            ) -> Result<Self::Value, A::Error> {
                while seq.next_element::<serde::de::IgnoredAny>()?.is_some() {}
                Ok(PermissiveOptStr(None))
            }
            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                mut map: A,
            ) -> Result<Self::Value, A::Error> {
                while map
                    .next_entry::<serde::de::IgnoredAny, serde::de::IgnoredAny>()?
                    .is_some()
                {}
                Ok(PermissiveOptStr(None))
            }
        }
        deserializer.deserialize_any(Visitor)
    }
}

struct PeekLineHead<'a> {
    seq: Option<u64>,
    event_type: Option<std::borrow::Cow<'a, str>>,
}

impl<'de> serde::Deserialize<'de> for PeekLineHead<'de> {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visitor;
        impl<'de> serde::de::Visitor<'de> for Visitor {
            type Value = PeekLineHead<'de>;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("a JSON object")
            }
            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                mut map: A,
            ) -> Result<Self::Value, A::Error> {
                let mut seq = None;
                let mut event_type = None;
                while let Some(key) = map.next_key::<std::borrow::Cow<'de, str>>()? {
                    match key.as_ref() {
                        "seq" => seq = map.next_value::<PermissiveU64>()?.0,
                        "event_type" => event_type = map.next_value::<PermissiveOptStr<'de>>()?.0,
                        _ => {
                            map.next_value::<serde::de::IgnoredAny>()?;
                        }
                    }
                }
                Ok(PeekLineHead { seq, event_type })
            }
        }
        deserializer.deserialize_map(Visitor)
    }
}

#[derive(Default, Clone)]
struct IntentWindow {
    claim_seq: u64,
    claim_solver_id: Option<String>,
    worker: String,
    claim_ts: Option<f64>,
    concluded_seq: Option<u64>,
    order: Vec<String>,
    calls: HashMap<String, Value>,
    latest_note: String,
    online: bool,
}

#[derive(Default)]
struct StepEventTracker {
    #[cfg(unix)]
    dev: u64,
    #[cfg(unix)]
    ino: u64,
    offset: u64,
    seq: u64,
    fallback_workers: HashMap<String, String>,
    intents: HashMap<String, IntentWindow>,
}

impl StepEventTracker {
    fn from_snapshot(snapshot: &Value, session_root: &Path) -> Self {
        let mut fallback_workers = HashMap::new();
        for section in ["open_steps", "done_steps"] {
            for step in snapshot[section].as_array().into_iter().flatten() {
                let Some(intent_id) = step["id"].as_str() else {
                    continue;
                };
                let worker = step["worker_id"]
                    .as_str()
                    .map(str::to_owned)
                    .or_else(|| {
                        historical_step_trace(session_root, intent_id)["worker_id"]
                            .as_str()
                            .map(str::to_owned)
                    })
                    .unwrap_or_default();
                fallback_workers.insert(intent_id.to_owned(), worker);
            }
        }
        Self {
            fallback_workers,
            ..Self::default()
        }
    }

    fn observe(&mut self, event: &Value) {
        let seq = event["seq"].as_u64().unwrap_or(0);
        let payload = &event["payload"];
        let delta_kind = match event["event_type"].as_str() {
            Some("blackboard.delta") => payload["kind"].as_str(),
            None => event["kind"].as_str(),
            _ => None,
        };
        if let Some(delta) = delta_kind {
            match delta {
                "intent_claimed" => {
                    let Some(intent_id) = payload["intent_id"]
                        .as_str()
                        .or_else(|| event["intent_id"].as_str())
                    else {
                        return;
                    };
                    let claim_solver_id = event["solver_id"]
                        .as_str()
                        .or_else(|| payload["worker"].as_str())
                        .or_else(|| event["actor"].as_str())
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .map(str::to_owned);
                    let worker = claim_solver_id
                        .clone()
                        .or_else(|| self.fallback_workers.get(intent_id).cloned())
                        .unwrap_or_default();
                    self.intents.insert(
                        intent_id.to_owned(),
                        IntentWindow {
                            claim_seq: seq,
                            claim_solver_id,
                            worker,
                            claim_ts: event["ts"].as_f64(),
                            concluded_seq: None,
                            order: Vec::new(),
                            calls: HashMap::new(),
                            latest_note: String::new(),
                            online: false,
                        },
                    );
                }
                "intent_concluded" => {
                    let Some(intent_id) = payload["intent_id"]
                        .as_str()
                        .or_else(|| event["intent_id"].as_str())
                    else {
                        return;
                    };
                    if let Some(win) = self.intents.get_mut(intent_id)
                        && win.claim_seq > 0
                        && seq > win.claim_seq
                        && win.concluded_seq.is_none()
                    {
                        win.concluded_seq = Some(seq);
                    }
                }
                _ => {}
            }
            return;
        }
        match event["event_type"].as_str() {
            Some(kind @ ("tool.start" | "tool.result" | "text.delta" | "worker.status")) => {
                let Some(solver) = event["solver_id"].as_str() else {
                    return;
                };
                for win in self.intents.values_mut() {
                    if win.claim_seq == 0
                        || seq <= win.claim_seq
                        || win.concluded_seq.is_some_and(|concluded| seq >= concluded)
                        || win.worker != solver
                    {
                        continue;
                    }
                    match kind {
                        "tool.start" => {
                            let call_id = payload["call_id"]
                                .as_str()
                                .or_else(|| payload["tool_call_id"].as_str())
                                .unwrap_or("");
                            if !call_id.is_empty() {
                                win.order.push(call_id.to_owned());
                                win.calls.insert(
                                    call_id.to_owned(),
                                    json!({
                                        "tool": payload["tool"],
                                        "command": payload["tool"],
                                        "output": "(running...)"
                                    }),
                                );
                            }
                        }
                        "tool.result" => {
                            let call_id = payload["call_id"]
                                .as_str()
                                .or_else(|| payload["tool_call_id"].as_str())
                                .unwrap_or("");
                            if !call_id.is_empty() {
                                if !win.calls.contains_key(call_id) {
                                    win.order.push(call_id.to_owned());
                                }
                                let entry = win.calls.entry(call_id.to_owned()).or_insert_with(|| {
                                    json!({
                                        "tool": payload["tool"],
                                        "command": payload["tool"]
                                    })
                                });
                                entry["output"] = payload["result"]["condensed"].clone();
                            }
                        }
                        "text.delta" => {
                            if let Some(text) = payload["text"].as_str() {
                                win.latest_note = text.chars().take(360).collect();
                            }
                        }
                        "worker.status" => {
                            if payload["online"] == true {
                                win.online = true;
                            }
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }

    fn sync_jsonl(&mut self, session_root: &Path) {
        let path = session_root.join("events.jsonl");
        let Ok(mut file) = fs::File::open(path) else {
            #[cfg(unix)]
            {
                self.dev = 0;
                self.ino = 0;
            }
            self.offset = 0;
            self.seq = 0;
            self.intents.clear();
            return;
        };
        let Ok(meta) = file.metadata() else {
            return;
        };
        let observed_len = meta.len();
        #[cfg(unix)]
        let (dev, ino) = {
            use std::os::unix::fs::MetadataExt;
            (meta.dev(), meta.ino())
        };
        #[cfg(unix)]
        let same_file = self.offset == 0 || (self.dev == dev && self.ino == ino);
        #[cfg(not(unix))]
        let same_file = true;
        if !same_file || self.offset > observed_len {
            self.offset = 0;
            self.seq = 0;
            self.intents.clear();
        }
        #[cfg(unix)]
        {
            self.dev = dev;
            self.ino = ino;
        }
        if self.offset == observed_len {
            return;
        }
        if self.offset > 0 && file.seek(SeekFrom::Start(self.offset)).is_err() {
            if file.seek(SeekFrom::Start(0)).is_err() {
                return;
            }
            self.offset = 0;
            self.seq = 0;
            self.intents.clear();
        }
        let mut reader = BufReader::new(file.take(observed_len.saturating_sub(self.offset)));
        let mut line = String::new();
        let mut scanned_offset = self.offset;
        loop {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) | Err(_) => break,
                Ok(bytes_read) => {
                    scanned_offset += bytes_read as u64;
                    if !line.ends_with('\n') {
                        break;
                    }
                    line.pop();
                    if line.ends_with('\r') {
                        line.pop();
                    }
                    if let Ok(head) = serde_json::from_str::<PeekLineHead<'_>>(&line) {
                        self.seq = std::cmp::max(self.seq + 1, head.seq.unwrap_or(0));
                        if matches!(
                            head.event_type.as_deref(),
                            Some(
                                "blackboard.delta"
                                    | "tool.start"
                                    | "tool.result"
                                    | "text.delta"
                                    | "worker.status"
                            )
                        ) && let Ok(mut ev) = serde_json::from_str::<Value>(&line)
                            && ev.is_object()
                        {
                            ev["seq"] = json!(self.seq);
                            self.observe(&ev);
                        }
                    }
                    self.offset = scanned_offset;
                }
            }
        }
    }

    fn apply(&self, snapshot: &mut Value, graph_events: &[Value], session_root: &Path) {
        for section in ["open_steps", "done_steps"] {
            let Some(steps) = snapshot[section].as_array_mut() else {
                continue;
            };
            for step in steps {
                let Some(intent_id) = step["id"].as_str().map(str::to_owned) else {
                    continue;
                };
                let snapshot_traces = step["recent_traces"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default();
                let snapshot_tool_calls = step["tool_calls"].as_u64().unwrap_or(0) as usize;
                let snapshot_note = step["latest_note"].as_str().unwrap_or("").to_owned();
                let snapshot_live_context = step["has_live_context"] == true;
                let file_trace = historical_step_trace(session_root, &intent_id);
                let win = self.intents.get(&intent_id);
                let live_running = win.map_or(step["running"] == true, |w| {
                    w.claim_seq > 0 && w.concluded_seq.is_none()
                });
                let worker = win
                    .and_then(|w| w.claim_solver_id.as_deref())
                    .or_else(|| step["worker_id"].as_str())
                    .or_else(|| file_trace["worker_id"].as_str())
                    .unwrap_or("")
                    .to_owned();
                let (live_traces, order_len, mut latest_note, online) = if let Some(w) = win {
                    let traces: Vec<Value> = w
                        .order
                        .iter()
                        .rev()
                        .take(15)
                        .rev()
                        .filter_map(|id| w.calls.get(id).cloned())
                        .collect();
                    (traces, w.order.len(), w.latest_note.clone(), w.claim_seq > 0 && w.online)
                } else {
                    (Vec::new(), 0, String::new(), false)
                };
                let pi_progress = if live_running {
                    historical_pi_progress(session_root, &worker)
                } else {
                    Value::Null
                };
                let file_traces = file_trace["recent_traces"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default();
                let pi_traces = pi_progress["recent_traces"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default();
                let observation_traces =
                    if live_traces.is_empty() && file_traces.is_empty() && snapshot_traces.is_empty()
                    {
                        historical_observation_traces(graph_events, session_root, &intent_id)
                    } else {
                        Vec::new()
                    };
                let traces = if !live_traces.is_empty() {
                    live_traces
                } else if live_running && !pi_traces.is_empty() {
                    pi_traces
                } else if !file_traces.is_empty() {
                    file_traces
                } else if !snapshot_traces.is_empty() {
                    snapshot_traces
                } else {
                    observation_traces.clone()
                };
                let tool_calls = order_len
                    .max(pi_progress["tool_calls"].as_u64().unwrap_or(0) as usize)
                    .max(file_trace["tool_calls"].as_u64().unwrap_or(0) as usize)
                    .max(snapshot_tool_calls)
                    .max(observation_traces.len())
                    .max(traces.len());
                if latest_note.is_empty() {
                    latest_note = pi_progress["latest_note"].as_str().unwrap_or("").to_owned();
                }
                if latest_note.is_empty() {
                    latest_note = snapshot_note;
                }
                step["running"] = json!(live_running);
                step["worker_id"] = json!(worker);
                step["tool_calls"] = json!(tool_calls);
                step["recent_traces"] = json!(traces);
                step["latest_note"] = json!(latest_note);
                let started_at = win
                    .and_then(|w| w.claim_ts)
                    .filter(|ts| *ts > 0.0)
                    .or_else(|| {
                        Some(pi_progress["started_ts"].as_f64().unwrap_or(0.0))
                            .filter(|ts| *ts > 0.0)
                    })
                    .or_else(|| {
                        Some(file_trace["started_at"].as_f64().unwrap_or(0.0))
                            .filter(|ts| *ts > 0.0)
                    });
                if let Some(started_at) = started_at {
                    step["started_at"] = json!(started_at);
                }
                if let Some(elapsed) = file_trace["elapsed_seconds"].as_f64().filter(|s| *s > 0.0) {
                    step["elapsed_seconds"] = json!(elapsed);
                }
                step["has_live_context"] = json!(
                    live_running
                        && (snapshot_live_context
                            || online
                            || tool_calls > 0
                            || !latest_note.is_empty())
                );
            }
        }
    }
}

pub fn apply_step_tracker_from_events(
    snapshot: &mut Value,
    session_root: &Path,
    graph_events: &[Value],
) {
    refresh_peek_snapshot(snapshot, graph_events, graph_events, session_root);
}

fn refresh_peek_snapshot(
    snapshot: &mut Value,
    events: &[Value],
    graph_events: &[Value],
    session_root: &Path,
) {
    let mut tracker = StepEventTracker::from_snapshot(snapshot, session_root);
    for event in events {
        tracker.observe(event);
    }
    tracker.apply(snapshot, graph_events, session_root);
}

const EXTENSION: &str = include_str!("planner_extension.ts");

#[derive(Clone)]
pub(crate) struct Selection {
    pub(crate) account_id: String,
    pub(crate) model: String,
    pub(crate) settings: Value,
    pub(crate) effort: Option<String>,
}

pub(crate) struct Reply {
    pub(crate) draft: Value,
    pub(crate) raw_response: String,
    pub(crate) exit_code: Option<i32>,
    pub(crate) stderr: String,
    pub(crate) tool_names: Vec<String>,
}

pub(crate) struct Failure {
    pub(crate) error_type: &'static str,
    pub(crate) detail: String,
    pub(crate) raw_response: String,
}

impl Failure {
    fn new(error_type: &'static str, detail: impl Into<String>, raw_response: String) -> Self {
        Self {
            error_type,
            detail: detail.into(),
            raw_response,
        }
    }
}

fn private_file(path: &std::path::Path, bytes: &[u8]) -> Result<(), Failure> {
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(path)
        .map_err(|error| Failure::new("PlannerRuntimeError", error.to_string(), String::new()))?;
    file.write_all(bytes)
        .and_then(|_| file.sync_all())
        .map_err(|error| Failure::new("PlannerRuntimeError", error.to_string(), String::new()))
}

struct ContainerCleanup {
    id: String,
    workspace: PathBuf,
    control_dir: PathBuf,
    accounts_dir: Option<PathBuf>,
    armed: bool,
}

impl ContainerCleanup {
    async fn finish(mut self) -> Result<(), Failure> {
        container_exec::teardown_run_container_checked(&self.id)
            .await
            .map_err(|error| Failure::new("PlannerRuntimeError", error, String::new()))?;
        self.remove_files();
        self.armed = false;
        Ok(())
    }

    fn remove_files(&self) {
        let _ = fs::remove_dir_all(&self.workspace);
        let _ = fs::remove_dir_all(&self.control_dir);
        if let Some(path) = &self.accounts_dir {
            let _ = fs::remove_dir_all(path);
        }
    }
}

impl Drop for ContainerCleanup {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let id = self.id.clone();
        let workspace = self.workspace.clone();
        let control_dir = self.control_dir.clone();
        let accounts_dir = self.accounts_dir.clone();
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                if container_exec::teardown_run_container_checked(&id)
                    .await
                    .is_ok()
                {
                    let _ = fs::remove_dir_all(workspace);
                    let _ = fs::remove_dir_all(control_dir);
                    if let Some(path) = accounts_dir {
                        let _ = fs::remove_dir_all(path);
                    }
                }
            });
        }
    }
}

pub(crate) async fn run_mode(
    state: Arc<AppState>,
    run_id: &str,
    source: &str,
    graph_snapshot: &Value,
    max_intents: usize,
    selection: Selection,
    workspace_root: PathBuf,
    host_workspace: Option<PathBuf>,
    mode: &str,
    system_prompt: &str,
    challenger: bool,
) -> Result<Reply, Failure> {
    let role = if challenger { "challenger" } else { "reason" };
    let native_id = format!("{mode}-{role}-{}", uuid::Uuid::new_v4().simple());
    let session_root = session_dir(&state, run_id);
    let workspace = workspace_root.join(&native_id);
    fs::create_dir_all(&workspace)
        .map_err(|error| Failure::new("PlannerRuntimeError", error.to_string(), String::new()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&workspace, fs::Permissions::from_mode(0o700)).map_err(|error| {
            Failure::new("PlannerRuntimeError", error.to_string(), String::new())
        })?;
    }
    let draft_path = workspace.join("draft.json");
    let graph_path = workspace.join("graph.json");
    let extension_path = workspace.join("pi_decide_extension.ts");
    private_file(&draft_path, br#"{"committed":false,"operations":[]}"#)?;
    let mut peek_snapshot = graph_snapshot.clone();
    let (mut peek_snapshot, mut step_tracker) = {
        let state = state.clone();
        let run_id = run_id.to_owned();
        let session_root = session_root.clone();
        let graph_path = graph_path.clone();
        tokio::task::spawn_blocking(move || -> Result<(Value, StepEventTracker), Failure> {
            let graph_events = SharedGraph::open(&state, &run_id)
                .map(|graph| graph.events())
                .unwrap_or_default();
            legacy_artifacts::materialize_for_peek(&state, &run_id, &peek_snapshot, &graph_events)
                .map_err(|error| Failure::new("PlannerRuntimeError", error, String::new()))?;
            let mut tracker = StepEventTracker::from_snapshot(&peek_snapshot, &session_root);
            tracker.sync_jsonl(&session_root);
            tracker.apply(&mut peek_snapshot, &graph_events, &session_root);
            private_file(&graph_path, peek_snapshot.to_string().as_bytes())?;
            Ok((peek_snapshot, tracker))
        })
        .await
        .map_err(|error| Failure::new("PlannerRuntimeError", error.to_string(), String::new()))??
    };
    fs::write(&extension_path, EXTENSION)
        .map_err(|error| Failure::new("PlannerRuntimeError", error.to_string(), String::new()))?;
    // Pi bundles TypeBox under its own installation. Node resolves the copied
    // source extension through this local package link inside the container.
    let node_modules = workspace.join("node_modules");
    fs::create_dir_all(&node_modules)
        .map_err(|error| Failure::new("PlannerRuntimeError", error.to_string(), String::new()))?;
    #[cfg(unix)]
    {
        let typebox = if selection.settings["worker_backend"] == "container" {
            PathBuf::from(
                "/usr/local/lib/node_modules/@earendil-works/pi-coding-agent/node_modules/typebox",
            )
        } else {
            local_typebox(&state)?
        };
        std::os::unix::fs::symlink(typebox, node_modules.join("typebox")).map_err(|error| {
            Failure::new("PlannerRuntimeError", error.to_string(), String::new())
        })?;
    }

    let registry = crate::plugins::registry(&state)
        .map_err(|error| Failure::new("PlannerPluginError", error.to_string(), String::new()))?;
    let native_components = registry
        .stage_worker_assets(&workspace, "pi", mode)
        .map_err(|error| Failure::new("PlannerPluginError", error.to_string(), String::new()))?;
    let bridge = super::planner_plugins::start(&state, run_id, mode)
        .await
        .map_err(|error| Failure::new("PlannerPluginError", error, String::new()))?;
    if peek_snapshot.is_object() {
        let mut merged_caps: std::collections::BTreeSet<String> = peek_snapshot["available_capabilities"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|item| item.as_str().map(str::trim).filter(|s| !s.is_empty()).map(str::to_owned))
            .collect();
        for tool in bridge.config["tools"].as_array().into_iter().flatten() {
            for field in ["name", "_tool"] {
                if let Some(value) = tool[field].as_str().map(str::trim).filter(|s| !s.is_empty()) {
                    merged_caps.insert(value.to_owned());
                }
            }
        }
        if !merged_caps.is_empty() || peek_snapshot.get("available_capabilities").is_some() {
            peek_snapshot["available_capabilities"] = json!(merged_caps.into_iter().collect::<Vec<_>>());
            fs::write(&graph_path, peek_snapshot.to_string()).map_err(|error| {
                Failure::new("PlannerRuntimeError", error.to_string(), String::new())
            })?;
        }
    }
    let plugin_path = workspace.join("plugins.json");
    let mut plugin_config = bridge.config.clone();
    if selection.settings["worker_backend"] != "container" {
        if let Some(url) = plugin_config["url"].as_str() {
            plugin_config["url"] = json!(url.replace("host.docker.internal", "127.0.0.1"));
        }
    }
    private_file(&plugin_path, plugin_config.to_string().as_bytes())?;
    if selection.settings["worker_backend"] != "container" {
        return run_local(
            state,
            run_id,
            source,
            max_intents,
            &selection,
            mode,
            system_prompt,
            challenger,
            role,
            &native_id,
            &workspace,
            &draft_path,
            &graph_path,
            &extension_path,
            &plugin_path,
            &native_components,
            &plugin_config,
            &session_root,
            &mut peek_snapshot,
            &mut step_tracker,
        )
        .await;
    }
    let mut settings = selection.settings.clone();
    // Planner invocations have their own private lifecycle even when solve Workers share a pool.
    settings["worker_container_scope"] = json!("run");
    // The planner exchanges draft, graph and extension files with Web through its bind-mounted
    // workspace, which a remote node cannot see; it stays on the local Runner.
    settings["worker_node_id"] = json!("local");

    if let Ok(Some(run)) = state.store.get("runs", run_id)
        && let Some(network) = run["worker_network"]
            .as_str()
            .filter(|value| !value.is_empty())
    {
        settings["worker_network"] = json!(network);
    }
    let host_workspace = host_workspace.as_deref().unwrap_or(&workspace);
    let handle = container_exec::ensure_run_container_owned(
        &state,
        &native_id,
        host_workspace,
        &settings,
        Some(run_id),
    )
    .await
    .map_err(|error| Failure::new("PlannerRuntimeError", error, String::new()))?;
    container_exec::register_run_container_owner(run_id, &native_id).await;
    let cleanup = ContainerCleanup {
        id: native_id.clone(),
        workspace,
        control_dir: handle.host_control_dir.clone(),
        accounts_dir: handle.host_accounts_dir.clone(),
        armed: true,
    };
    let result = async {
        let (mut env, _) = container_exec::prepare_container_worker_env(
            &state,
            &handle,
            role,
            "pi",
            &format!("account:{}", selection.account_id),
            Some(&selection.model),
            &[],
        )
        .await
        .map_err(|error| Failure::new("PlannerCredentialError", error.message, String::new()))?;
        env.insert(
            "MUTEKI_DECIDE_DRAFT_PATH".to_owned(),
            handle
                .to_container_path(&draft_path)
                .map_err(|error| Failure::new("PlannerRuntimeError", error, String::new()))?,
        );
        env.insert(
            "MUTEKI_DECIDE_GRAPH_PATH".to_owned(),
            handle
                .to_container_path(&graph_path)
                .map_err(|error| Failure::new("PlannerRuntimeError", error, String::new()))?,
        );
        env.insert(
            "MUTEKI_DECIDE_MAX_STEPS".to_owned(),
            if max_intents==usize::MAX {"-1".to_owned()} else {max_intents.to_string()},
        );
        env.insert("MUTEKI_DECIDE_CHALLENGER".to_owned(),
            if challenger { "1" } else { "0" }.to_owned());
        env.insert("MUTEKI_DECIDE_PLUGIN_PATH".into(),handle.to_container_path(&plugin_path).map_err(|error|Failure::new("PlannerPluginError",error,String::new()))?);
        let extension = handle
            .to_container_path(&extension_path)
            .map_err(|error| Failure::new("PlannerRuntimeError", error, String::new()))?;
        let mut argv:Vec<String> = vec![
            "pi",
            "-p",
            "--mode",
            "json",
            "--no-session",
            "--no-extensions",
            "--no-skills",
            "--no-context-files",
            "--no-builtin-tools",
            "--extension",
            &extension,
            "--tools",
            if challenger {
                "board_query,read,peek_step,supersede_fact,archive_directive,open_step,satisfy_goal,preview,commit"
            } else {
                "board_query,read,add_sub_goal,open_step,peek_step,drop_step,change_step_priority,supersede_fact,archive_directive,satisfy_goal,preview,commit"
            },
            "--provider",
            "muteki",
            "--model",
            &selection.model,
            "--system-prompt",
            system_prompt,
        ]
        .into_iter()
        .map(str::to_owned)
        .collect();
        if let Some(effort)=selection.effort.as_deref(){
            if !matches!(effort,"off"|"minimal"|"low"|"medium"|"high"|"xhigh"){return Err(Failure::new("PlannerConfigurationError","Unsupported Pi reasoning effort",String::new()));}
            argv.extend(["--thinking".into(),effort.into()]);
        }
        for skill in &native_components.skills {
            argv.extend([
                "--skill".into(),
                handle
                    .to_container_path(skill)
                    .map_err(|error| Failure::new("PlannerPluginError", error, String::new()))?,
            ]);
        }
        for extension in &native_components.extensions {
            argv.extend([
                "--extension".into(),
                handle
                    .to_container_path(extension)
                    .map_err(|error| Failure::new("PlannerPluginError", error, String::new()))?,
            ]);
        }
        if let Some(index)=argv.iter().position(|argument|argument=="--tools"){
            let tools=bridge.config["tools"].as_array().into_iter().flatten().filter_map(|tool|tool["name"].as_str()).collect::<Vec<_>>();
            if !tools.is_empty(){argv[index+1]=format!("{},{}",argv[index+1],tools.join(","));}
        }
        let generation=state.store.get("runs",run_id).ok().flatten().and_then(|run|run["execution_generation"].as_u64()).unwrap_or(1);
        let usage_spec=super::worker::WorkerSpec{solver_id:format!("{role}-g{generation}"),profile_id:"planner".into(),profile_label:role.into(),raw_engine:"cli-pi".into(),norm_engine:"pi".into(),adapter_id:"pi.rpc".into(),instance_id:"default".into(),model:Some(selection.model.clone()),effort:selection.effort.clone(),account_id:selection.account_id.clone(),effective_credential_id:format!("account:{}",selection.account_id),backend:"container".into(),profile:settings["llm_profiles"]["planner"].clone()};
        let mut usage=super::usage::Observer::new(&native_id,role);
        let call_limit=super::execution_budget::limit(&state,run_id,"execute",Duration::from_secs(300)).map_err(|error|Failure::new("PlannerBudgetExpired",error,String::new()))?;
        let tag = format!("{mode}-{role}");
        let node = crate::runner_remote::controller::selected(&settings).unwrap_or("local").to_owned();
        let reservation = crate::runner_resources::admit_wait(&state, &native_id, &tag, &node, call_limit, ||{}).await
            .map_err(|error| Failure::new("PlannerAdmissionTimeout", error, String::new()))?;
        let spec = WorkerSpec {
            reservation_id:Some(reservation.id().to_owned()),
            owner_run_id: None,
            owner_token: None,
            owner_workspace: None,
            argv,
            cwd: handle.container_workspace.clone(),
            stdin: Some(source.to_owned()),
            stdin_mode: None,
            env,
            timeout_sec: call_limit.as_secs().max(1) as i64,
            tag: Some(tag),
            output_limit_bytes: handle.limits.output_limit_bytes,
            disk_limit_bytes: handle.limits.disk_limit_bytes,
        };
        let (_worker_id, mut frames) = handle
            .link
            .start_worker(&spec)
            .await
            .map_err(|error| Failure::new("PlannerRuntimeError", error, String::new()))?;
        let mut assistant = Vec::new();
        let mut errors = Vec::new();
        let mut tool_names = Vec::new();
        let mut exit_code = None;
        let deadline = Instant::now() + call_limit;
        let mut last_peek_refresh = Instant::now();
        let mut committed_at:Option<Instant>=None;
        while Instant::now() < deadline {
            let committed=fs::read(&draft_path).ok().and_then(|bytes|serde_json::from_slice::<Value>(&bytes).ok()).is_some_and(|draft|draft["committed"]==true && draft["operations"].is_array());
            if committed && committed_at.is_none(){committed_at=Some(Instant::now());}
            if committed_at.is_some_and(|at|at.elapsed()>=Duration::from_secs(5)){break;}

            if last_peek_refresh.elapsed() >= Duration::from_secs(1) {
                let s = state.clone();
                let rid = run_id.to_owned();
                let sroot = session_root.clone();
                let gpath = graph_path.clone();
                let mut snap = std::mem::take(&mut peek_snapshot);
                let mut trk = std::mem::take(&mut step_tracker);
                if let Ok((next_snap, next_trk)) = tokio::task::spawn_blocking(move || {
                    let graph_events = SharedGraph::open(&s, &rid)
                        .map(|graph| graph.events())
                        .unwrap_or_default();
                    trk.sync_jsonl(&sroot);
                    trk.apply(&mut snap, &graph_events, &sroot);
                    // Docker Desktop keeps the original bind-mounted inode visible to
                    // Pi; replacing it from the Mac host can leave the container with
                    // an unlinked graph.json. Refresh the mounted file in place.
                    let _ = fs::write(&gpath, snap.to_string());
                    (snap, trk)
                })
                .await
                {
                    peek_snapshot = next_snap;
                    step_tracker = next_trk;
                }
                last_peek_refresh = Instant::now();
            }
            let Some(frame) = tokio::time::timeout(Duration::from_secs(1), frames.recv())
                .await
                .ok()
                .flatten()
            else {
                continue;
            };
            match frame.t.as_str() {
                "transport_error"=>{errors.push("Planner transport was lost; process outcome is unknown".into());break;}
                "out" => {
                    usage.container(&state,run_id,&usage_spec,&frame.line).map_err(|error|Failure::new("PlannerUsageError",error,String::new()))?;
                    if let Ok(event) = serde_json::from_str::<Value>(&frame.line)
                        && event["type"] == "tool_execution_start"
                        && let Some(name) = event["toolName"].as_str()
                    {
                        tool_names.push(name.to_owned());
                    }
                    if let Some(event) = container_exec::parse_container_cli_json_line(&frame.line)
                    {
                        assistant.extend(event.assistant_texts);
                        if let Some(error) = event.error_message {
                            errors.push(error);
                        }
                    }
                }
                "err" => {
                    if !frame.line.trim().is_empty() {
                        errors.push(frame.line);
                    }
                }
                "exit" => {
                    exit_code = Some(frame.rc);
                    break;
                }
                _ => {}
            }
        }
        let raw_response = assistant.join("\n");
        let draft: Value = fs::read_to_string(&draft_path)
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .ok_or_else(|| {
                Failure::new(
                    "ReportFormatError",
                    "Pi Decide 草稿无法读取",
                    raw_response.clone(),
                )
            })?;
        if draft["committed"] != true || !draft["operations"].is_array() {
            if exit_code.is_none(){return Err(Failure::new("PlannerTimeoutError","Pi Decide exceeded its deadline without a durable commit",raw_response));}

            let detail = if errors.is_empty() {
                "Pi did not commit a valid planner draft".to_owned()
            } else {
                errors.join("\n").chars().take(8192).collect()
            };
            return Err(Failure::new(
                "ReportFormatError",
                format!("{detail}; tools={}", tool_names.join(",")),
                raw_response,
            ));
        }
        Ok(Reply {
            draft,
            raw_response,
            exit_code,
            stderr: errors.join("\n"),
            tool_names,
        })
    }
    .await;
    cleanup.finish().await?;
    result
}

fn local_typebox(state: &AppState) -> Result<PathBuf, Failure> {
    let instance =
        crate::runtime_api::instance(&state.store, "pi.rpc", "default").unwrap_or(Value::Null);
    let binary = instance["binary_path"]
        .as_str()
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("PATH").and_then(|paths| {
                std::env::split_paths(&paths)
                    .map(|path| path.join("pi"))
                    .find(|path| path.is_file())
            })
        })
        .ok_or_else(|| {
            Failure::new(
                "PlannerRuntimeError",
                "Pi binary was not found",
                String::new(),
            )
        })?;
    let binary = fs::canonicalize(binary)
        .map_err(|error| Failure::new("PlannerRuntimeError", error.to_string(), String::new()))?;
    for ancestor in binary.ancestors().skip(1) {
        for relative in [
            "node_modules/typebox",
            "node_modules/@earendil-works/pi-coding-agent/node_modules/typebox",
            "lib/node_modules/@earendil-works/pi-coding-agent/node_modules/typebox",
        ] {
            let candidate = ancestor.join(relative);
            if candidate.join("package.json").is_file() {
                return Ok(candidate);
            }
        }
    }
    Err(Failure::new(
        "PlannerRuntimeError",
        "The installed Pi package has no resolvable TypeBox dependency",
        String::new(),
    ))
}
struct LocalCleanup {
    state: Arc<AppState>,
    thread: String,
    workspace: PathBuf,
}
impl Drop for LocalCleanup {
    fn drop(&mut self) {
        let (state, thread, path) = (
            self.state.clone(),
            self.thread.clone(),
            self.workspace.clone(),
        );
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                state.agents.close(&thread).await;
                let _ = fs::remove_dir_all(path);
            });
        }
    }
}
#[allow(clippy::too_many_arguments)]
async fn run_local(
    state: Arc<AppState>,
    run_id: &str,
    source: &str,
    max_intents: usize,
    selection: &Selection,
    mode: &str,
    system_prompt: &str,
    challenger: bool,
    role: &str,
    id: &str,
    workspace: &Path,
    draft_path: &Path,
    graph_path: &Path,
    extension_path: &Path,
    plugin_path: &Path,
    native_components: &crate::plugins::StagedWorkerComponents,
    plugin_config: &Value,
    session_root: &Path,
    peek_snapshot: &mut Value,
    step_tracker: &mut StepEventTracker,
) -> Result<Reply, Failure> {
    use agent::{AgentEvent, DeltaKind, ItemContent, SessionCommand};
    let _cleanup = LocalCleanup {
        state: state.clone(),
        thread: id.into(),
        workspace: workspace.into(),
    };
    let fail = |detail: String| Failure::new("PlannerRuntimeError", detail, String::new());
    let descriptor = crate::agent_runtime::descriptors::find("pi")
        .ok_or_else(|| fail("Pi descriptor missing".into()))?;
    let credential_id = format!("account:{}", selection.account_id);
    let credential = credentials::resolve(
        &state.store,
        &native_chat::state_dir(&state),
        descriptor,
        &credential_id,
        Some(&selection.model),
    )
    .await
    .map_err(|error| fail(error.message))?;
    let instance =
        crate::runtime_api::instance(&state.store, "pi.rpc", "default").unwrap_or(Value::Null);
    let mut env = credential.env;
    env.extend(
        credentials::env_refs_with_platform(&state, &instance["env_refs"])
            .map_err(|error| fail(error.message))?,
    );
    env.extend(
        instance["env"]
            .as_object()
            .into_iter()
            .flatten()
            .filter_map(|(key, value)| Some((key.clone(), value.as_str()?.into()))),
    );
    for (key, value) in [
        (
            "MUTEKI_DECIDE_DRAFT_PATH",
            draft_path.to_string_lossy().into_owned(),
        ),
        (
            "MUTEKI_DECIDE_GRAPH_PATH",
            graph_path.to_string_lossy().into_owned(),
        ),
        (
            "MUTEKI_DECIDE_PLUGIN_PATH",
            plugin_path.to_string_lossy().into_owned(),
        ),
        (
            "MUTEKI_DECIDE_MAX_STEPS",
            if max_intents == usize::MAX {
                "-1".to_owned()
            } else {
                max_intents.to_string()
            },
        ),
        (
            "MUTEKI_DECIDE_CHALLENGER",
            if challenger { "1" } else { "0" }.into(),
        ),
    ] {
        env.push((key.into(), value));
    }
    let mut args = instance["launch_args"]
        .as_array()
        .into_iter()
        .flatten()
        .chain(instance["extra_args"].as_array().into_iter().flatten())
        .filter_map(Value::as_str)
        .map(str::to_owned)
        .collect::<Vec<_>>();
    args.extend([
        "--no-session".into(),
        "--no-extensions".into(),
        "--no-skills".into(),
        "--no-context-files".into(),
        "--no-builtin-tools".into(),
        "--extension".into(),
        extension_path.to_string_lossy().into_owned(),
        "--system-prompt".into(),
        system_prompt.into(),
    ]);
    for skill in &native_components.skills {
        args.extend([
            "--skill".into(),
            skill.to_string_lossy().into_owned(),
        ]);
    }
    for extension in &native_components.extensions {
        args.extend([
            "--extension".into(),
            extension.to_string_lossy().into_owned(),
        ]);
    }
    let mut tools = vec![
        "board_query",
        "read",
        "open_step",
        "peek_step",
        "supersede_fact",
        "archive_directive",
        "satisfy_goal",
        "preview",
        "commit",
    ];
    if !challenger {
        tools.extend(["add_sub_goal", "drop_step", "change_step_priority"]);
    }
    tools.extend(
        plugin_config["tools"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|tool| tool["name"].as_str()),
    );
    args.extend(["--tools".into(), tools.join(",")]);
    let request = crate::agent_runtime::StartRequest {
        thread_id: id.into(),
        adapter_id: "pi.rpc".into(),
        instance_id: "default".into(),
        model: Some(selection.model.clone()),
        access_mode: crate::agent_runtime::access::FULL_ACCESS.into(),
        credential_id: credential_id.clone(),
        cwd: session_root.into(),
        binary_override: instance["binary_path"]
            .as_str()
            .filter(|v| !v.is_empty())
            .map(PathBuf::from),
        env,
        home: None,
        extra_args: args,
        resume: None,
        fork: false,
        instructions: None,
        mcp_servers: Vec::new(),
    };
    let call_limit =
        super::execution_budget::limit(&state, run_id, "execute", Duration::from_secs(300))
            .map_err(fail)?;
    let deadline = tokio::time::Instant::now() + call_limit;
    let (session, events) = tokio::time::timeout_at(deadline, state.agents.start(request))
        .await
        .map_err(|_| fail("Pi startup exceeded the execution deadline".into()))?
        .map_err(|error| fail(error.message))?;
    session
        .send(SessionCommand::SendTurn {
            delivery_id: 1,
            text: source.into(),
            options: Some(agent::TurnOptions {
                effort: selection.effort.clone(),
                ..Default::default()
            }),
            attachments: Vec::new(),
        })
        .await
        .map_err(|error| fail(error.message))?;
    let generation = state
        .store
        .get("runs", run_id)
        .ok()
        .flatten()
        .and_then(|run| run["execution_generation"].as_u64())
        .unwrap_or(1);
    let spec = super::worker::WorkerSpec {
        solver_id: format!("{role}-g{generation}"),
        profile_id: "planner".into(),
        profile_label: role.into(),
        raw_engine: "cli-pi".into(),
        norm_engine: "pi".into(),
        adapter_id: "pi.rpc".into(),
        instance_id: "default".into(),
        model: Some(selection.model.clone()),
        effort: selection.effort.clone(),
        account_id: selection.account_id.clone(),
        effective_credential_id: credential_id,
        backend: "local".into(),
        profile: selection.settings["llm_profiles"]["planner"].clone(),
    };
    let mut usage = super::usage::Observer::new(id, role);
    let mut raw = String::new();
    let mut errors = Vec::new();
    let mut tool_names = Vec::new();
    let mut seen_tools = std::collections::HashSet::new();
    let mut committed_at: Option<Instant> = None;
    let mut last_peek_refresh = Instant::now();
    let mut tick = tokio::time::interval(Duration::from_millis(250));
    loop {
        tokio::select! {
            _=tokio::time::sleep_until(deadline)=>break,
            _=tick.tick()=>{
                let draft=fs::read(draft_path).ok().and_then(|bytes|serde_json::from_slice::<Value>(&bytes).ok());
                if draft.as_ref().is_some_and(|draft|draft["committed"]==true && draft["operations"].is_array()){if committed_at.is_none(){committed_at=Some(Instant::now());}if committed_at.is_some_and(|at|at.elapsed()>=Duration::from_secs(5)){break;}}
                if last_peek_refresh.elapsed() >= Duration::from_secs(1) {
                    let s = state.clone();
                    let rid = run_id.to_owned();
                    let sroot = session_root.to_path_buf();
                    let gpath = graph_path.to_path_buf();
                    let mut snap = std::mem::take(peek_snapshot);
                    let mut trk = std::mem::take(step_tracker);
                    let (next_snap, next_trk, write_res) = tokio::task::spawn_blocking(move || {
                        let graph_events = SharedGraph::open(&s, &rid)
                            .map(|graph| graph.events())
                            .unwrap_or_default();
                        trk.sync_jsonl(&sroot);
                        trk.apply(&mut snap, &graph_events, &sroot);
                        let res = fs::write(&gpath, snap.to_string()).map_err(|error| error.to_string());
                        (snap, trk, res)
                    })
                    .await
                    .map_err(|error| fail(error.to_string()))?;
                    *peek_snapshot = next_snap;
                    *step_tracker = next_trk;
                    write_res.map_err(fail)?;
                    last_peek_refresh = Instant::now();
                }
            },
            event=events.recv()=>{let event=event.map_err(|_|fail("Pi session closed".into()))?;session.observe(&event);usage.observe(&state,run_id,&spec,&event).map_err(fail)?;let Some(event)=event.into_root_event()else{continue};match event{
                AgentEvent::Delta{kind:DeltaKind::AssistantText,text,..}=>raw.push_str(&text),
                AgentEvent::ItemStarted(agent::ThreadItem{id,content:ItemContent::ToolCall{name,input,output,..},..})
                |AgentEvent::ItemUpdated(agent::ThreadItem{id,content:ItemContent::ToolCall{name,input,output,..},..})
                |AgentEvent::ItemCompleted(agent::ThreadItem{id,content:ItemContent::ToolCall{name,input,output,..},..})=>{
                    if seen_tools.insert(id.clone()){tool_names.push(name.clone());}
                    if let Some(output)=output{super::emit_event(&state,run_id,Some(role),"planner.tool",json!({"call_id":id,"tool":name,"input":input,"output":output}));}
                },
                AgentEvent::TurnCompleted{..}=>break,
                AgentEvent::TurnFailed{message,..}|AgentEvent::Error{message,fatal:true}=>{errors.push(message);break;},
                AgentEvent::ProviderStartFailed{error}=>{errors.push(error);break;},_=>{}
            }}
        }
    }
    let draft = fs::read(draft_path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
        .filter(|draft| draft["committed"] == true && draft["operations"].is_array())
        .ok_or_else(|| {
            Failure::new(
                "ReportFormatError",
                errors
                    .last()
                    .cloned()
                    .unwrap_or_else(|| "Pi exited without a durable planner commit".into()),
                raw.clone(),
            )
        })?;
    state.agents.close(id).await;
    let _ = mode;
    Ok(Reply {
        draft,
        raw_response: raw,
        exit_code: None,
        stderr: errors.join("\n"),
        tool_names,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Store;

    #[test]
    fn peek_snapshot_uses_persisted_worker_command_and_output() {
        let directory = tempfile::tempdir().unwrap();
        let mut snapshot = json!({"open_steps":[{"id":"step_1","running":true}],
            "done_steps":[]});
        let events = vec![
            json!({"seq":1,"event_type":"blackboard.delta","solver_id":"pi#1",
                "payload":{"kind":"intent_claimed","intent_id":"step_1"}}),
            json!({"seq":2,"event_type":"worker.status","solver_id":"pi#1",
                "payload":{"online":true}}),
            json!({"seq":3,"event_type":"tool.start","solver_id":"pi#1",
                "payload":{"call_id":"call-1","tool":"curl /login.php"}}),
            json!({"seq":4,"event_type":"tool.result","solver_id":"pi#1",
                "payload":{"call_id":"call-1","tool":"curl",
                    "result":{"condensed":"HTTP 200"}}}),
        ];
        refresh_peek_snapshot(&mut snapshot, &events, &[], directory.path());
        let step = &snapshot["open_steps"][0];
        assert_eq!(step["tool_calls"], 1);
        assert_eq!(step["recent_traces"][0]["output"], "HTTP 200");
        assert_eq!(step["has_live_context"], true);
    }

    #[test]
    fn peek_snapshot_recovers_done_step_from_persisted_trace_and_observation_artifact() {
        let directory = tempfile::tempdir().unwrap();
        let trace_dir = directory.path().join(".muteki-agent-state/step_traces");
        fs::create_dir_all(&trace_dir).unwrap();
        fs::write(
            trace_dir.join("step_1.json"),
            json!({
                "worker_id":"pi#1","tool_calls":2,"elapsed_seconds":17.0,
                "recent_traces":[{"tool":"bash","command":"curl /login.php",
                    "output":"HTTP 200 from saved Step trace"}]
            })
            .to_string(),
        )
        .unwrap();
        let arts_dir = directory.path().join("arts");
        fs::create_dir_all(&arts_dir).unwrap();
        fs::write(
            arts_dir.join("art-3.json"),
            json!({
                "artifact_id":"art-3","command":"curl /",
                "content":"HTTP 302 Found from saved Observation artifact"
            })
            .to_string(),
        )
        .unwrap();
        fs::write(
            arts_dir.join("abcdef123456.txt"),
            "HTTP 404 from source ArtifactStore",
        )
        .unwrap();
        let observations = vec![
            json!({"kind":"observation_added","payload":{
            "intent_id":"step_2","evidence_provenance":{"artifact_refs":[{
                "artifact_id":"art-3","command":"curl /"}]}}}),
            json!({"kind":"observation_added","payload":{
                "intent_id":"step_3","evidence_provenance":{"artifact_refs":[{
                    "artifact_id":"abcdef123456","command":"curl /missing"}]}}}),
        ];
        let mut snapshot = json!({"open_steps":[],"done_steps":[
            {"id":"step_1","running":false},
            {"id":"step_2","running":false},
            {"id":"step_3","running":false}
        ]});
        refresh_peek_snapshot(&mut snapshot, &[], &observations, directory.path());
        let saved = &snapshot["done_steps"][0];
        assert_eq!(saved["worker_id"], "pi#1");
        assert_eq!(saved["tool_calls"], 2);
        assert_eq!(saved["elapsed_seconds"], 17.0);
        assert_eq!(
            saved["recent_traces"][0]["output"],
            "HTTP 200 from saved Step trace"
        );
        let observed = &snapshot["done_steps"][1];
        assert_eq!(observed["tool_calls"], 1);
        assert_eq!(observed["recent_traces"][0]["command"], "curl /");
        assert_eq!(
            observed["recent_traces"][0]["output"],
            "HTTP 302 Found from saved Observation artifact"
        );
        assert_eq!(
            snapshot["done_steps"][2]["recent_traces"][0]["output"],
            "HTTP 404 from source ArtifactStore"
        );
    }

    #[test]
    fn peek_snapshot_recovers_running_pi_session_when_run_events_are_missing() {
        let directory = tempfile::tempdir().unwrap();
        let sessions = directory.path().join(".muteki-agent-state/pi#1/sessions");
        fs::create_dir_all(&sessions).unwrap();
        let rows = [
            json!({"type":"message","timestamp":"2026-11-01T00:00:00Z",
                "message":{"role":"assistant","content":[
                    {"type":"text","text":"Checking saved response"},
                    {"type":"toolCall","id":"call-1","name":"bash",
                        "arguments":{"command":"curl /login.php"}}]}}),
            json!({"type":"message","message":{"role":"toolResult",
                "toolCallId":"call-1","content":[{"type":"text","text":"HTTP 200"}]}}),
        ];
        fs::write(
            sessions.join("session.jsonl"),
            rows.iter()
                .map(|row| format!("{row}\n"))
                .collect::<String>(),
        )
        .unwrap();
        let mut snapshot = json!({"open_steps":[{"id":"step_1","running":true,
            "worker_id":"pi#1"}],"done_steps":[]});
        refresh_peek_snapshot(&mut snapshot, &[], &[], directory.path());
        let step = &snapshot["open_steps"][0];
        assert_eq!(step["tool_calls"], 1);
        assert_eq!(step["recent_traces"][0]["command"], "curl /login.php");
        assert_eq!(step["recent_traces"][0]["output"], "HTTP 200");
        assert_eq!(step["latest_note"], "Checking saved response");
        assert_eq!(step["has_live_context"], true);
    }

    #[cfg(unix)]
    #[test]
    fn peek_snapshot_does_not_read_artifacts_symlinked_outside_the_run() {
        let directory = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let arts = directory.path().join("arts");
        fs::create_dir_all(&arts).unwrap();
        let private = outside.path().join("private.txt");
        fs::write(&private, "outside run content").unwrap();
        std::os::unix::fs::symlink(&private, arts.join("abcdef654321.txt")).unwrap();
        std::os::unix::fs::symlink(&private, arts.join("art-5.json")).unwrap();
        assert!(historical_artifact_output(directory.path(), "abcdef654321").is_empty());
        assert!(historical_artifact_output(directory.path(), "art-5").is_empty());
    }

    #[test]
    #[ignore = "requires MUTEKI_LEGACY_PENTEST_SOURCE and MUTEKI_LEGACY_PENTEST_RUN"]
    fn imported_source_artifact_bytes_reach_done_step_peek() {
        let source = PathBuf::from(std::env::var("MUTEKI_LEGACY_PENTEST_SOURCE").unwrap());
        let run_id = std::env::var("MUTEKI_LEGACY_PENTEST_RUN").unwrap();
        let directory = tempfile::tempdir().unwrap();
        let fixture = directory.path().join("source");
        let local = directory.path().join("rust");
        let source_arts = source.join("sessions").join(&run_id).join("workspace/arts");
        let fixture_arts = fixture
            .join("sessions")
            .join(&run_id)
            .join("workspace/arts");
        fs::create_dir_all(&fixture_arts).unwrap();
        for entry in fs::read_dir(&source_arts).unwrap() {
            let entry = entry.unwrap();
            if entry.file_type().unwrap().is_file() {
                fs::copy(entry.path(), fixture_arts.join(entry.file_name())).unwrap();
            }
        }
        let fixture_state = fixture.join("state");
        fs::create_dir_all(&fixture_state).unwrap();
        fs::copy(
            source.join("state").join(format!("{run_id}.jsonl")),
            fixture_state.join(format!("{run_id}.jsonl")),
        )
        .unwrap();
        let fixture_graph = fixture_state.join("control/graphs").join(&run_id);
        fs::create_dir_all(&fixture_graph).unwrap();
        fs::copy(
            source
                .join("state/control/graphs")
                .join(&run_id)
                .join("shared_graph.db"),
            fixture_graph.join("shared_graph.db"),
        )
        .unwrap();
        let platform_dir = fixture_state.join("_platform");
        fs::create_dir_all(&platform_dir).unwrap();
        let platform = rusqlite::Connection::open(platform_dir.join("platform.db")).unwrap();
        for table in [
            "artifacts",
            "projects",
            "workspaces",
            "threads",
            "tasks",
            "src_records",
        ] {
            platform
                .execute(&format!("CREATE TABLE {table} (id TEXT)"), [])
                .unwrap();
        }
        let competition = rusqlite::Connection::open(platform_dir.join("competition.db")).unwrap();
        for table in ["competitions", "artifact_objects"] {
            competition
                .execute(&format!("CREATE TABLE {table} (id TEXT)"), [])
                .unwrap();
        }
        fs::create_dir_all(&local).unwrap();
        let store = Store::open(local.join("muteki.sqlite")).unwrap();
        store.put("system", "state_dir", &json!(&local)).unwrap();
        let imported = crate::history_import::run(&fixture, &local, &store).unwrap();
        assert_eq!(imported.runs, 1);
        let state = AppState::new(store);
        let graph_path = local
            .join("legacy/graphs")
            .join(&run_id)
            .join("shared_graph.db");
        let graph = crate::history_import::read_db(&graph_path).unwrap();
        let graph_events = crate::history_import::table_rows(&graph, "events").unwrap();
        let (observation, artifact) = graph_events
            .iter()
            .find_map(|event| {
                if event["kind"] != "observation_added" {
                    return None;
                }
                let intent = event["payload"]["intent_id"].as_str()?;
                if intent.is_empty() {
                    return None;
                }
                for reference in event["payload"]["evidence_provenance"]["artifact_refs"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .rev()
                {
                    let id = reference["artifact_id"].as_str()?;
                    let path = fs::read_dir(&fixture_arts)
                        .ok()?
                        .flatten()
                        .map(|entry| entry.path())
                        .find(|path| {
                            path.file_name().is_some_and(|name| {
                                name.to_string_lossy()
                                    .strip_prefix(id)
                                    .is_some_and(|suffix| suffix.starts_with('.'))
                            })
                        });
                    if let Some(path) = path {
                        return Some((event, path));
                    }
                }
                None
            })
            .expect("source graph must have an Observation with a persisted artifact");
        let intent = observation["payload"]["intent_id"].as_str().unwrap();
        let filename = artifact.file_name().unwrap();
        let expected_bytes = fs::read(&artifact).unwrap();
        let expected_output = compact_trace(std::str::from_utf8(&expected_bytes).unwrap(), 420);
        let mut snapshot = json!({"open_steps":[],"done_steps":[{"id":intent,"running":false}]});
        assert!(
            legacy_artifacts::materialize_for_peek(&state, &run_id, &snapshot, &graph_events)
                .unwrap()
                > 0
        );
        let session = session_dir(&state, &run_id);
        assert_eq!(
            fs::read(session.join("arts").join(filename)).unwrap(),
            expected_bytes
        );
        refresh_peek_snapshot(&mut snapshot, &[], &graph_events, &session);
        assert!(
            snapshot["done_steps"][0]["recent_traces"]
                .as_array()
                .unwrap()
                .iter()
                .any(|trace| trace["output"] == expected_output)
        );
        assert!(snapshot["done_steps"][0]["tool_calls"].as_u64().unwrap() >= 1);
    }
}
