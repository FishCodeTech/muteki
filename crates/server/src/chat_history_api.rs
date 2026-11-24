//! Read-only search, process replay and recorded context for CHAT history.
use crate::{AppState, conversation_view::select, history_import::read_db};
use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::StatusCode,
    routing::get,
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{collections::HashMap, path::PathBuf, sync::Arc};

type Params = HashMap<String, String>;
type Error = (StatusCode, Json<Value>);
type Api = Result<Json<Value>, Error>;

pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/threads/search", get(search))
        .route("/api/threads/{id}/turns/{turn}/process", get(process))
        .route("/api/threads/{id}/turns/{turn}/context", get(context))
}

fn fail(status: StatusCode, code: &str, message: &str) -> Error {
    (
        status,
        Json(json!({"error":{"code":code,"message":message}})),
    )
}
fn database(_: impl std::fmt::Display) -> Error {
    fail(
        StatusCode::INTERNAL_SERVER_ERROR,
        "conversation.history.unavailable",
        "Could not read recorded conversation history",
    )
}
fn missing() -> Error {
    fail(
        StatusCode::NOT_FOUND,
        "conversation.turn.not_found",
        "Thread or turn not found",
    )
}
fn number(p: &Params, key: &str, default: u64) -> Result<u64, Error> {
    p.get(key)
        .map(|v| {
            v.parse().map_err(|_| {
                fail(
                    StatusCode::BAD_REQUEST,
                    "conversation.query.invalid",
                    "Invalid pagination parameter",
                )
            })
        })
        .unwrap_or(Ok(default))
}
fn rows<'a>(v: &'a Value, key: &str) -> &'a [Value] {
    v[key].as_array().map(Vec::as_slice).unwrap_or(&[])
}
fn text<'a>(v: &'a Value, key: &str) -> &'a str {
    v[key].as_str().unwrap_or("")
}
fn thread_turn(s: &AppState, tid: &str, turn_id: &str) -> Result<(Value, Value), Error> {
    let doc = s
        .store
        .get("conversation_threads", tid)
        .map_err(database)?
        .ok_or_else(missing)?;
    let turn = rows(&doc, "turns")
        .iter()
        .find(|t| t["turn_id"] == turn_id)
        .cloned()
        .ok_or_else(missing)?;
    Ok((doc, turn))
}
fn history_root(s: &AppState) -> Result<Option<PathBuf>, Error> {
    Ok(s.store
        .get("system", "history_root")
        .map_err(database)?
        .and_then(|v| v.as_str().map(PathBuf::from)))
}

/// Structured secrets and original provider source blocks are never public.
fn scrub(value: &mut Value) {
    match value {
        Value::Object(object) => {
            object.retain(|key, _| {
                let key = key.to_ascii_lowercase().replace(['-', '_', '.'], "");
                key != "auth"
                    && key != "authentication"
                    && !key.ends_with("token")
                    && ![
                        "sourcecontent",
                        "password",
                        "passwd",
                        "secret",
                        "apikey",
                        "accesstoken",
                        "refreshtoken",
                        "idtoken",
                        "privatekey",
                        "authorization",
                        "credential",
                        "cookie",
                        "authjson",
                        "clientsecret",
                        "bearertoken",
                    ]
                    .iter()
                    .any(|forbidden| key.contains(forbidden))
            });
            for v in object.values_mut() {
                scrub(v);
            }
        }
        Value::Array(values) => {
            for v in values {
                scrub(v);
            }
        }
        _ => {}
    }
}
fn public_turn(turn: &Value) -> Value {
    let mut v = select(
        turn,
        &[
            "turn_id",
            "seq",
            "kind",
            "status",
            "original_status",
            "text",
            "interaction_mode",
            "created_at",
            "completed_at",
            "usage",
            "context_delivery",
            "historical",
        ],
    );
    scrub(&mut v);
    v
}
fn public_event(event: &Value, stream_seq: u64) -> Value {
    let mut payload = select(
        &event["payload"],
        &[
            "thread_id",
            "turn_id",
            "message_id",
            "tool_call_id",
            "call_id",
            "invocation_id",
            "tool_id",
            "name",
            "tool_name",
            "tool",
            "arguments",
            "input",
            "args",
            "output",
            "result",
            "chunk",
            "text",
            "delta",
            "summary",
            "status",
            "error",
            "is_error",
            "isError",
            "exit_code",
            "exitCode",
            "result_status",
            "outcome",
            "usage",
            "context_delivery",
            "artifact_id",
            "sha256",
            "media_type",
            "size",
            "kind",
            "role",
            "attachments",
            "created_at",
            "completed_at",
            "output_complete",
            "resources",
            "runtime",
            "phase",
            "message_phase",
            "channel",
            "replace",
            "thinking",
            "reasoning_summary",
            "item_id",
            "partial",
            "duration_ms",
            "elapsed_ms",
            "agent_id",
            "parent_id",
            "parent_agent_id",
            "parent_call_id",
            "parent_tool_use_id",
            "is_agent",
            "isAgent",
            "agent",
            "title",
            "model",
            "trigger",
            "confirmed",
            "message",
            "tokens_before",
            "tokens_after",
            "superseded_turn_ids",
            "replacement_turn_id",
            "retry_of_turn_id",
            "edited",
            "workspace_policy",
        ],
    );
    let native = select(&event["payload"]["native"], &[
        "command_actions", "mcp_server", "output_unavailable",
    ]);
    if native.as_object().is_some_and(|fields| !fields.is_empty()) {
        payload["native"] = native;
    }
    scrub(&mut payload);
    json!({"event_id":event["event_id"],"event_type":event["event_type"],"occurred_at":event["occurred_at"],"seq":stream_seq,"stream_seq":stream_seq,"payload":payload})
}

