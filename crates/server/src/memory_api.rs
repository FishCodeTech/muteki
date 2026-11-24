//! Explicitly consented, thread-scoped memory management over immutable imports.
use crate::{AppState, store::Store};
use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::StatusCode,
    routing::{delete, get},
};
use rusqlite::{Connection, OpenFlags};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};
use uuid::Uuid;

static WRITER: Mutex<()> = Mutex::new(());
type Failure = (StatusCode, Json<Value>);
type Result<T> = std::result::Result<T, Failure>;
const COLLECTION: &str = "conversation_memory_records";

pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/threads/{id}/memory", get(list).post(record))
        .route("/api/threads/{id}/memory/{memory}", delete(remove))
}
fn fail(status: StatusCode, code: &str, message: &str) -> Failure {
    let category = match status {
        StatusCode::BAD_REQUEST | StatusCode::UNPROCESSABLE_ENTITY => "validation",
        StatusCode::NOT_FOUND => "not_found",
        StatusCode::FORBIDDEN | StatusCode::UNAUTHORIZED => "permission",
        StatusCode::CONFLICT => "conflict",
        _ => "internal",
    };
    (
        status,
        Json(json!({"detail":message,"error":{"code":code,"message":message,"category":category}})),
    )
}
fn internal(_: impl std::fmt::Display) -> Failure {
    fail(
        StatusCode::INTERNAL_SERVER_ERROR,
        "conversation.memory.unavailable",
        "无法读取或保存长期记忆",
    )
}
fn text<'a>(v: &'a Value, key: &str) -> &'a str {
    v[key].as_str().unwrap_or("")
}
fn require_thread(store: &Store, id: &str) -> Result<()> {
    if store
        .get("conversation_threads", id)
        .map_err(internal)?
        .is_none()
    {
        return Err(fail(
            StatusCode::NOT_FOUND,
            "conversation.thread.not_found",
            "对话不存在",
        ));
    }
    Ok(())
}
fn timestamp(value: Option<f64>) -> String {
    value
        .and_then(|ts| chrono::DateTime::from_timestamp_millis((ts * 1000.0) as i64))
        .map(|ts| ts.to_rfc3339())
        .unwrap_or_default()
}
/// Read only the migration-owned checkpointed copy, never the source installation.
fn imported(store: &Store, thread: &str) -> Result<(BTreeMap<String, Value>, u64)> {
    let mut rows = BTreeMap::new();
    let mut watermark = 0;
    for row in store.list("conversation_memories").map_err(internal)? {
        if text(&row, "thread_id") == thread && !text(&row, "memory_id").is_empty() {
            watermark = watermark.max(row["updated_seq"].as_u64().unwrap_or(0));
            rows.insert(text(&row, "memory_id").to_owned(), row);
        }
    }
    let Some(root) = store.get("system", "state_dir").map_err(internal)? else {
        return Ok((rows, watermark));
    };
    let Some(root) = root.as_str() else {
        return Err(internal("invalid state directory"));
    };
    let path = std::path::Path::new(root).join("legacy/_platform/memory.db");
    if !path.exists() {
        return Ok((rows, watermark));
    }
    let meta = path.symlink_metadata().map_err(internal)?;
    if !meta.is_file()
        || meta.file_type().is_symlink()
        || path.with_file_name("memory.db-wal").exists()
    {
        return Err(internal("memory snapshot is not checkpointed"));
    }
    let uri = reqwest::Url::from_file_path(&path).map_err(|_| internal("invalid snapshot path"))?;
    let conn = Connection::open_with_flags(
        format!("{uri}?mode=ro&immutable=1"),
        OpenFlags::SQLITE_OPEN_READ_ONLY
            | OpenFlags::SQLITE_OPEN_URI
            | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(internal)?;
    watermark = watermark.max(
        conn.query_row("SELECT COALESCE(MAX(seq),0) FROM graph_events", [], |r| {
            r.get::<_, u64>(0)
        })
        .map_err(internal)?,
    );
    let mut stmt = conn.prepare("SELECT m.memory_id,m.thread_id,m.kind,m.content,m.source_json,m.created_seq,m.updated_seq,m.deleted_seq,m.delete_reason,c.ts,u.ts FROM memory_items m LEFT JOIN graph_events c ON c.seq=m.created_seq LEFT JOIN graph_events u ON u.seq=m.updated_seq WHERE m.thread_id=?1 ORDER BY m.created_seq").map_err(internal)?;
    let imported = stmt.query_map([thread], |r| {
        let source: String = r.get(4)?;
        Ok(json!({"memory_id":r.get::<_,String>(0)?,"thread_id":r.get::<_,String>(1)?,"kind":r.get::<_,String>(2)?,"content":r.get::<_,String>(3)?,"source":serde_json::from_str::<Value>(&source).unwrap_or_else(|_| json!({})),"created_seq":r.get::<_,u64>(5)?,"updated_seq":r.get::<_,u64>(6)?,"deleted_seq":r.get::<_,Option<u64>>(7)?,"delete_reason":r.get::<_,Option<String>>(8)?,"created_at":timestamp(r.get(9)?),"updated_at":timestamp(r.get(10)?)}))
    }).map_err(internal)?;
    for row in imported {
        let row = row.map_err(internal)?;
        rows.insert(text(&row, "memory_id").to_owned(), row);
    }
    Ok((rows, watermark))
}
fn overlay(store: &Store, id: &str) -> Result<Value> {
    Ok(store
        .get(COLLECTION, id)
        .map_err(internal)?
        .unwrap_or_else(|| json!({"version":0,"values":{"rows":{},"commands":{},"watermark":0}})))
}
fn merged(store: &Store, id: &str, overlay: &Value) -> Result<(BTreeMap<String, Value>, u64)> {
    let (mut rows, base) = imported(store, id)?;
    if let Some(saved) = overlay["values"]["rows"].as_object() {
        for (key, row) in saved {
            if text(row, "thread_id") == id {
                rows.insert(key.clone(), row.clone());
            }
        }
    }
    Ok((
        rows,
        base.max(overlay["values"]["watermark"].as_u64().unwrap_or(0)),
    ))
}
#[derive(Default, Deserialize)]
struct Search {
    #[serde(default, alias = "query")]
    q: String,
    #[serde(default)]
    include_deleted: bool,
}
pub(crate) fn query_list(s: &AppState, id: &str, params: &Value) -> Result<Value> {
    let q = params["query"]
        .as_str()
        .or_else(|| params["q"].as_str())
        .unwrap_or("")
        .to_owned();
    let include_deleted = params["include_deleted"].as_bool().unwrap_or_else(|| {
        params["include_deleted"]
            .as_str()
            .is_some_and(|v| matches!(v, "true" | "1"))
    });
    list_inner(&s.store, id, &q, include_deleted)
}
fn list_inner(store: &Store, id: &str, raw_query: &str, include_deleted: bool) -> Result<Value> {
    require_thread(store, id)?;
    let (rows, watermark) = merged(store, id, &overlay(store, id)?)?;
    let needle = raw_query.trim().to_lowercase();
    let mut memories = vec![];
    let mut tombstones = vec![];
    for row in rows.into_values() {
        if !row["deleted_seq"].is_null() {
            tombstones.push(json!({"memory_id":row["memory_id"],"deleted_seq":row["deleted_seq"],"deleted_at":row["updated_at"],"reason":row["delete_reason"]}));
        } else if needle.is_empty()
            || text(&row, "content").to_lowercase().contains(&needle)
            || text(&row, "kind").to_lowercase().contains(&needle)
        {
            memories.push(row);
        }
    }
    memories.sort_by_key(|r| r["created_seq"].as_u64().unwrap_or(0));
    tombstones.sort_by_key(|r| r["deleted_seq"].as_u64().unwrap_or(0));
    let mut result = json!({"thread_id":id,"scope":format!("thread:{id}"),"query":raw_query,"memories":memories,"deleted_count":tombstones.len(),"watermark":watermark,"supported":true});
    if include_deleted {
        result["tombstones"] = json!(tombstones);
    }
    Ok(result)
}
async fn list(
    State(s): State<Arc<AppState>>,
    Path(id): Path<String>,
    Query(query): Query<Search>,
) -> Result<Json<Value>> {
    list_inner(&s.store, &id, &query.q, query.include_deleted).map(Json)
}
#[derive(Deserialize)]
struct Record {
    content: String,
    #[serde(default = "note")]
    kind: String,
    consent: bool,
    #[serde(default)]
    command_id: String,
    #[serde(default)]
    idempotency_key: String,
}
fn note() -> String {
    "note".into()
}
#[derive(Deserialize)]
struct Delete {
    confirm: bool,
    #[serde(default = "reason")]
    reason: String,
    #[serde(default)]
    command_id: String,
    #[serde(default)]
    idempotency_key: String,
}
fn reason() -> String {
    "user_requested".into()
}
fn identifiers(command: &str, idem: &str) -> Result<(String, String)> {
    if command.chars().count() > 240 || idem.chars().count() > 240 {
        return Err(fail(
            StatusCode::UNPROCESSABLE_ENTITY,
            "conversation.memory.invalid",
            "请求标识过长",
        ));
    }
    let command = if command.trim().is_empty() {
        format!("cmd_memory_{}", Uuid::new_v4())
    } else {
        command.trim().to_owned()
    };
    let idem = if idem.trim().is_empty() {
        command.clone()
    } else {
        idem.trim().to_owned()
    };
    Ok((command, idem))
}
fn replay(
    overlay: &Value,
    command: &str,
    idem: &str,
    request: &Value,
) -> Result<Option<Json<Value>>> {
    if let Some(commands) = overlay["values"]["commands"].as_object() {
        for saved in commands.values() {
            if text(saved, "command_id") == command || text(saved, "idempotency_key") == idem {
                if saved["request"] != *request {
                    return Err(fail(
                        StatusCode::CONFLICT,
                        "conversation.memory.command_conflict",
                        "请求标识已用于其他操作",
                    ));
                }
                let mut response = saved["response"].clone();
                response["receipt"]["deduplicated"] = json!(true);
                return Ok(Some(Json(response)));
            }
        }
    }
    Ok(None)
}
fn commit(
    store: &Store,
    id: &str,
    mut overlay: Value,
    row: Value,
    seq: u64,
    command: String,
    idem: String,
    request: Value,
) -> Result<Json<Value>> {
    let response = json!({"receipt":{"command_id":command,"command_type":request["command_type"],"state":"completed","aggregate":{"type":"thread","id":id},"output":{"memory_id":row["memory_id"],"graph_stream_seq":seq},"deduplicated":false}});
    let key = text(&row, "memory_id").to_owned();
    overlay["values"]["rows"][key] = row;
    overlay["values"]["watermark"] = json!(seq);
    overlay["values"]["commands"][&command] =
        json!({"command_id":command,"idempotency_key":idem,"request":request,"response":response});
    let saved = store
        .compare_and_put(
            COLLECTION,
            id,
            overlay["version"].as_u64().unwrap_or(0),
            overlay["values"].clone(),
        )
        .map_err(internal)?;
    if saved.is_err() {
        return Err(fail(
            StatusCode::CONFLICT,
            "conversation.memory.version_conflict",
            "记忆已变化，请重试",
        ));
    }
    Ok(Json(response))
}
async fn record(
    State(s): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(body): Json<Record>,
) -> Result<Json<Value>> {
    require_thread(&s.store, &id)?;
    if !body.consent {
        return Err(fail(
            StatusCode::FORBIDDEN,
            "conversation.memory.consent_required",
            "写入长期记忆需要用户明确允许",
        ));
    }
    let content = body.content.trim();
    if content.is_empty() || body.content.chars().count() > 20_000 || body.kind.chars().count() > 80
    {
        return Err(fail(
            StatusCode::UNPROCESSABLE_ENTITY,
            "conversation.memory.invalid",
            "记忆内容不能为空且最多 20000 字，类型最多 80 字",
        ));
    }
    let (command, idem) = identifiers(&body.command_id, &body.idempotency_key)?;
    let request = json!({"command_type":"conversation.memory.record","thread_id":id,"content":content,"kind":body.kind,"consent":true});
    let _guard = WRITER.lock().map_err(internal)?;
    let overlay = overlay(&s.store, &id)?;
    if let Some(response) = replay(&overlay, &command, &idem, &request)? {
        return Ok(response);
    }
    let (_, watermark) = merged(&s.store, &id, &overlay)?;
    let seq = watermark
        .checked_add(1)
        .ok_or_else(|| internal("sequence overflow"))?;
    let now = chrono::Utc::now().to_rfc3339();
    let row = json!({"memory_id":format!("mem-{}",Uuid::new_v4()),"thread_id":id,"content":content,"kind":if body.kind.is_empty(){"note"}else{&body.kind},"source":{"actor":"local-user","thread_id":id,"via":"builtin.conversation"},"created_seq":seq,"updated_seq":seq,"created_at":now,"updated_at":now});
    commit(&s.store, &id, overlay, row, seq, command, idem, request)
}
async fn remove(
    State(s): State<Arc<AppState>>,
    Path((id, memory)): Path<(String, String)>,
    Json(body): Json<Delete>,
) -> Result<Json<Value>> {
    require_thread(&s.store, &id)?;
    if !body.confirm || body.reason.chars().count() > 240 {
        return Err(fail(
            StatusCode::UNPROCESSABLE_ENTITY,
            "conversation.memory.confirm_required",
            "删除长期记忆需要明确确认，删除原因最多 240 字",
        ));
    }
    let (command, idem) = identifiers(&body.command_id, &body.idempotency_key)?;
    let request = json!({"command_type":"conversation.memory.delete","thread_id":id,"memory_id":memory,"reason":body.reason,"confirm":true});
    let _guard = WRITER.lock().map_err(internal)?;
    let overlay = overlay(&s.store, &id)?;
    if let Some(response) = replay(&overlay, &command, &idem, &request)? {
        return Ok(response);
    }
    let (mut rows, watermark) = merged(&s.store, &id, &overlay)?;
    let mut row = rows
        .remove(&memory)
        .filter(|r| r["deleted_seq"].is_null())
        .ok_or_else(|| {
            fail(
                StatusCode::NOT_FOUND,
                "conversation.memory.not_found",
                "此对话中没有这条有效记忆",
            )
        })?;
    let seq = watermark
        .checked_add(1)
        .ok_or_else(|| internal("sequence overflow"))?;
    row["deleted_seq"] = json!(seq);
    row["updated_seq"] = json!(seq);
    row["delete_reason"] = json!(if body.reason.is_empty() {
        "user_requested"
    } else {
        &body.reason
    });
    row["updated_at"] = json!(chrono::Utc::now().to_rfc3339());
    commit(&s.store, &id, overlay, row, seq, command, idem, request)
}

pub(crate) fn record_from_command(
    s: &AppState,
    id: &str,
    payload: &Value,
    body: &Value,
) -> Result<(String, u64, String)> {
    let raw_content = payload["content"].as_str().unwrap_or("");
    let content = raw_content.trim();
    if content.is_empty() {
        return Err(fail(
            StatusCode::BAD_REQUEST,
            "conversation.memory.content_required",
            "memory.record requires payload.content",
        ));
    }
    if payload["consent"] != true {
        return Err(fail(
            StatusCode::FORBIDDEN,
            "conversation.memory.consent_required",
            "写入长期记忆需要用户明确允许",
        ));
    }
    let kind = payload["kind"]
        .as_str()
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .unwrap_or("note")
        .to_owned();
    if raw_content.chars().count() > 20_000 || kind.chars().count() > 80 {
        return Err(fail(
            StatusCode::BAD_REQUEST,
            "conversation.memory.invalid",
            "记忆内容不能为空且最多 20000 字，类型最多 80 字",
        ));
    }
    let memory_id = payload["memory_id"]
        .as_str()
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| format!("mem-{}", Uuid::new_v4()));
    let actor = body["actor"]["id"].as_str().unwrap_or("local-user");
    let (command, idem) = identifiers(text(body, "command_id"), text(body, "idempotency_key"))?;
    let request = json!({"command_type":"conversation.memory.record","thread_id":id,"content":content,"kind":kind,"consent":true});
    let _guard = WRITER.lock().map_err(internal)?;
    let overlay = overlay(&s.store, id)?;
    if let Some(Json(saved)) = replay(&overlay, &command, &idem, &request)? {
        let mid = saved["receipt"]["output"]["memory_id"]
            .as_str()
            .unwrap_or(&memory_id)
            .to_owned();
        let seq = saved["receipt"]["output"]["graph_stream_seq"]
            .as_u64()
            .unwrap_or(0);
        return Ok((mid, seq, kind));
    }
    let (_, watermark) = merged(&s.store, id, &overlay)?;
    let seq = watermark
        .checked_add(1)
        .ok_or_else(|| internal("sequence overflow"))?;
    let now = chrono::Utc::now().to_rfc3339();
    let row = json!({"memory_id":memory_id,"thread_id":id,"content":content,"kind":kind,"source":{"actor":actor,"thread_id":id,"via":"builtin.conversation"},"created_seq":seq,"updated_seq":seq,"created_at":now,"updated_at":now});
    let _ = commit(&s.store, id, overlay, row, seq, command, idem, request)?;
    Ok((memory_id, seq, kind))
}

