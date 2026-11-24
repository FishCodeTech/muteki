use crate::AppState;
use crate::store::Store;
use axum::{
    Json,
    http::{HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};
use flate2::{Compression, write::GzEncoder};
use regex::Regex;
use rusqlite::{Connection, OptionalExtension, params, params_from_iter};
use serde_json::{Map, Value, json};
use std::{
    collections::{BTreeMap, BTreeSet, HashSet},
    fmt,
    io::{Cursor, Read, Seek, SeekFrom},
    path::Path,
    sync::{Arc, OnceLock},
    time::{SystemTime, UNIX_EPOCH},
};

const SCHEMA_VERSION: &str = "muteki-src-debug-bundle.v1";
const TRAFFIC_BODY_CHAR_LIMIT: usize = 4096;
const LOG_TAIL_LINES: usize = 2000;
const MAX_LOG_READ_BYTES: u64 = 2 * 1024 * 1024;
const EVENT_QUERY_CHUNK_SIZE: usize = 500;

pub struct DebugArchive {
    pub filename: String,
    pub bytes: Vec<u8>,
    pub metadata: Value,
    pub summary: Value,
}

#[derive(Debug)]
pub enum DebugExportError {
    InvalidTaskId(String),
    NotFound(String),
    Internal(String),
}

impl fmt::Display for DebugExportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidTaskId(msg) | Self::NotFound(msg) | Self::Internal(msg) => {
                f.write_str(msg)
            }
        }
    }
}

impl std::error::Error for DebugExportError {}

struct RawDocument {
    collection: String,
    id: String,
    value: Value,
}

struct ChatEventRow {
    seq: i64,
    thread_id: String,
    turn_id: String,
    stream_seq: i64,
    event_id: String,
    event_type: String,
    payload: String,
}

struct SnapshotData {
    target_task: Value,
    is_historical: bool,
    src_docs: Vec<RawDocument>,
    thread_docs: Vec<(String, Value)>,
    related_docs: Vec<RawDocument>,
    matched_src: Vec<bool>,
    thread_ids: BTreeSet<String>,
    run_ids: BTreeSet<String>,
    command_ids: BTreeSet<String>,
    chat_events: Vec<ChatEventRow>,
}

fn secret_key_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?i)(cookie|authorization|api[_-]?key|token|secret|password|passwd|private[_-]?key|credential|x-csrf|x-auth|session[_-]?id|totp_seed|proxy_password)")
            .expect("secret key regex")
    })
}

fn bearer_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?i)(bearer\s+)[A-Za-z0-9._~+/=-]{8,}").expect("bearer regex"))
}

fn sk_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\bsk-[A-Za-z0-9_-]{12,}\b").expect("sk regex"))
}

fn github_token_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"\b(?:ghp|gho|ghu|ghs|ghr|github_pat)_[A-Za-z0-9_]{12,}\b")
            .expect("github token regex")
    })
}

fn cookie_text_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r#"(?i)(cookie\s*[:=]\s*)[^\r\n;"']{6,}"#).expect("cookie regex"))
}

fn auth_text_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?i)(authorization\s*[:=]\s*(?:basic|digest|token|apikey)\s+)[A-Za-z0-9._~+/=-]{8,}")
            .expect("auth header regex")
    })
}

fn private_key_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"-----BEGIN [A-Z0-9 ]*PRIVATE KEY-----[\s\S]*?-----END [A-Z0-9 ]*PRIVATE KEY-----")
            .expect("private key regex")
    })
}

fn url_userinfo_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?i)\b((?:https?|socks5h?|wss?)://)([^:/@\s]+):([^@\s]+)@")
            .expect("url userinfo regex")
    })
}

fn is_benign_metric_or_ref_key(lower: &str) -> bool {
    matches!(
        lower,
        "credential_id"
            | "credential_ref"
            | "credential_scope"
            | "token_count"
            | "token_usage"
            | "tokens"
            | "max_tokens"
            | "max_output_tokens"
            | "max_input_tokens"
            | "context_tokens"
            | "context_window_tokens"
    ) || lower.ends_with("_tokens")
        || lower.ends_with("_token_count")
}

fn is_secret_header_name(name: &str) -> bool {
    matches!(
        name.trim().to_ascii_lowercase().as_str(),
        "authorization"
            | "proxy-authorization"
            | "cookie"
            | "set-cookie"
            | "x-api-key"
            | "x-auth-token"
            | "x-csrf-token"
    )
}

fn is_large_text_key(lower: &str) -> bool {
    matches!(
        lower,
        "body" | "raw" | "request_body" | "response_body" | "html" | "content"
    )
}