pub(crate) async fn search(State(s): State<Arc<AppState>>, Query(p): Query<Params>) -> Api {
    let query = p.get("q").map(String::as_str).unwrap_or("").trim();
    let limit = number(&p, "limit", 30)?.clamp(1, 100) as usize;
    let offset = number(&p, "offset", 0)? as usize;
    let cjk = query
        .chars()
        .any(|c| ('\u{4e00}'..='\u{9fff}').contains(&c));
    if query.is_empty() || (!cjk && query.chars().count() < 2) {
        return Ok(Json(
            json!({"query":query,"count":0,"hits":[],"reason":"query_too_short","offset":offset,"limit":limit,"has_more":false,"next_offset":null}),
        ));
    }
    let include_archived = p
        .get("include_archived")
        .is_some_and(|v| v == "true" || v == "1");
    let include_superseded = p
        .get("include_superseded")
        .is_some_and(|v| v == "true" || v == "1");
    let include_src = p
        .get("include_src")
        .is_some_and(|v| v == "true" || v == "1");
    let needle = query.to_lowercase();
    let mut hits = Vec::new();
    for doc in s.store.list("conversation_threads").map_err(database)? {
        let thread = &doc["thread"];
        if !include_src && thread["mode"] == "src" {
            continue;
        }
        let archived = thread["state"]["status"] == "archived";
        if crate::chat::is_ctf_thread(thread) || crate::chat::is_side_chat(thread) {
            continue;
        }
        if (archived && !include_archived)
            || p.get("project_id")
                .is_some_and(|id| !id.is_empty() && thread["project_id"] != id.as_str())
        {
            continue;
        }
        for message in rows(&doc, "messages") {
            let body = text(message, "text");
            if !body.to_lowercase().contains(&needle) {
                continue;
            }
            let superseded = rows(&doc, "turns").iter().any(|turn| {
                turn["turn_id"] == message["turn_id"] && turn["status"] == "superseded"
            });
            if superseded && !include_superseded {
                continue;
            }
            let chars: Vec<char> = body.chars().collect();
            let match_at = chars
                .iter()
                .enumerate()
                .find_map(|(index, _)| {
                    let tail: String = chars[index..]
                        .iter()
                        .take(query.chars().count() + 4)
                        .collect();
                    tail.to_lowercase().starts_with(&needle).then_some(index)
                })
                .unwrap_or(0);
            let start = match_at.saturating_sub(42);
            let end = (match_at + query.chars().count() + 42).min(chars.len());
            let snippet = format!(
                "{}{}{}",
                if start > 0 { "…" } else { "" },
                chars[start..end].iter().collect::<String>(),
                if end < chars.len() { "…" } else { "" }
            );
            hits.push((text(message,"created_at").to_owned(), json!({"thread_id":thread["thread_id"],"message_id":message["message_id"],"turn_id":message["turn_id"],"role":message["role"].as_str().unwrap_or("user"),"kind":"message","stream_seq":message["stream_seq"].as_u64().unwrap_or(0),"thread_title":thread["title"].as_str().unwrap_or(""),"project_id":thread["project_id"].as_str().unwrap_or(""),"archived":archived,"superseded":superseded,"snippet":snippet})));
        }
    }
    hits.sort_by(|a, b| {
        b.0.cmp(&a.0)
            .then_with(|| b.1["stream_seq"].as_u64().cmp(&a.1["stream_seq"].as_u64()))
            .then_with(|| text(&b.1, "message_id").cmp(text(&a.1, "message_id")))
    });
    let has_more = hits.len().saturating_sub(offset) > limit;
    let page: Vec<Value> = hits
        .into_iter()
        .skip(offset)
        .take(limit)
        .map(|(_, v)| v)
        .collect();
    Ok(Json(
        json!({"query":query,"count":page.len(),"hits":page,"offset":offset,"limit":limit,"has_more":has_more,"next_offset":if has_more {Some(offset + limit)} else {None}}),
    ))
}