pub(crate) fn delete_from_command(
    s: &AppState,
    id: &str,
    payload: &Value,
    body: &Value,
) -> Result<(String, u64)> {
    let memory = payload["memory_id"].as_str().map(str::trim).unwrap_or("");
    if memory.is_empty() {
        return Err(fail(
            StatusCode::BAD_REQUEST,
            "conversation.memory.id_required",
            "memory.delete requires payload.memory_id",
        ));
    }
    if payload["confirm"] != true {
        return Err(fail(
            StatusCode::BAD_REQUEST,
            "conversation.memory.confirm_required",
            "删除长期记忆需要明确确认",
        ));
    }
    let del_reason = payload["reason"]
        .as_str()
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .unwrap_or("user_requested");
    if del_reason.chars().count() > 240 {
        return Err(fail(
            StatusCode::BAD_REQUEST,
            "conversation.memory.invalid",
            "删除原因最多 240 字",
        ));
    }
    let (command, idem) = identifiers(text(body, "command_id"), text(body, "idempotency_key"))?;
    let request = json!({"command_type":"conversation.memory.delete","thread_id":id,"memory_id":memory,"reason":del_reason,"confirm":true});
    let _guard = WRITER.lock().map_err(internal)?;
    let overlay = overlay(&s.store, id)?;
    if let Some(Json(saved)) = replay(&overlay, &command, &idem, &request)? {
        let seq = saved["receipt"]["output"]["graph_stream_seq"]
            .as_u64()
            .unwrap_or(0);
        return Ok((memory.to_owned(), seq));
    }
    let (mut rows, watermark) = merged(&s.store, id, &overlay)?;
    let mut row = rows
        .remove(memory)
        .filter(|r| r["deleted_seq"].is_null())
        .ok_or_else(|| {
            fail(
                StatusCode::NOT_FOUND,
                "conversation.memory.not_found",
                "此对话中没有这条有效记忆",
            )
        })?;
    let seq = watermark
        .checked_add(1)
        .ok_or_else(|| internal("sequence overflow"))?;
    row["deleted_seq"] = json!(seq);
    row["updated_seq"] = json!(seq);
    row["delete_reason"] = json!(del_reason);
    row["updated_at"] = json!(chrono::Utc::now().to_rfc3339());
    let _ = commit(&s.store, id, overlay, row, seq, command, idem, request)?;
    Ok((memory.to_owned(), seq))
}