pub fn redact_text(input: &str, max_chars: Option<usize>) -> String {
    let out = private_key_regex().replace_all(input, "<REDACTED_PRIVATE_KEY>");
    let out = bearer_regex().replace_all(&out, "${1}<REDACTED>");
    let out = auth_text_regex().replace_all(&out, "${1}<REDACTED>");
    let out = sk_regex().replace_all(&out, "<REDACTED_KEY>");
    let out = github_token_regex().replace_all(&out, "<REDACTED_KEY>");
    let out = cookie_text_regex().replace_all(&out, "${1}<REDACTED>");
    let out = url_userinfo_regex().replace_all(&out, "${1}***:***@");
    if let Some(limit) = max_chars
        && out.len() > limit
    {
        let mut cut = None;
        let mut char_count = 0usize;
        for (idx, _) in out.char_indices() {
            if char_count == limit {
                cut = Some(idx);
            }
            char_count += 1;
        }
        if let Some(byte_idx) = cut {
            return format!("{}...<truncated:{}>", &out[..byte_idx], char_count);
        }
    }
    out.into_owned()
}

pub fn scrub_value(value: &Value, cap_large: bool) -> Value {
    scrub_inner(value, cap_large, "")
}

fn scrub_inner(value: &Value, cap_large: bool, parent_key: &str) -> Value {
    match value {
        Value::Object(map) => {
            let mut out = Map::new();
            for (k, v) in map {
                let lower = k.to_ascii_lowercase();
                if secret_key_regex().is_match(k) && !is_benign_metric_or_ref_key(&lower) {
                    if matches!(v, Value::Number(_) | Value::Bool(_) | Value::Null) {
                        out.insert(k.clone(), v.clone());
                    } else {
                        out.insert(k.clone(), Value::String("<REDACTED>".to_owned()));
                    }
                } else {
                    out.insert(k.clone(), scrub_inner(v, cap_large, k));
                }
            }
            Value::Object(out)
        }
        Value::Array(items) => {
            if items.len() == 2 && items[0].as_str().is_some_and(is_secret_header_name) {
                Value::Array(vec![
                    scrub_inner(&items[0], cap_large, parent_key),
                    Value::String("<REDACTED>".to_owned()),
                ])
            } else {
                Value::Array(
                    items
                        .iter()
                        .map(|item| scrub_inner(item, cap_large, parent_key))
                        .collect(),
                )
            }
        }
        Value::String(text) => {
            let limit = if cap_large && is_large_text_key(&parent_key.to_ascii_lowercase()) {
                Some(TRAFFIC_BODY_CHAR_LIMIT)
            } else {
                None
            };
            Value::String(redact_text(text, limit))
        }
        _ => value.clone(),
    }
}

fn insert_non_empty(set: &mut BTreeSet<String>, value: &str) -> bool {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        false
    } else {
        set.insert(trimmed.to_owned())
    }
}

fn extract_linked_ids(
    val: &Value,
    all_thread_ids: &HashSet<String>,
    thread_ids: &mut BTreeSet<String>,
    run_ids: &mut BTreeSet<String>,
    command_ids: &mut BTreeSet<String>,
    outbox_ids: &mut BTreeSet<String>,
    effect_ids: &mut BTreeSet<String>,
) -> bool {
    let mut changed = false;
    match val {
        Value::Object(map) => {
            for (k, v) in map {
                if let Some(s) = v.as_str().filter(|s| !s.trim().is_empty()) {
                    match k.as_str() {
                        "thread_id" | "child_thread_id" | "main_thread_id"
                        | "parent_thread_id" | "subthread_id" => {
                            changed |= insert_non_empty(thread_ids, s);
                        }
                        "agent_id" => {
                            if s.starts_with("thr")
                                || s.starts_with("thread")
                                || s.starts_with("conv")
                                || s.starts_with("sub")
                                || all_thread_ids.contains(s)
                            {
                                changed |= insert_non_empty(thread_ids, s);
                            }
                        }
                        "run_id" | "bound_run_id" | "source_run_id" => {
                            changed |= insert_non_empty(run_ids, s);
                        }
                        "command_id" => {
                            changed |= insert_non_empty(command_ids, s);
                        }
                        "outbox_id" => {
                            changed |= insert_non_empty(outbox_ids, s);
                        }
                        "effect_id" => {
                            changed |= insert_non_empty(effect_ids, s);
                        }
                        _ => {}
                    }
                } else if matches!(k.as_str(), "related_run_ids" | "source_run_ids")
                    && let Some(arr) = v.as_array()
                {
                    for item in arr {
                        if let Some(s) = item.as_str() {
                            changed |= insert_non_empty(run_ids, s);
                        }
                    }
                } else {
                    changed |= extract_linked_ids(
                        v,
                        all_thread_ids,
                        thread_ids,
                        run_ids,
                        command_ids,
                        outbox_ids,
                        effect_ids,
                    );
                }
            }
        }
        Value::Array(items) => {
            for item in items {
                changed |= extract_linked_ids(
                    item,
                    all_thread_ids,
                    thread_ids,
                    run_ids,
                    command_ids,
                    outbox_ids,
                    effect_ids,
                );
            }
        }
        _ => {}
    }
    changed
}

fn id_matches_token(doc_id: &str, token: &str) -> bool {
    if doc_id == token {
        return true;
    }
    doc_id
        .strip_prefix(token)
        .is_some_and(|rest| rest.starts_with(':'))
        || doc_id
            .strip_suffix(token)
            .is_some_and(|rest| rest.ends_with(':'))
        || doc_id.contains(&format!(":{token}:"))
}