async fn process(
    State(s): State<Arc<AppState>>,
    Path((tid, turn_id)): Path<(String, String)>,
    Query(p): Query<Params>,
) -> Api {
    let (doc, turn) = thread_turn(&s, &tid, &turn_id)?;
    if turn["status"] == "superseded" {
        return Err(fail(
            StatusCode::BAD_REQUEST,
            "conversation.turn.superseded",
            "Turn is superseded",
        ));
    }
    let limit = number(&p, "limit", 2000)?.clamp(1, 2000) as usize;
    let after = number(&p, "after_seq", 0)?;
    let before = p.get("before_seq").map(|_| number(&p, "before_seq", 0)).transpose()?;
    if before == Some(0) || (before.is_some() && after > 0) {
        return Err(fail(StatusCode::BAD_REQUEST, "conversation.query.invalid", "Invalid process cursor"));
    }
    let requested = p
        .get("watermark")
        .map(|_| number(&p, "watermark", 0))
        .transpose()?;
    let mut events = Vec::new();
    let head;
    let historical = doc["historical"] == true && turn["process_recorded"] != true;
    if historical {
        let root = history_root(&s)?.ok_or_else(|| {
            fail(
                StatusCode::NOT_FOUND,
                "conversation.history.unavailable",
                "Imported history is unavailable",
            )
        })?;
        let conn = read_db(&root.join("_platform/platform.db")).map_err(database)?;
        head = conn.query_row("SELECT COALESCE(MAX(stream_seq),0) FROM domain_events WHERE aggregate_type='thread' AND aggregate_id=?1", [&tid], |r| r.get::<_,u64>(0)).map_err(database)?;
        let watermark = requested.unwrap_or(head).min(head);
        let through = before.map_or(watermark, |seq| (seq - 1).min(watermark));
        let order = if before.is_some() { "DESC" } else { "ASC" };
        let mut stmt = conn.prepare(&format!("SELECT stream_seq,payload FROM domain_events WHERE aggregate_type='thread' AND aggregate_id=?1 AND json_extract(payload,'$.payload.turn_id')=?2 AND stream_seq>?3 AND stream_seq<=?4 ORDER BY stream_seq {order} LIMIT ?5")).map_err(database)?;
        let records = stmt
            .query_map(
                rusqlite::params![tid, turn_id, after, through, limit + 1],
                |row| Ok((row.get::<_, u64>(0)?, row.get::<_, String>(1)?)),
            )
            .map_err(database)?;
        for record in records {
            let (seq, raw) = record.map_err(database)?;
            let event: Value = serde_json::from_str(&raw).map_err(database)?;
            events.push(public_event(&event, seq));
        }
    } else {
        head = doc["event_seq"].as_u64().unwrap_or(0);
        let watermark = requested.unwrap_or(head).min(head);
        let page = match before {
            Some(seq) => s.store.chat_events_before(&tid, &turn_id, (seq - 1).min(watermark), limit + 1),
            None => s.store.chat_events(&tid, Some(&turn_id), after, watermark, limit + 1),
        }.map_err(database)?;
        for mut event in page {
            let seq = event["stream_seq"].as_u64().unwrap_or(0);
            if event["event_type"] == "core.agent.event" {
                if let Some(tool) = crate::tool_resources::legacy_native(&event, &turn_id, seq, doc["thread"]["state"]["lineage"]["native_subagent"].is_string()) {
                    event["event_type"] = json!("core.tool.completed");
                    event["payload"] = tool;
                }
            } else {
                crate::tool_resources::legacy_projection(&mut event, &turn_id, seq);
            }
            events.push(public_event(&event, seq));
        }
    }
    let watermark = requested.unwrap_or(head).min(head);
    if after > watermark || requested.is_some_and(|v| v > head) {
        return Err(fail(
            StatusCode::BAD_REQUEST,
            "conversation.query.invalid",
            "Cursor is outside the recorded stream",
        ));
    }
    let more = events.len() > limit;
    events.truncate(limit);
    if before.is_some() {
        events.reverse();
    }
    let next = if more && before.is_none() {
        events.last().and_then(|v| v["stream_seq"].as_u64())
    } else {
        None
    };
    let previous = if more && before.is_some() {
        events.first().and_then(|v| v["stream_seq"].as_u64())
    } else {
        None
    };
    Ok(Json(
        json!({"thread_id":tid,"turn_id":turn_id,"turn":public_turn(&turn),"count":events.len(),"events":events,"truncated":more,"has_more":more,"after_seq":after,"next_after_seq":next,"next_before_seq":previous,"watermark":watermark,"recorded":historical || turn["process_recorded"] == true}),
    ))
}

