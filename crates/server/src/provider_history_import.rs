//! Passive Claude/Codex history copies. No provider or execution is involved.
use crate::AppState;
use axum::{
    Json, Router,
    extract::{Query, State},
    http::StatusCode,
    routing::{get, post},
};
use regex::Regex;
use rusqlite::{Connection, OpenFlags};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::HashSet,
    fs,
    io::{BufRead, BufReader, Read},
    path::{Path, PathBuf},
    sync::{Arc, LazyLock},
    time::SystemTime,
};
use uuid::Uuid;

const FILE_BYTES: u64 = 32 * 1024 * 1024;
const RECORD_BYTES: usize = 1024 * 1024;
const RECORDS: usize = 20_000;
const SCAN_FILES: usize = 10_000;
const SCAN_BYTES: u64 = 64 * 1024 * 1024;
type Error = (StatusCode, Json<Value>);
type Api = Result<Json<Value>, Error>;
fn error(status: StatusCode, code: &str, message: impl ToString) -> Error {
    (
        status,
        Json(json!({"error":{"code":code,"message":message.to_string()}})),
    )
}
fn invalid(message: impl ToString) -> Error {
    error(
        StatusCode::UNPROCESSABLE_ENTITY,
        "conversation.import.scan_failed",
        message,
    )
}
fn budget() -> Error {
    error(
        StatusCode::UNPROCESSABLE_ENTITY,
        "conversation.import.scan_budget_exceeded",
        "历史扫描超过预算，请选择更具体的目录或拆分文件。",
    )
}
fn database(_: rusqlite::Error) -> Error {
    error(
        StatusCode::INTERNAL_SERVER_ERROR,
        "conversation.import.storage_failed",
        "无法保存导入历史",
    )
}
pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/import/scan", get(scan))
        .route("/api/import/apply", post(apply))
}
fn source(adapter: &str, input: &str) -> Result<PathBuf, Error> {
    if !matches!(adapter, "claude" | "codex") {
        return Err(error(
            StatusCode::BAD_REQUEST,
            "conversation.import.provider_invalid",
            "请选择 Claude 或 Codex 历史来源。",
        ));
    }
    if input.is_empty() && std::env::var("MUTEKI_HOST_DISCOVERY").is_ok_and(|v| v.trim() == "0") {
        return Err(error(
            StatusCode::CONFLICT,
            "conversation.import.host_discovery_disabled",
            "服务宿主自动发现已禁用，请明确选择历史文件目录。",
        ));
    }
    if input.is_empty() || input == "~" || input.starts_with("~/") {
        let home = std::env::var_os("HOME").ok_or_else(|| invalid("无法找到主目录，请指定路径"))?;
        return Ok(PathBuf::from(home).join(if input.is_empty() {
            if adapter == "claude" {
                ".claude"
            } else {
                ".codex"
            }
        } else {
            input.strip_prefix("~/").unwrap_or("")
        }));
    }
    Ok(PathBuf::from(input))
}
#[derive(Deserialize)]
pub(crate) struct ScanQuery {
    #[serde(default = "claude")]
    adapter: String,
    #[serde(default)]
    path: String,
    #[serde(default = "hundred")]
    limit: i64,
}
fn claude() -> String {
    "claude".into()
}
fn hundred() -> i64 {
    100
}
#[derive(Clone)]
struct Scan {
    preview: Value,
    messages: Vec<Value>,
}
pub(crate) async fn scan(Query(q): Query<ScanQuery>) -> Api {
    let base = source(&q.adapter, &q.path)?;
    if !base.exists() {
        return Ok(Json(json!({"scans":[],"base_path":base,"exists":false})));
    }
    let path = base.clone();
    let (scans, _) = tokio::task::spawn_blocking(move || {
        scan_directory(
            &path,
            &q.adapter,
            q.limit.clamp(1, 500) as usize,
            false,
            false,
        )
    })
    .await
    .map_err(|e| invalid(e.to_string()))??;
    Ok(Json(
        json!({"scans":scans.into_iter().map(|s|s.preview).collect::<Vec<_>>(),"base_path":base,"exists":true}),
    ))
}
#[derive(Deserialize)]
pub(crate) struct Apply {
    #[serde(default = "claude")]
    adapter_id: String,
    #[serde(default)]
    source_path: String,
    #[serde(default)]
    sessions: Vec<String>,
    #[serde(default)]
    project_id: String,
    #[serde(default)]
    dry_run: bool,
}
pub(crate) async fn apply(State(state): State<Arc<AppState>>, Json(body): Json<Apply>) -> Api {
    let base = source(&body.adapter_id, &body.source_path)?;
    if !base.exists() {
        return Err(error(
            StatusCode::NOT_FOUND,
            "conversation.import.source_missing",
            format!("path not found: {}", base.display()),
        ));
    }
    let adapter = body.adapter_id.clone();
    let (mut scans, failures) =
        tokio::task::spawn_blocking(move || scan_directory(&base, &adapter, 500, true, true))
            .await
            .map_err(|e| invalid(e.to_string()))??;
    if !body.sessions.is_empty() {
        let selected: HashSet<_> = body.sessions.iter().map(String::as_str).collect();
        scans.retain(|s| selected.contains(s.preview["session_id"].as_str().unwrap_or("")));
        if scans.len() != selected.len() {
            return Err(error(
                StatusCode::CONFLICT,
                "conversation.import.source_missing",
                if failures.is_empty() {
                    "选中的会话来源已改变或不在本次扫描范围，请重新扫描。".into()
                } else {
                    failures.join("; ")
                },
            ));
        }
    }
    let legacy = if body.dry_run {
        None
    } else {
        legacy_snapshot(&state.store)?
    };
    let _guard = state.chat.active.lock().await;
    if !body.project_id.is_empty()
        && state
            .store
            .get("projects", &body.project_id)
            .map_err(database)?
            .is_none()
    {
        return Err(error(
            StatusCode::NOT_FOUND,
            "conversation.import.project_missing",
            "项目不存在",
        ));
    }
    let (mut imported, mut skipped, mut failed) = (vec![], vec![], vec![]);
    for scan in scans {
        let sid = scan.preview["session_id"].as_str().unwrap_or("");
        if body.dry_run {
            imported.push(json!({"session_id":sid,"thread_id":null}));
            continue;
        }
        let key = format!("{}:{}", body.adapter_id, sid);
        let doc = document(&scan, &key, &body.project_id);
        let mut identity = json!({"adapter_id":body.adapter_id,"external_session_id":sid,"source_path":scan.preview["source_path"],"resume_handle":null});
        if state
            .store
            .get("provider_history_identities", &key)
            .map_err(database)?
            .is_none()
        {
            match legacy_thread(legacy.as_ref(), &state.store, &body.adapter_id, sid) {
                Ok(Some(tid)) => identity["legacy_thread_id"] = json!(tid),
                Ok(None) => {}
                Err(e) => {
                    failed.push(json!({"session_id":sid,"error":e}));
                    continue;
                }
            }
        }
        match state.store.import_provider_history(&key, &identity, doc) {
            Ok(Ok((doc, is_skipped))) => {
                let tid = doc["thread"]["thread_id"].as_str().unwrap_or("");
                if !is_skipped {
                    crate::chat::notify_imported(&state, tid);
                }
                let record = json!({"session_id":sid,"thread_id":tid});
                if is_skipped {
                    skipped.push(record);
                } else {
                    imported.push(record);
                }
            }
            Ok(Err(e)) => failed.push(json!({"session_id":sid,"error":e})),
            Err(e) => {
                tracing::error!(%e,"Provider history import failed");
                failed.push(json!({"session_id":sid,"error":"conversation.import.storage_failed: 无法保存导入历史"}));
            }
        }
    }
    Ok(Json(
        json!({"total":imported.len()+skipped.len()+failed.len(),"imported":imported,"skipped":skipped,"failed":failed,"scan_failures":failures}),
    ))
}
fn legacy_snapshot(store: &crate::store::Store) -> Result<Option<Connection>, Error> {
    let Some(directory) = store
        .get("system", "state_dir")
        .map_err(database)?
        .and_then(|v| v.as_str().map(str::to_owned))
    else {
        return Ok(None);
    };
    let path = PathBuf::from(directory).join("legacy/_platform/platform.db");
    if !path.is_file() {
        return Ok(None);
    }
    let mut uri =
        reqwest::Url::from_file_path(&path).map_err(|_| invalid("无法定位目标历史快照"))?;
    uri.set_query(Some("immutable=1"));
    Connection::open_with_flags(
        uri.as_str(),
        OpenFlags::SQLITE_OPEN_READ_ONLY
            | OpenFlags::SQLITE_OPEN_URI
            | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map(Some)
    .map_err(database)
}
fn legacy_thread(
    connection: Option<&Connection>,
    store: &crate::store::Store,
    adapter: &str,
    session: &str,
) -> Result<Option<String>, String> {
    let Some(connection) = connection else {
        return Ok(None);
    };
    let mut stmt = connection.prepare("SELECT thread_id FROM agent_sessions WHERE external_session_id=?1 AND adapter_id=?2 AND thread_id IS NOT NULL AND thread_id!=''").map_err(|e| e.to_string())?;
    let ids = stmt
        .query_map(rusqlite::params![session, adapter], |row| {
            row.get::<_, String>(0)
        })
        .map_err(|e| e.to_string())?;
    for id in ids {
        let id = id.map_err(|e| e.to_string())?;
        if store
            .get("conversation_threads", &id)
            .map_err(|e| e.to_string())?
            .is_some()
        {
            return Ok(Some(id));
        }
    }
    Ok(None)
}
fn document(scan: &Scan, key: &str, project: &str) -> Value {
    let tid = Uuid::new_v4().to_string();
    let time = chrono::Utc::now().to_rfc3339();
    let messages:Vec<_>=scan.messages.iter().enumerate().map(|(i,m)|{
        let role=m["role"].as_str().unwrap_or("unknown");
        json!({"message_id":Uuid::new_v4().to_string(),"thread_id":tid,"turn_id":null,"role":role,"kind":if role=="tool"{"imported_tool"}else if matches!(role,"user"|"assistant"|"system"){"message"}else{"imported_record"},"text":text_of(&m["content"]),"stream_seq":i as i64-scan.messages.len() as i64,"source_provider":scan.preview["adapter_id"],"source_role":m["source_role"],"source_content":m["source_content"],"provider_history_identity":key,"historical":true,"created_at":scan.preview["created_at"]})
    }).collect();
    json!({"thread":{"thread_id":tid,"title":scan.preview["title"],"title_source":"user","mode":"conversation","project_id":project,"workspace_id":null,"created_at":scan.preview["created_at"],"updated_at":time,"archived":false,"state":{"status":"idle","running_turn_id":null,"queue_count":0,"queue_paused":false,"queue_revision":0,"last_message_preview":format!("导入自 {} · {} 条历史消息",scan.preview["adapter_id"].as_str().unwrap_or(""),messages.len())}},"runtime":{},"import_status":"read_only","native_resume_supported":false,"queue":[],"messages":messages,"turns":[],"tools":[],"artifacts":[],"event_seq":0,"historical":true,"import_source":"provider_history"})
}
fn candidates(
    path: &Path,
    adapter: &str,
    out: &mut Vec<(SystemTime, PathBuf, u64)>,
) -> Result<(), Error> {
    let meta = fs::symlink_metadata(path).map_err(invalid)?;
    if meta.file_type().is_symlink() {
        return Ok(());
    }
    if meta.is_dir() {
        for entry in fs::read_dir(path).map_err(invalid)? {
            candidates(&entry.map_err(invalid)?.path(), adapter, out)?;
        }
    } else if meta.is_file()
        && (path.extension().is_some_and(|x| x == "jsonl")
            || adapter == "codex" && path.extension().is_some_and(|x| x == "json"))
    {
        out.push((
            meta.modified().map_err(invalid)?,
            path.to_owned(),
            meta.len(),
        ));
        if out.len() > SCAN_FILES {
            return Err(budget());
        }
    }
    Ok(())
}
fn scan_directory(
    base: &Path,
    adapter: &str,
    limit: usize,
    tolerant: bool,
    retain_messages: bool,
) -> Result<(Vec<Scan>, Vec<String>), Error> {
    let mut files = vec![];
    candidates(base, adapter, &mut files)?;
    files.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| b.1.cmp(&a.1)));
    let (mut scans, mut failures, mut seen, mut bytes) = (vec![], vec![], HashSet::new(), 0u64);
    for (_, path, size) in files {
        if scans.len() >= limit {
            break;
        }
        bytes += size;
        if bytes > SCAN_BYTES || size > FILE_BYTES {
            return Err(budget());
        }
        let result = read_scan(&path, adapter);
        match result {
            Ok(Some(mut scan)) => {
                let sid = scan.preview["session_id"].as_str().unwrap_or("").to_owned();
                if !seen.insert(sid.clone()) {
                    return Err(invalid(format!(
                        "conversation.import.source_conflict: 同目录有重复会话 ID {sid}"
                    )));
                }
                if !retain_messages {
                    scan.messages = Vec::new();
                }
                scans.push(scan);
            }
            Ok(None) => {}
            Err(e) if tolerant => failures.push(format!(
                "{}: {}",
                path.display(),
                e.1.0["error"]["message"].as_str().unwrap_or("历史解析失败")
            )),
            Err(e) => return Err(e),
        }
    }
    Ok((scans, failures))
}
fn records(path: &Path) -> Result<Vec<Value>, Error> {
    if fs::metadata(path).map_err(invalid)?.len() > FILE_BYTES {
        return Err(budget());
    }
    let mut reader = BufReader::new(fs::File::open(path).map_err(invalid)?);
    let first = loop {
        let buf = reader.fill_buf().map_err(invalid)?;
        let whitespace = buf.iter().take_while(|b| b.is_ascii_whitespace()).count();
        if whitespace == buf.len() && !buf.is_empty() {
            reader.consume(whitespace);
        } else {
            break buf.get(whitespace).copied();
        }
    };
    if path.extension().is_some_and(|x| x == "json") || first == Some(b'[') {
        let mut raw = vec![];
        reader
            .take(FILE_BYTES + 1)
            .read_to_end(&mut raw)
            .map_err(invalid)?;
        if raw.len() as u64 > FILE_BYTES {
            return Err(budget());
        }
        let parsed: Value = serde_json::from_slice(&raw).map_err(invalid)?;
        let rows = match parsed {
            Value::Array(v) => v,
            Value::Object(_) => vec![parsed],
            _ => {
                return Err(invalid(
                    "conversation.import.invalid_record: requires a JSON object or array",
                ));
            }
        };
        if rows.len() > RECORDS {
            return Err(budget());
        }
        return Ok(rows.into_iter().filter(Value::is_object).collect());
    }
    let (mut out, mut count, mut total) = (vec![], 0usize, 0usize);
    loop {
        let mut raw = vec![];
        let n = reader
            .by_ref()
            .take(RECORD_BYTES as u64 + 1)
            .read_until(b'\n', &mut raw)
            .map_err(invalid)?;
        if n == 0 {
            break;
        }
        count += 1;
        total += n;
        if n > RECORD_BYTES || count > RECORDS || total as u64 > FILE_BYTES {
            return Err(budget());
        }
        if raw.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        let v: Value = serde_json::from_slice(&raw).map_err(|e| {
            invalid(format!(
                "conversation.import.invalid_record: record {count}: {e}"
            ))
        })?;
        if v.is_object() {
            out.push(v);
        }
    }
    Ok(out)
}
fn read_scan(path: &Path, adapter: &str) -> Result<Option<Scan>, Error> {
    let rows = records(path)?;
    let messages = parse_messages(rows, adapter);
    if messages.is_empty() {
        return Ok(None);
    }
    let sid = path.file_stem().unwrap_or_default().to_string_lossy();
    let title = messages
        .iter()
        .find(|m| m["role"] == "user" && !text_of(&m["content"]).trim().is_empty())
        .map(|m| {
            let text = text_of(&m["content"]);
            let text = text.trim();
            let mut title: String = text.chars().take(80).collect();
            if text.chars().count() > 80 {
                title.push('…');
            }
            title
        })
        .unwrap_or_else(|| format!("导入会话 {}", sid.chars().take(8).collect::<String>()));
    let created: chrono::DateTime<chrono::Utc> = fs::metadata(path)
        .and_then(|m| m.modified())
        .map_err(invalid)?
        .into();
    Ok(Some(Scan {
        preview: json!({"session_id":sid,"adapter_id":adapter,"source_path":path,"title":title,"message_count":messages.len(),"created_at":created.to_rfc3339(),"import_status":"read_only","missing_fields":[]}),
        messages,
    }))
}
fn parse_messages(rows: Vec<Value>, adapter: &str) -> Vec<Value> {
    let (mut messages, mut response, mut events) = (vec![], vec![], vec![]);
    for record in rows {
        let (mut obj, target) = if record["message"].is_object() {
            (record["message"].clone(), &mut messages)
        } else if adapter == "codex"
            && record["type"] == "response_item"
            && record["payload"].is_object()
        {
            let obj = record["payload"].clone();
            if obj["type"] == "function_call_output" {
                push_message(
                    &mut response,
                    "tool",
                    obj.get("output").unwrap_or(&json!("")).clone(),
                );
                continue;
            }
            if obj["type"] == "function_call" {
                push_message(&mut response, "assistant", json!([obj]));
                continue;
            }
            (obj, &mut response)
        } else if adapter == "codex"
            && record["type"] == "event_msg"
            && record["payload"].is_object()
        {
            let p = &record["payload"];
            let role = match p["type"].as_str() {
                Some("user_message") => "user",
                Some("agent_message") => "assistant",
                _ => continue,
            };
            push_message(
                &mut events,
                role,
                p.get("message").cloned().unwrap_or(json!("")),
            );
            continue;
        } else {
            (record, &mut messages)
        };
        let role = obj["role"].as_str().unwrap_or("").to_owned();
        let content = obj
            .get_mut("content")
            .map(Value::take)
            .or_else(|| obj.get_mut("text").map(Value::take))
            .unwrap_or(json!(""));
        push_message(target, &role, content);
    }
    messages.extend(if response.is_empty() {
        events
    } else {
        response
    });
    messages
}
fn push_message(out: &mut Vec<Value>, role: &str, original: Value) {
    if role.is_empty() {
        return;
    }
    if let Some(content) = visible_content(role, &original) {
        let role = if role == "human" { "user" } else { role };
        out.push(
            json!({"role":role,"content":content,"source_role":role,"source_content":content}),
        );
    }
}
static ENVELOPE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?s)^<muteki_context key="muteki\.[a-z0-9][a-z0-9._-]*" kind="[a-z_]+" trust="(?:application|untrusted)">\n.*?\n</muteki_context>(?:\n\n|$)"#).unwrap()
});
static EXTERNAL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?s)^\s*<external_(muteki\.[a-z0-9][a-z0-9._-]*)>.*</external_(muteki\.[a-z0-9][a-z0-9._-]*)>\s*$").unwrap()
});
fn visible_text(mut text: &str) -> String {
    while let Some(m) = ENVELOPE.find(text) {
        text = &text[m.end()..];
    }
    if EXTERNAL.captures(text).is_some_and(|c| c[1] == c[2]) {
        String::new()
    } else {
        text.to_owned()
    }
}
fn context_block(block: &Value) -> bool {
    if block["type"] == "resource" {
        return block["resource"]["uri"]
            .as_str()
            .is_some_and(|s| s.starts_with("muteki://context/"));
    }
    if block["type"] != "text" {
        return false;
    }
    let text = block["text"].as_str().unwrap_or("");
    text.starts_with("<resource uri=\"muteki://context/")
        || text.starts_with("<system-reminder>\n<muteki_context ")
            && text.ends_with("</muteki_context>\n</system-reminder>")
        || text.starts_with("<muteki_context ") && visible_text(text).is_empty()
}
fn visible_content(role: &str, content: &Value) -> Option<Value> {
    if matches!(role, "system" | "developer") {
        return None;
    }
    if matches!(role, "assistant" | "agent") {
        if content["type"] == "thinking" || content["type"] == "redacted_thinking" {
            return None;
        }
        if let Some(blocks) = content.as_array() {
            let visible: Vec<_> = blocks.iter().filter(|block| {
                !matches!(block["type"].as_str(), Some("thinking" | "redacted_thinking"))
            }).cloned().collect();
            return if visible.is_empty() { None } else { Some(json!(visible)) };
        }
        return Some(content.clone());
    }
    if !matches!(role, "user" | "human") {
        return Some(content.clone());
    }
    if let Some(text) = content.as_str() {
        let text = visible_text(text);
        return if text.is_empty() {
            None
        } else {
            Some(json!(text))
        };
    }
    if let Some(blocks) = content.as_array() {
        let blocks: Vec<_> = blocks
            .iter()
            .filter_map(|b| {
                if context_block(b) {
                    return None;
                }
                let mut b = b.clone();
                if matches!(b["type"].as_str(), Some("text" | "input_text")) {
                    let text = visible_text(b["text"].as_str().unwrap_or(""));
                    if text.is_empty() {
                        return None;
                    }
                    b["text"] = json!(text);
                }
                Some(b)
            })
            .collect();
        return if blocks.is_empty() {
            None
        } else {
            Some(json!(blocks))
        };
    }
    Some(content.clone())
}