fn id_matches_any(doc_id: &str, set: &BTreeSet<String>) -> bool {
    set.iter().any(|token| id_matches_token(doc_id, token))
}

fn value_references_str(val: &Value, needle: &str) -> bool {
    match val {
        Value::String(s) => id_matches_token(s, needle),
        Value::Array(items) => items.iter().any(|item| value_references_str(item, needle)),
        Value::Object(map) => map.values().any(|v| value_references_str(v, needle)),
        _ => false,
    }
}

fn value_references_any(val: &Value, set: &BTreeSet<String>) -> bool {
    if set.is_empty() {
        return false;
    }
    match val {
        Value::String(s) => id_matches_any(s, set),
        Value::Array(items) => items.iter().any(|item| value_references_any(item, set)),
        Value::Object(map) => map.values().any(|v| value_references_any(v, set)),
        _ => false,
    }
}

fn non_empty_str(v: &Value) -> Option<&str> {
    v.as_str().map(str::trim).filter(|s| !s.is_empty())
}

fn explicit_other_task(val: &Value, target_task_id: &str) -> bool {
    let direct_fields = [
        non_empty_str(&val["task_id"]),
        non_empty_str(&val["values"]["task_id"]),
        non_empty_str(&val["src_task_id"]),
        if val["aggregate_type"] == "task" {
            non_empty_str(&val["aggregate_id"])
        } else {
            None
        },
        if val["aggregate"]["type"] == "task" {
            non_empty_str(&val["aggregate"]["id"])
        } else {
            None
        },
    ];
    direct_fields
        .into_iter()
        .flatten()
        .any(|id| id != target_task_id)
}

fn value_references_target_task(val: &Value, target_task_id: &str) -> bool {
    let task_fields = [
        non_empty_str(&val["task_id"]),
        non_empty_str(&val["src_task_id"]),
        non_empty_str(&val["values"]["task_id"]),
        if val["aggregate_type"].is_null() || val["aggregate_type"] == "task" {
            non_empty_str(&val["aggregate_id"])
        } else {
            None
        },
        if val["aggregate"]["type"].is_null() || val["aggregate"]["type"] == "task" {
            non_empty_str(&val["aggregate"]["id"])
        } else {
            None
        },
        non_empty_str(&val["request"]["payload"]["task_id"]),
        non_empty_str(&val["request"]["task_id"]),
        non_empty_str(&val["output"]["task_id"]),
        non_empty_str(&val["receipt"]["output"]["task_id"]),
        if val["receipt"]["aggregate"]["type"].is_null()
            || val["receipt"]["aggregate"]["type"] == "task"
        {
            non_empty_str(&val["receipt"]["aggregate"]["id"])
        } else {
            None
        },
        non_empty_str(&val["response"]["receipt"]["output"]["task_id"]),
        if val["response"]["receipt"]["aggregate"]["type"].is_null()
            || val["response"]["receipt"]["aggregate"]["type"] == "task"
        {
            non_empty_str(&val["response"]["receipt"]["aggregate"]["id"])
        } else {
            None
        },
    ];
    task_fields
        .into_iter()
        .flatten()
        .any(|id| id == target_task_id)
}

fn src_doc_matches_task(
    doc: &RawDocument,
    target_task_id: &str,
    project_id: Option<&str>,
    thread_ids: &BTreeSet<String>,
    run_ids: &BTreeSet<String>,
    command_ids: &BTreeSet<String>,
    outbox_ids: &BTreeSet<String>,
    effect_ids: &BTreeSet<String>,
) -> bool {
    let val = &doc.value;
    if explicit_other_task(val, target_task_id) {
        return false;
    }
    let doc_id = doc.id.as_str();
    match doc.collection.as_str() {
        "src_tasks" | "history_src_tasks" => doc_id == target_task_id,
        "src_live_policies" | "src_task_event_heads" => {
            doc_id == target_task_id || val["task_id"] == target_task_id
        }
        "src_live_records" => {
            val["values"]["task_id"] == target_task_id || val["task_id"] == target_task_id
        }
        "history_src_records" => {
            val["task_id"] == target_task_id
                || doc_id.starts_with(&format!("{target_task_id}:"))
        }
        "src_project_memory" => {
            doc_id == target_task_id
                || doc_id == format!("task:{target_task_id}")
                || val["task_id"] == target_task_id
                || project_id.is_some_and(|p| {
                    doc_id == p || doc_id == format!("project:{p}") || val["project_id"] == p
                })
        }
        "src_credential_metadata" => {
            val["task_id"] == target_task_id
                || doc_id.starts_with(&format!("task:{target_task_id}:"))
                || project_id.is_some_and(|p| {
                    val["project_id"] == p || doc_id.starts_with(&format!("project:{p}:"))
                })
        }
        _ => {
            let quoted_prefix = format!(
                "[{},",
                serde_json::to_string(target_task_id).unwrap_or_default()
            );
            doc_id == target_task_id
                || doc_id.starts_with(&format!("{target_task_id}:"))
                || doc_id.starts_with(&quoted_prefix)
                || id_matches_any(doc_id, command_ids)
                || id_matches_any(doc_id, outbox_ids)
                || id_matches_any(doc_id, effect_ids)
                || id_matches_any(doc_id, run_ids)
                || id_matches_any(doc_id, thread_ids)
                || value_references_target_task(val, target_task_id)
                || value_references_any(val, run_ids)
                || value_references_any(val, thread_ids)
                || value_references_any(val, command_ids)
                || value_references_any(val, outbox_ids)
                || value_references_any(val, effect_ids)
        }
    }
}