fn digest(id: &str) -> String {
    format!("{:x}", Sha256::digest(id.as_bytes()))[..24].to_owned()
}
async fn context(
    State(s): State<Arc<AppState>>,
    Path((tid, turn_id)): Path<(String, String)>,
) -> Api {
    let (doc, turn) = thread_turn(&s, &tid, &turn_id)?;
    let mut ledger = turn
        .get("context_ledger")
        .filter(|v| v.is_object())
        .cloned();
    if ledger.is_none() && doc["historical"] == true {
        if let Some(root) = history_root(&s)? {
            let path = root
                .join("sessions/_context_ledger")
                .join(digest(&tid))
                .join(format!("{}.json", digest(&turn_id)));
            match std::fs::read(path) {
                Ok(bytes) => {
                    let value: Value = serde_json::from_slice(&bytes).map_err(database)?;
                    if value["thread_id"] != tid || value["turn_id"] != turn_id {
                        return Err(database("ledger identity mismatch"));
                    }
                    ledger = Some(value);
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(database(e)),
            }
        }
    }
    let recorded = ledger.is_some();
    let part_source = ledger
        .as_ref()
        .map(|v| rows(v, "parts"))
        .unwrap_or_else(|| rows(&turn, "context_delivery"));
    let parts: Vec<Value> = part_source
        .iter()
        .map(|part| {
            let mut part = select(
                part,
                &[
                    "key",
                    "kind",
                    "trust",
                    "scope",
                    "channel",
                    "artifact_id",
                    "sha256",
                    "chars",
                    "text",
                    "name",
                    "media_type",
                    "size",
                    "source_turn_id",
                ],
            );
            scrub(&mut part);
            part
        })
        .collect();
    let user_text = ledger
        .as_ref()
        .and_then(|v| v["user_text"].as_str())
        .unwrap_or_else(|| text(&turn, "text"));
    let mut result = json!({"thread_id":tid,"turn_id":turn_id,"user_text":user_text,"parts":parts,"recorded":recorded,"availability":if recorded {"recorded"} else if !part_source.is_empty() {"metadata_only"} else {"absent"}});
    if let Some(status) = ledger.as_ref().and_then(|v| v.get("delivery_status")) {
        result["delivery_status"] = status.clone();
    }
    if let Some(adapter) = ledger.as_ref().and_then(|v| v.get("adapter_id")) {
        result["adapter_id"] = adapter.clone();
    }
    Ok(Json(result))
}

#[cfg(test)]
mod context_tests {
    use super::*;

    #[tokio::test]
    async fn live_context_ledger_and_metadata_fallback_keep_distinct_availability() {
        let store = crate::store::Store::open(":memory:").unwrap();
        let s = Arc::new(AppState::new(store));
        let mut doc = json!({"turns":[{"turn_id":"turn","text":"user","context_delivery":[{"key":"attachment","chars":3}],"context_ledger":{"thread_id":"thread","turn_id":"turn","user_text":"user","delivery_status":"request_prepared","parts":[{"key":"attachment","text":"abc","chars":3,"authorization":"not public"}]}}]});
        s.store.put("conversation_threads", "thread", &doc).unwrap();
        let result = context(State(s.clone()), Path(("thread".into(), "turn".into())))
            .await
            .unwrap()
            .0;
        assert_eq!(result["parts"][0]["text"], "abc");
        assert_eq!(result["delivery_status"], "request_prepared");
        assert_eq!(result["recorded"], true);
        assert_eq!(result["availability"], "recorded");
        assert!(result["parts"][0].get("authorization").is_none());
        doc["turns"][0]
            .as_object_mut()
            .unwrap()
            .remove("context_ledger");
        s.store.put("conversation_threads", "thread", &doc).unwrap();
        let fallback = context(State(s), Path(("thread".into(), "turn".into())))
            .await
            .unwrap()
            .0;
        assert_eq!(fallback["recorded"], false);
        assert_eq!(fallback["availability"], "metadata_only");
    }
}