/// Remove reasoning blocks from older provider-history copies at startup.
/// The original provider files remain untouched; only imported read-only copies change.
pub(crate) fn sanitize_imported_document(document: &mut Value) -> bool {
    let provider_history = document["import_source"] == "provider_history";
    let Some(messages) = document["messages"].as_array_mut() else { return false };
    let mut changed = false;
    messages.retain_mut(|message| {
        let imported_claude = message["historical"] == true
            && message["source_provider"] == "claude"
            && matches!(message["source_role"].as_str(), Some("assistant" | "agent"));
        if !imported_claude { return true; }
        let original = &message["source_content"];
        let has_thinking = matches!(original["type"].as_str(), Some("thinking" | "redacted_thinking"))
            || original.as_array().is_some_and(|blocks| blocks.iter().any(|block| {
                matches!(block["type"].as_str(), Some("thinking" | "redacted_thinking"))
            }));
        if !has_thinking { return true; }
        let Some(content) = visible_content("assistant", original) else {
            changed = true;
            return false;
        };
        let text = text_of(&content);
        message["source_content"] = content;
        message["text"] = json!(text);
        changed = true;
        true
    });
    let count = messages.len();
    let provider = messages.first().and_then(|row| row["source_provider"].as_str()).unwrap_or("provider").to_owned();
    if changed && provider_history {
        document["thread"]["state"]["last_message_preview"] = json!(format!(
            "导入自 {} · {} 条历史消息",
            provider,
            count
        ));
    }
    changed
}
fn text_of(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .filter_map(|b| {
                if let Some(s) = b.as_str() {
                    Some(s.to_owned())
                } else if b.is_object() {
                    Some(match b["type"].as_str() {
                        Some("text" | "input_text" | "output_text") => {
                            b["text"].as_str().unwrap_or("").to_owned()
                        }
                        Some("tool_result") => text_of(&b["content"]),
                        _ => b.to_string(),
                    })
                } else {
                    None
                }
            })
            .collect::<Vec<_>>()
            .join("\n"),
        Value::Null => String::new(),
        _ => content.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn claude_history_never_projects_private_reasoning() {
        let parsed = parse_messages(vec![
            json!({"message":{"role":"user","content":"Question"}}),
            json!({"message":{"role":"assistant","content":[{"type":"thinking","thinking":"private"}]}}),
            json!({"message":{"role":"assistant","content":[{"type":"thinking","thinking":"private"},{"type":"text","text":"Answer"}]}}),
        ], "claude");
        assert_eq!(parsed.len(), 2);
        assert_eq!(text_of(&parsed[1]["content"]), "Answer");

        let mut imported = json!({"import_source":"provider_history","thread":{"state":{}},"messages":[
            {"role":"user","text":"Question","historical":true,"source_provider":"claude","source_role":"user","source_content":"Question","stream_seq":-3},
            {"role":"assistant","text":"private","historical":true,"source_provider":"claude","source_role":"assistant","source_content":[{"type":"thinking","thinking":"private"}],"stream_seq":-2},
            {"role":"assistant","text":"Answer","historical":true,"source_provider":"claude","source_role":"assistant","source_content":[{"type":"text","text":"Answer"}],"stream_seq":-1}
        ]});
        assert!(sanitize_imported_document(&mut imported));
        assert_eq!(imported["messages"].as_array().unwrap().len(), 2);
        assert_eq!(imported["messages"][1]["text"], "Answer");
        assert!(!imported.to_string().contains("private"));
    }
    #[test]
    fn synthetic_legacy_identity_reuses_matching_thread_and_rejects_conflicts() {
        let dir = tempfile::tempdir().unwrap();
        let platform = dir.path().join("legacy/_platform");
        fs::create_dir_all(&platform).unwrap();
        let conn = Connection::open(platform.join("platform.db")).unwrap();
        conn.execute_batch("CREATE TABLE agent_sessions(external_session_id TEXT, adapter_id TEXT, thread_id TEXT);
            INSERT INTO agent_sessions VALUES ('old-source','claude','missing-thread'), ('old-source','claude','legacy-thread');").unwrap();
        drop(conn);
        let store = crate::store::Store::open(":memory:").unwrap();
        store
            .put("system", "state_dir", &json!(dir.path()))
            .unwrap();
        let legacy = json!({"thread":{"thread_id":"legacy-thread","state":{}},"messages":[{"message_id":"original-message","role":"user","text":"Historical question","stream_seq":-1},{"message_id":"local-followup","role":"assistant","text":"Local answer","stream_seq":1}]});
        store
            .put("conversation_threads", "legacy-thread", &legacy)
            .unwrap();
        let conn = legacy_snapshot(&store).unwrap().unwrap();
        assert_eq!(
            legacy_thread(Some(&conn), &store, "claude", "old-source").unwrap(),
            Some("legacy-thread".into())
        );
        assert!(
            legacy_thread(Some(&conn), &store, "codex", "old-source")
                .unwrap()
                .is_none()
        );
        let scan = Scan {
            preview: json!({"session_id":"old-source","adapter_id":"claude","title":"Historical question","created_at":"2026-01-01T00:00:00Z"}),
            messages: parse_messages(
                vec![json!({"role":"human","content":"Historical question"})],
                "claude",
            ),
        };
        let key = "claude:old-source";
        let identity = json!({"legacy_thread_id":"legacy-thread"});
        let (saved, skipped) = store
            .import_provider_history(key, &identity, document(&scan, key, ""))
            .unwrap()
            .unwrap();
        assert!(skipped);
        assert_eq!(saved, legacy);
        assert_eq!(store.list("conversation_threads").unwrap().len(), 1);
        assert_eq!(
            store
                .get("provider_history_identities", key)
                .unwrap()
                .unwrap()["legacy_thread_id"],
            "legacy-thread"
        );
        let mut conflict = scan.clone();
        conflict.messages[0]["content"] = json!("Different source question");
        assert!(
            store
                .import_provider_history(key, &json!({}), document(&conflict, key, ""))
                .unwrap()
                .is_err()
        );
        assert_eq!(
            store
                .get("conversation_threads", "legacy-thread")
                .unwrap()
                .unwrap(),
            legacy
        );
        let mut grown = scan.clone();
        grown.messages.extend(parse_messages(
            vec![json!({"role":"assistant","content":"New source answer"})],
            "claude",
        ));
        let (saved, skipped) = store
            .import_provider_history(key, &json!({}), document(&grown, key, ""))
            .unwrap()
            .unwrap();
        assert!(!skipped);
        assert_eq!(saved["messages"][0]["message_id"], "original-message");
        assert_eq!(saved["messages"][1]["text"], "New source answer");
        assert_eq!(saved["messages"][2]["message_id"], "local-followup");
        assert_eq!(store.list("conversation_threads").unwrap().len(), 1);

        let mut complete = legacy.clone();
        complete["thread"]["thread_id"] = json!("complete-legacy");
        complete["messages"][0]["stream_seq"] = json!(-2);
        complete["messages"][1]["stream_seq"] = json!(-1);
        store
            .put("conversation_threads", "complete-legacy", &complete)
            .unwrap();
        let identity = json!({"legacy_thread_id":"complete-legacy"});
        let short_key = "claude:short-preview";
        assert!(
            store
                .import_provider_history(short_key, &identity, document(&scan, short_key, ""))
                .unwrap()
                .unwrap()
                .1
        );
        let mut full = scan.clone();
        full.messages.extend(parse_messages(
            vec![json!({"role":"assistant","content":"Local answer"})],
            "claude",
        ));
        let (retained, skipped) = store
            .import_provider_history(short_key, &identity, document(&full, short_key, ""))
            .unwrap()
            .unwrap();
        assert!(skipped);
        assert_eq!(retained, complete);
    }
    #[test]
    fn synthetic_history_preserves_canonical_content_and_recovers_growth() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("synthetic-codex.jsonl");
        let envelope = "<muteki_context key=\"muteki.rules\" kind=\"rules\" trust=\"application\">\nhidden\n</muteki_context>\n\nShow the history";
        let rows = vec![
            json!({"type":"response_item","payload":{"role":"developer","content":"hidden developer"}}),
            json!({"type":"event_msg","payload":{"type":"user_message","message":"mirror"}}),
            json!({"type":"response_item","payload":{"role":"user","content":[{"type":"input_text","text":envelope},{"type":"resource","resource":{"uri":"muteki://context/test","text":"hidden resource"}}]}}),
            json!({"type":"response_item","payload":{"type":"function_call","name":"synthetic_tool","arguments":"{}"}}),
            json!({"type":"response_item","payload":{"type":"function_call_output","output":"synthetic output"}}),
            json!({"type":"response_item","payload":{"role":"assistant","content":[{"type":"output_text","text":"A historical answer"}]}}),
        ];
        let write = |rows: &[Value]| {
            fs::write(
                &path,
                rows.iter()
                    .map(Value::to_string)
                    .collect::<Vec<_>>()
                    .join("\n"),
            )
            .unwrap()
        };
        write(&rows);
        let (scans, failures) = scan_directory(dir.path(), "codex", 100, false, true).unwrap();
        assert!(failures.is_empty());
        let first = &scans[0];
        assert_eq!(first.messages.len(), 4);
        assert_eq!(first.preview["title"], "Show the history");
        assert_eq!(
            first.messages[0]["source_content"],
            json!([{ "type": "input_text", "text": "Show the history" }])
        );
        assert_eq!(
            first.messages[0]["source_content"],
            first.messages[0]["content"]
        );
        assert_eq!(first.messages[2]["role"], "tool");
        let store = crate::store::Store::open(":memory:").unwrap();
        let key = "codex:synthetic-codex";
        let identity = json!({"adapter_id":"codex","external_session_id":"synthetic-codex"});
        let (saved, skip) = store
            .import_provider_history(key, &identity, document(first, key, "project-synthetic"))
            .unwrap()
            .unwrap();
        assert!(!skip);
        let tid = saved["thread"]["thread_id"].as_str().unwrap();
        assert_eq!(saved["thread"]["project_id"], "project-synthetic");
        assert_eq!(
            store
                .get("provider_history_identities", key)
                .unwrap()
                .unwrap()["thread_id"],
            tid
        );
        assert!(
            store
                .import_provider_history(key, &identity, document(first, key, ""))
                .unwrap()
                .unwrap()
                .1
        );
        let mut local = saved.clone();
        local["messages"].as_array_mut().unwrap().push(json!({"message_id":"local-after-import","role":"user","text":"Local follow-up","stream_seq":1}));
        store.put("conversation_threads", tid, &local).unwrap();
        let mut grown = rows.clone();
        grown.push(json!({"type":"response_item","payload":{"role":"assistant","content":"New historical suffix"}}));
        write(&grown);
        let current = read_scan(&path, "codex").unwrap().unwrap();
        assert_ne!(first.messages.len(), current.messages.len());
        let (saved, skip) = store
            .import_provider_history(key, &identity, document(&current, key, ""))
            .unwrap()
            .unwrap();
        assert!(!skip);
        assert_eq!(saved["thread"]["thread_id"], tid);
        assert_eq!(saved["messages"].as_array().unwrap().len(), 6);
        assert_eq!(saved["messages"][5]["message_id"], "local-after-import");
        assert_eq!(saved["messages"][4]["stream_seq"], -1);
        let mut conflict = current.clone();
        conflict.messages[0]["source_content"] = json!("changed content");
        assert!(
            store
                .import_provider_history(key, &identity, document(&conflict, key, ""))
                .unwrap()
                .is_err()
        );
        assert_eq!(
            store.get("conversation_threads", tid).unwrap().unwrap(),
            saved
        );
        assert_eq!(parse_messages(vec![json!({"type":"event_msg","payload":{"type":"agent_message","message":"old event-only"}})],"codex").len(),1);
        fs::write(
            dir.path().join("synthetic-claude.jsonl"),
            json!({"message":{"role":"human","content":"Claude history"}}).to_string(),
        )
        .unwrap();
        let claude = scan_directory(dir.path(), "claude", 100, false, true)
            .unwrap()
            .0;
        let claude_session = claude
            .iter()
            .find(|s| s.preview["session_id"] == "synthetic-claude")
            .unwrap();
        assert_eq!(claude_session.messages[0]["role"], "user");
        assert_eq!(claude_session.messages[0]["source_role"], "user");
        let claude_doc = document(claude_session, "claude:synthetic-claude", "");
        assert_eq!(claude_doc["messages"][0]["role"], "user");
        assert_eq!(claude_doc["messages"][0]["source_role"], "user");
        assert_eq!(claude_doc["messages"][0]["kind"], "message");
        fs::write(dir.path().join("broken.jsonl"), "{broken").unwrap();
        assert_eq!(
            scan_directory(dir.path(), "codex", 100, true, true)
                .unwrap()
                .1
                .len(),
            1
        );
    }
}