fn thread_state(doc: &Value) -> &Value {
    if doc["thread"]["state"].is_object() {
        &doc["thread"]["state"]
    } else {
        &doc["state"]
    }
}

fn thread_parent_candidates<'a>(doc: &'a Value) -> Vec<&'a str> {
    let t_meta = &doc["thread"];
    let t_state = &doc["thread"]["state"];
    let legacy_state = &doc["state"];
    [
        t_state["lineage"]["parent_thread_id"].as_str(),
        doc["lineage"]["parent_thread_id"].as_str(),
        legacy_state["lineage"]["parent_thread_id"].as_str(),
        t_meta["parent_thread_id"].as_str(),
        t_meta["src_delegation"]["parent_thread_id"].as_str(),
        t_meta["src_delegation"]["main_thread_id"].as_str(),
        t_meta["side_chat"]["parent_thread_id"].as_str(),
    ]
    .into_iter()
    .flatten()
    .map(str::trim)
    .filter(|s| !s.is_empty())
    .collect()
}

fn collect_subthread_children(state: &Value, thread_ids: &mut BTreeSet<String>) -> bool {
    let mut changed = false;
    for field in ["subagents", "agents"] {
        if let Some(items) = state[field].as_array() {
            for item in items {
                for key in ["agent_id", "thread_id"] {
                    if let Some(sid) = non_empty_str(&item[key]) {
                        changed |= thread_ids.insert(sid.to_owned());
                    }
                }
            }
        }
    }
    changed
}

fn collect_snapshot_data(
    conn: &Connection,
    target_task_id: &str,
) -> rusqlite::Result<Option<SnapshotData>> {
    let live_task = conn
        .query_row(
            "SELECT value FROM documents WHERE collection='src_tasks' AND id=?1",
            params![target_task_id],
            |row| row.get::<_, String>(0),
        )
        .optional()?;
    let (raw_task, is_historical) = match live_task {
        Some(raw) => (raw, false),
        None => {
            let Some(hist) = conn
                .query_row(
                    "SELECT value FROM documents WHERE collection='history_src_tasks' AND id=?1",
                    params![target_task_id],
                    |row| row.get::<_, String>(0),
                )
                .optional()?
            else {
                return Ok(None);
            };
            (hist, true)
        }
    };
    let target_task: Value = serde_json::from_str(&raw_task).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
    })?;
    let project_id = non_empty_str(&target_task["project_id"]).map(str::to_owned);

    let mut src_docs = Vec::new();
    {
        let mut stmt = conn.prepare(
            "SELECT collection, id, value FROM documents WHERE collection LIKE 'src\\_%' ESCAPE '\\' ORDER BY collection, id",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?;
        for row in rows {
            let (collection, id, raw) = row?;
            let value: Value = serde_json::from_str(&raw).map_err(|e| {
                rusqlite::Error::FromSqlConversionFailure(
                    0,
                    rusqlite::types::Type::Text,
                    Box::new(e),
                )
            })?;
            src_docs.push(RawDocument {
                collection,
                id,
                value,
            });
        }
    }
    {
        let mut stmt = conn.prepare(
            "SELECT collection, id, value FROM documents WHERE collection IN ('history_src_tasks', 'history_src_records') ORDER BY collection, id",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?;
        for row in rows {
            let (collection, id, raw) = row?;
            let value: Value = serde_json::from_str(&raw).map_err(|e| {
                rusqlite::Error::FromSqlConversionFailure(
                    0,
                    rusqlite::types::Type::Text,
                    Box::new(e),
                )
            })?;
            src_docs.push(RawDocument {
                collection,
                id,
                value,
            });
        }
    }

    let mut thread_docs = Vec::new();
    let mut all_thread_ids = HashSet::new();
    {
        let mut stmt = conn.prepare(
            "SELECT id, value FROM documents WHERE collection='conversation_threads' ORDER BY id",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        for row in rows {
            let (id, raw) = row?;
            let value: Value = serde_json::from_str(&raw).map_err(|e| {
                rusqlite::Error::FromSqlConversionFailure(
                    0,
                    rusqlite::types::Type::Text,
                    Box::new(e),
                )
            })?;
            all_thread_ids.insert(id.clone());
            thread_docs.push((id, value));
        }
    }

    let mut related_docs = Vec::new();
    {
        let mut stmt = conn.prepare(
            "SELECT collection, id, value FROM documents WHERE collection IN ('runs', 'run_side_chats', 'conversation_commands', 'conversation_runtime_selections', 'platform_commands') ORDER BY collection, id",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?;
        for row in rows {
            let (collection, id, raw) = row?;
            let value: Value = serde_json::from_str(&raw).map_err(|e| {
                rusqlite::Error::FromSqlConversionFailure(
                    0,
                    rusqlite::types::Type::Text,
                    Box::new(e),
                )
            })?;
            related_docs.push(RawDocument {
                collection,
                id,
                value,
            });
        }
    }

    let mut thread_ids = BTreeSet::new();
    let mut run_ids = BTreeSet::new();
    let mut command_ids = BTreeSet::new();
    let mut outbox_ids = BTreeSet::new();
    let mut effect_ids = BTreeSet::new();
    if let Some(main_tid) = non_empty_str(&target_task["binding"]["thread_id"]) {
        thread_ids.insert(main_tid.to_owned());
    }
    extract_linked_ids(
        &target_task,
        &all_thread_ids,
        &mut thread_ids,
        &mut run_ids,
        &mut command_ids,
        &mut outbox_ids,
        &mut effect_ids,
    );

    let mut matched_src = vec![false; src_docs.len()];
    let mut changed = true;
    while changed {
        changed = false;
        for (idx, doc) in src_docs.iter().enumerate() {
            if matched_src[idx] {
                continue;
            }
            if src_doc_matches_task(
                doc,
                target_task_id,
                project_id.as_deref(),
                &thread_ids,
                &run_ids,
                &command_ids,
                &outbox_ids,
                &effect_ids,
            ) {
                matched_src[idx] = true;
                changed = true;
                extract_linked_ids(
                    &doc.value,
                    &all_thread_ids,
                    &mut thread_ids,
                    &mut run_ids,
                    &mut command_ids,
                    &mut outbox_ids,
                    &mut effect_ids,
                );
            }
        }
        for doc in &related_docs {
            if doc.collection == "runs" {
                let is_match = run_ids.contains(&doc.id)
                    || doc.value["src_task_id"] == target_task_id
                    || doc.value["task_id"] == target_task_id;
                if is_match {
                    changed |= run_ids.insert(doc.id.clone());
                    changed |= extract_linked_ids(
                        &doc.value,
                        &all_thread_ids,
                        &mut thread_ids,
                        &mut run_ids,
                        &mut command_ids,
                        &mut outbox_ids,
                        &mut effect_ids,
                    );
                }
            } else if doc.collection == "run_side_chats" {
                let is_match = id_matches_any(&doc.id, &run_ids)
                    || id_matches_any(&doc.id, &thread_ids)
                    || value_references_str(&doc.value, target_task_id)
                    || value_references_any(&doc.value, &run_ids)
                    || value_references_any(&doc.value, &thread_ids);
                if is_match {
                    changed |= extract_linked_ids(
                        &doc.value,
                        &all_thread_ids,
                        &mut thread_ids,
                        &mut run_ids,
                        &mut command_ids,
                        &mut outbox_ids,
                        &mut effect_ids,
                    );
                }
            }
        }
        for (tid, doc) in &thread_docs {
            let t_meta = &doc["thread"];
            let parent_linked = thread_parent_candidates(doc)
                .into_iter()
                .any(|parent| thread_ids.contains(parent));
            let task_linked = t_meta["src_delegation"]["task_id"] == target_task_id
                || t_meta["side_chat"]["parent_task_id"] == target_task_id
                || t_meta["metadata"]["src_task_id"] == target_task_id
                || doc["native_session"]["src_task_id"] == target_task_id
                || non_empty_str(&t_meta["side_chat"]["bound_run_id"])
                    .is_some_and(|rid| run_ids.contains(rid))
                || non_empty_str(&t_meta["side_chat"]["run_id"])
                    .is_some_and(|rid| run_ids.contains(rid));
            if thread_ids.contains(tid) || parent_linked || task_linked {
                changed |= thread_ids.insert(tid.clone());
                changed |= collect_subthread_children(&doc["thread"]["state"], &mut thread_ids);
                changed |= collect_subthread_children(&doc["state"], &mut thread_ids);
            }
        }
    }

    let mut chat_events = Vec::new();
    let thread_list: Vec<String> = thread_ids.iter().cloned().collect();
    for chunk in thread_list.chunks(EVENT_QUERY_CHUNK_SIZE) {
        let placeholders = vec!["?"; chunk.len()].join(",");
        let sql = format!(
            "SELECT seq, thread_id, turn_id, stream_seq, event_id, event_type, payload FROM chat_events WHERE thread_id IN ({placeholders}) ORDER BY seq ASC"
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(params_from_iter(chunk.iter()), |row| {
            Ok(ChatEventRow {
                seq: row.get(0)?,
                thread_id: row.get(1)?,
                turn_id: row.get(2)?,
                stream_seq: row.get(3)?,
                event_id: row.get(4)?,
                event_type: row.get(5)?,
                payload: row.get(6)?,
            })
        })?;
        for row in rows {
            chat_events.push(row?);
        }
    }
    chat_events.sort_by_key(|event| event.seq);

    Ok(Some(SnapshotData {
        target_task,
        is_historical,
        src_docs,
        thread_docs,
        related_docs,
        matched_src,
        thread_ids,
        run_ids,
        command_ids,
        chat_events,
    }))
}

fn read_log_tail(path: &Path) -> Option<String> {
    let meta = std::fs::symlink_metadata(path).ok()?;
    if !meta.is_file() || meta.file_type().is_symlink() {
        return None;
    }
    let mut file = std::fs::File::open(path).ok()?;
    let len = meta.len();
    if len > MAX_LOG_READ_BYTES {
        file.seek(SeekFrom::End(-(MAX_LOG_READ_BYTES as i64)))
            .ok()?;
    }
    let mut buf = Vec::new();
    file.read_to_end(&mut buf).ok()?;
    let text = String::from_utf8_lossy(&buf);
    let mut lines: Vec<&str> = text.lines().collect();
    if len > MAX_LOG_READ_BYTES && !lines.is_empty() {
        lines.remove(0);
    }
    let start = lines.len().saturating_sub(LOG_TAIL_LINES);
    let redacted: Vec<String> = lines[start..]
        .iter()
        .map(|line| redact_text(line, None))
        .collect();
    Some(redacted.join("\n"))
}

fn append_tar_bytes<W: std::io::Write>(
    builder: &mut tar::Builder<W>,
    name: &str,
    data: &[u8],
    mtime: u64,
) -> Result<(), DebugExportError> {
    let mut header = tar::Header::new_gnu();
    header.set_size(data.len() as u64);
    header.set_mode(0o644);
    header.set_mtime(mtime);
    header.set_cksum();
    builder
        .append_data(&mut header, name, Cursor::new(data))
        .map_err(|e| DebugExportError::Internal(format!("tar append {name}: {e}")))
}

fn safe_filename_task_part(task_id: &str) -> String {
    let cleaned: String = task_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_') {
                c
            } else {
                '-'
            }
        })
        .take(48)
        .collect();
    let trimmed = cleaned.trim_matches('-');
    if trimmed.is_empty() {
        "task".to_owned()
    } else {
        trimmed.to_owned()
    }
}

fn is_traffic_record(col: &str, val: &Value) -> bool {
    col.contains("traffic")
        || (col == "src_live_records"
            && matches!(
                val["values"]["kind"].as_str(),
                Some("traffic_exchange" | "exchange" | "http_exchange")
            ))
}

pub fn build_debug_archive(
    store: &Store,
    state_dir: Option<&Path>,
    task_id: &str,
) -> Result<DebugArchive, DebugExportError> {
    let target_task_id = task_id.trim();
    if target_task_id.is_empty()
        || target_task_id.len() > 200
        || target_task_id.chars().any(char::is_control)
    {
        return Err(DebugExportError::InvalidTaskId(
            "无效的 SRC 任务 ID".to_owned(),
        ));
    }

    let snapshot = store
        .read_snapshot(|conn| collect_snapshot_data(conn, target_task_id))
        .map_err(|e| DebugExportError::Internal(format!("读取数据库快照失败: {e}")))?
        .ok_or_else(|| DebugExportError::NotFound("SRC Task 不存在".to_owned()))?;

    let mut src_docs_map: BTreeMap<String, Vec<Value>> = BTreeMap::new();
    let mut src_doc_count = 0usize;
    for (idx, doc) in snapshot.src_docs.iter().enumerate() {
        if !snapshot.matched_src[idx] {
            continue;
        }
        let cap = is_traffic_record(&doc.collection, &doc.value);
        src_docs_map
            .entry(doc.collection.clone())
            .or_default()
            .push(json!({
                "id": doc.id,
                "value": scrub_value(&doc.value, cap),
            }));
        src_doc_count += 1;
    }

    let mut exported_threads: BTreeMap<String, Value> = BTreeMap::new();
    for (tid, doc) in &snapshot.thread_docs {
        if snapshot.thread_ids.contains(tid) {
            exported_threads.insert(tid.clone(), scrub_value(doc, false));
        }
    }

    let mut chat_events_out = Vec::with_capacity(snapshot.chat_events.len());
    for event in &snapshot.chat_events {
        let payload = match serde_json::from_str::<Value>(&event.payload) {
            Ok(parsed) => scrub_value(&parsed, false),
            Err(_) => Value::String(redact_text(&event.payload, Some(TRAFFIC_BODY_CHAR_LIMIT))),
        };
        chat_events_out.push(json!({
            "seq": event.seq,
            "thread_id": event.thread_id,
            "turn_id": event.turn_id,
            "stream_seq": event.stream_seq,
            "event_id": event.event_id,
            "event_type": event.event_type,
            "payload": payload,
        }));
    }

    let mut related_docs_map: BTreeMap<String, Vec<Value>> = BTreeMap::new();
    let mut related_doc_count = 0usize;
    for doc in &snapshot.related_docs {
        let matched = match doc.collection.as_str() {
            "runs" => {
                snapshot.run_ids.contains(&doc.id)
                    || doc.value["src_task_id"] == target_task_id
                    || doc.value["task_id"] == target_task_id
            }
            "run_side_chats" => {
                id_matches_any(&doc.id, &snapshot.run_ids)
                    || id_matches_any(&doc.id, &snapshot.thread_ids)
                    || value_references_str(&doc.value, target_task_id)
                    || value_references_any(&doc.value, &snapshot.run_ids)
                    || value_references_any(&doc.value, &snapshot.thread_ids)
            }
            "conversation_commands" | "conversation_runtime_selections" => {
                id_matches_any(&doc.id, &snapshot.thread_ids)
                    || value_references_any(&doc.value, &snapshot.thread_ids)
            }
            "platform_commands" => {
                snapshot.command_ids.contains(&doc.id)
                    || value_references_str(&doc.value, target_task_id)
                    || value_references_any(&doc.value, &snapshot.thread_ids)
                    || value_references_any(&doc.value, &snapshot.run_ids)
            }
            _ => false,
        };
        if matched {
            related_docs_map
                .entry(doc.collection.clone())
                .or_default()
                .push(json!({
                    "id": doc.id,
                    "value": scrub_value(&doc.value, false),
                }));
            related_doc_count += 1;
        }
    }

    let mut threads_overview = Vec::with_capacity(exported_threads.len());
    for (tid, doc) in &exported_threads {
        let t_meta = &doc["thread"];
        let t_state = thread_state(doc);
        let parent_thread_id = thread_parent_candidates(doc)
            .into_iter()
            .next()
            .map(Value::from)
            .unwrap_or(Value::Null);
        let subagents = if !t_state["subagents"].is_null() {
            t_state["subagents"].clone()
        } else if !t_state["agents"].is_null() {
            t_state["agents"].clone()
        } else {
            Value::Null
        };
        let turns = doc["turns"].as_array();
        let last_turn_status = turns
            .and_then(|arr| arr.last())
            .and_then(|turn| turn.get("status"))
            .cloned()
            .unwrap_or(Value::Null);
        threads_overview.push(json!({
            "thread_id": tid,
            "title": t_meta["title"],
            "mode": t_meta["mode"],
            "status": t_state["status"],
            "running_turn_id": t_state["running_turn_id"],
            "parent_thread_id": parent_thread_id,
            "src_delegation": t_meta["src_delegation"],
            "side_chat": t_meta["side_chat"],
            "subagents": subagents,
            "turn_count": turns.map_or(0, Vec::len),
            "last_turn_status": last_turn_status,
            "last_error": t_state["last_error"],
            "updated_at": t_meta["updated_at"],
        }));
    }

    let mut included_logs = Vec::new();
    let mut log_entries: Vec<(&'static str, String)> = Vec::new();
    if let Some(root) = state_dir {
        let candidates = [
            (root.join("desktop-startup.log"), "desktop-startup-tail.log"),
            (root.join("runner").join("runner.log"), "runner-tail.log"),
        ];
        for (path, archive_name) in candidates {
            if let Some(content) = read_log_tail(&path) {
                included_logs.push(archive_name);
                log_entries.push((archive_name, content));
            }
        }
    }

    let src_collection_counts: BTreeMap<String, usize> = src_docs_map
        .iter()
        .map(|(k, v)| (k.clone(), v.len()))
        .collect();
    let related_collection_counts: BTreeMap<String, usize> = related_docs_map
        .iter()
        .map(|(k, v)| (k.clone(), v.len()))
        .collect();

    let exported_at = chrono::Utc::now().to_rfc3339();
    let summary = json!({
        "schema": SCHEMA_VERSION,
        "exported_at": exported_at,
        "task_id": target_task_id,
        "task_title": redact_text(snapshot.target_task["title"].as_str().unwrap_or(""), None),
        "main_thread_id": snapshot.target_task["binding"]["thread_id"],
        "historical": snapshot.is_historical,
        "scope": "src_task",
        "traffic_body_char_limit": TRAFFIC_BODY_CHAR_LIMIT,
        "log_tail_lines": LOG_TAIL_LINES,
        "src_collections": src_collection_counts,
        "related_collections": related_collection_counts,
        "thread_count": exported_threads.len(),
        "run_ids": snapshot.run_ids.iter().cloned().collect::<Vec<_>>(),
        "chat_event_count": chat_events_out.len(),
        "included_logs": included_logs,
        "threads_overview": threads_overview,
    });

    let metadata = json!({
        "schema": SCHEMA_VERSION,
        "task_id": target_task_id,
        "src_collection_count": src_docs_map.len(),
        "src_document_count": src_doc_count,
        "thread_count": exported_threads.len(),
        "chat_event_count": chat_events_out.len(),
        "related_document_count": related_doc_count,
        "included_log_count": log_entries.len(),
    });

    let summary_bytes = serde_json::to_vec_pretty(&summary)
        .map_err(|e| DebugExportError::Internal(format!("serialize summary: {e}")))?;
    let src_bytes = serde_json::to_vec(&src_docs_map)
        .map_err(|e| DebugExportError::Internal(format!("serialize src_documents: {e}")))?;
    let threads_bytes = serde_json::to_vec(&exported_threads)
        .map_err(|e| DebugExportError::Internal(format!("serialize conversation_threads: {e}")))?;
    let mut events_jsonl = String::new();
    for event in &chat_events_out {
        let line = serde_json::to_string(event)
            .map_err(|e| DebugExportError::Internal(format!("serialize chat_event: {e}")))?;
        events_jsonl.push_str(&line);
        events_jsonl.push('\n');
    }
    let related_bytes = serde_json::to_vec(&related_docs_map)
        .map_err(|e| DebugExportError::Internal(format!("serialize related_documents: {e}")))?;

    let mtime = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let encoder = GzEncoder::new(Vec::new(), Compression::default());
    let mut tar_builder = tar::Builder::new(encoder);
    append_tar_bytes(&mut tar_builder, "summary.json", &summary_bytes, mtime)?;
    append_tar_bytes(&mut tar_builder, "src_documents.json", &src_bytes, mtime)?;
    append_tar_bytes(
        &mut tar_builder,
        "conversation_threads.json",
        &threads_bytes,
        mtime,
    )?;
    append_tar_bytes(
        &mut tar_builder,
        "chat_events.jsonl",
        events_jsonl.as_bytes(),
        mtime,
    )?;
    append_tar_bytes(
        &mut tar_builder,
        "related_documents.json",
        &related_bytes,
        mtime,
    )?;
    for (name, content) in log_entries {
        append_tar_bytes(&mut tar_builder, name, content.as_bytes(), mtime)?;
    }

    let encoder = tar_builder
        .into_inner()
        .map_err(|e| DebugExportError::Internal(format!("tar finish: {e}")))?;
    let bytes = encoder
        .finish()
        .map_err(|e| DebugExportError::Internal(format!("gzip finish: {e}")))?;

    let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S");
    let filename = format!(
        "muteki-src-debug-{}-{}.tar.gz",
        safe_filename_task_part(target_task_id),
        stamp
    );

    Ok(DebugArchive {
        filename,
        bytes,
        metadata,
        summary,
    })
}

fn content_disposition(name: &str) -> String {
    let clean: String = name
        .chars()
        .filter(|c| !c.is_control() && !"<>:\"/\\|?*".contains(*c))
        .take(120)
        .collect();
    let fallback: String = clean.chars().filter(|c| c.is_ascii()).collect();
    let encoded: String = clean
        .bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b"-_.".contains(&b) {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect();
    format!(
        "attachment; filename=\"{}\"; filename*=UTF-8''{}",
        if fallback.is_empty() {
            "muteki-src-debug.tar.gz"
        } else {
            &fallback
        },
        encoded
    )
}

fn error_response(status: StatusCode, code: &str, message: &str) -> Response {
    (
        status,
        Json(json!({"error": {"code": code, "message": message}})),
    )
        .into_response()
}

pub(crate) async fn export_handler(s: Arc<AppState>, task_id: String) -> Response {
    let state_dir = crate::host_process::configured_state_dir(&s);
    let result = tokio::task::spawn_blocking(move || {
        build_debug_archive(&s.store, state_dir.as_deref(), &task_id)
    })
    .await;

    let archive = match result {
        Ok(Ok(archive)) => archive,
        Ok(Err(DebugExportError::InvalidTaskId(msg))) => {
            return error_response(StatusCode::BAD_REQUEST, "src.invalid_request", &msg);
        }
        Ok(Err(DebugExportError::NotFound(msg))) => {
            return error_response(StatusCode::NOT_FOUND, "src.task_not_found", &msg);
        }
        Ok(Err(DebugExportError::Internal(msg))) => {
            tracing::error!(error = %msg, "SRC debug logs export failed");
            return error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "src.debug_logs_failed",
                &msg,
            );
        }
        Err(join_err) => {
            tracing::error!(error = %join_err, "SRC debug logs export task panicked");
            return error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "src.debug_logs_failed",
                "打包 SRC 诊断日志任务失败",
            );
        }
    };

    let disposition_header = match HeaderValue::from_str(&content_disposition(&archive.filename)) {
        Ok(v) => v,
        Err(e) => {
            return error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "src.debug_logs_failed",
                &e.to_string(),
            );
        }
    };
    let metadata_header = match HeaderValue::from_str(&archive.metadata.to_string()) {
        Ok(v) => v,
        Err(e) => {
            return error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "src.debug_logs_failed",
                &e.to_string(),
            );
        }
    };

    let mut response = archive.bytes.into_response();
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/gzip"),
    );
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(header::CONTENT_DISPOSITION, disposition_header);
    headers.insert("X-Muteki-Export-Metadata", metadata_header);
    headers.insert(
        "Access-Control-Expose-Headers",
        HeaderValue::from_static("Content-Disposition, X-Muteki-Export-Metadata"),
    );
    response
}
