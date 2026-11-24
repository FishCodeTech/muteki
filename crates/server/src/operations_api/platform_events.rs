//! Signed, aggregate-scoped event pages and bounded waits.
use super::*;
use axum::routing::post;
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use std::io::{BufRead, BufReader};

type EventRow = (u64, Value);
const MAX_PAGE: usize = 500;
const MAX_WAIT: f64 = 30.0;

fn cursor_key(state: &AppState) -> Result<Vec<u8>, Failure> {
    super::platform_cursor_key(state).map_err(storage)
}

fn cursor_scope(scope: &str) -> String {
    let text = scope.trim();
    if text.is_empty() { return String::new(); }
    format!("{:x}", Sha256::digest(text.as_bytes()))[..16].to_owned()
}

pub(super) fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/events/read", post(read))
        .route("/api/events/wait", post(wait))
}
fn text<'a>(value: &'a Value, key: &str) -> &'a str {
    value[key].as_str().unwrap_or("").trim()
}
fn request(
    body: Option<Json<Value>>,
) -> Result<(Value, String, String, usize, Option<String>), Failure> {
    let Some(Json(body)) = body else {
        return Err(fail(
            StatusCode::BAD_REQUEST,
            "platform.request.invalid",
            "Event request body is required",
        ));
    };
    if !body.is_object() {
        return Err(fail(
            StatusCode::BAD_REQUEST,
            "platform.request.invalid",
            "Event request must be an object",
        ));
    }
    let aggregate_type = text(&body, "aggregate_type").to_owned();
    let aggregate_id = text(&body, "aggregate_id").to_owned();
    if aggregate_type.is_empty() || aggregate_id.is_empty() {
        return Err(fail(
            StatusCode::BAD_REQUEST,
            "platform.request.invalid",
            "aggregate_type and aggregate_id are required",
        ));
    }
    if aggregate_type.len() > 128 || aggregate_id.len() > 256 {
        return Err(fail(
            StatusCode::BAD_REQUEST,
            "platform.request.invalid",
            "Aggregate identity is too long",
        ));
    }
    let limit = match body.get("limit") {
        None | Some(Value::Null) => 100,
        Some(value) => value
            .as_u64()
            .ok_or_else(|| {
                fail(
                    StatusCode::BAD_REQUEST,
                    "platform.request.invalid",
                    "limit must be a positive integer",
                )
            })?
            .clamp(1, MAX_PAGE as u64) as usize,
    };
    let cursor = match body.get("after_cursor") {
        None | Some(Value::Null) => None,
        Some(Value::String(value)) => Some(value.clone()),
        Some(_) => {
            return Err(fail(
                StatusCode::BAD_REQUEST,
                "platform.request.invalid",
                "after_cursor must be a string",
            ));
        }
    };
    Ok((body, aggregate_type, aggregate_id, limit, cursor))
}
pub(super) fn signed(
    state: &AppState,
    aggregate_type: &str,
    aggregate_id: &str,
    seq: u64,
    scope: &str,
) -> Result<String, Failure> {
    signed_positions(state,aggregate_type,aggregate_id,(seq,0),scope)
}
pub(super) fn signed_domain(state: &AppState, kind: &str, id: &str, seq: u64, scope: &str) -> Result<String, Failure> {
    let (_,native_head) = stream(state,kind,id,0,0)?;
    signed_positions(state,kind,id,(native_head,seq),scope)
}
fn signed_positions(
    state: &AppState, aggregate_type: &str, aggregate_id: &str, position: (u64,u64), scope: &str,
) -> Result<String, Failure> {
    let key = cursor_key(state)?;
    let scope_hash = cursor_scope(scope);
    let payload =
        json!({"at":aggregate_type,"aid":aggregate_id,"seq":position.0,"ds":position.1,"scp":scope_hash}).to_string();
    let mut mac = Hmac::<Sha256>::new_from_slice(&key).map_err(storage)?;
    mac.update(payload.as_bytes());
    let signature = mac.finalize().into_bytes();
    Ok(format!(
        "v1.{}.{}",
        URL_SAFE_NO_PAD.encode(payload),
        URL_SAFE_NO_PAD.encode(&signature[..24])
    ))
}
fn after(
    state: &AppState,
    cursor: Option<&str>,
    aggregate_type: &str,
    aggregate_id: &str,
    _scope: &str,
) -> Result<(u64,u64), Failure> {
    let Some(cursor) = cursor.filter(|text| !text.is_empty()) else {
        return Ok((0,0));
    };
    if cursor.len() > 4096 {
        return Err(fail(
            StatusCode::BAD_REQUEST,
            "event.cursor_invalid",
            "Event cursor is too long",
        ));
    }
    let parts: Vec<&str> = cursor.split('.').collect();
    if parts.len() != 3 || parts[0] != "v1" {
        return Err(fail(
            StatusCode::BAD_REQUEST,
            "event.cursor_invalid",
            "Malformed event cursor",
        ));
    }
    let payload = URL_SAFE_NO_PAD.decode(parts[1]).map_err(|_| {
        fail(
            StatusCode::BAD_REQUEST,
            "event.cursor_invalid",
            "Malformed event cursor",
        )
    })?;
    let signature = URL_SAFE_NO_PAD.decode(parts[2]).map_err(|_| {
        fail(
            StatusCode::BAD_REQUEST,
            "event.cursor_invalid",
            "Malformed event cursor",
        )
    })?;
    let key = cursor_key(state)?;
    let mut mac = Hmac::<Sha256>::new_from_slice(&key).map_err(storage)?;
    mac.update(&payload);
    let expected = mac.finalize().into_bytes();
    let mismatch = signature.len() != 24 || signature.iter().zip(&expected[..24])
        .fold(0u8, |difference, (actual, expected)| difference | (*actual ^ *expected)) != 0;
    if mismatch {
        return Err(fail(
            StatusCode::BAD_REQUEST,
            "event.cursor_invalid",
            "Event cursor signature is invalid",
        ));
    }
    let data: Value = serde_json::from_slice(&payload).map_err(|_| {
        fail(
            StatusCode::BAD_REQUEST,
            "event.cursor_invalid",
            "Malformed event cursor",
        )
    })?;
    if data["at"] != aggregate_type || data["aid"] != aggregate_id {
        return Err(fail(
            StatusCode::FORBIDDEN,
            "event.cursor_scope",
            "Event cursor belongs to another aggregate",
        ));
    }
    let seq = data["seq"].as_u64().ok_or_else(|| {
        fail(
            StatusCode::BAD_REQUEST,
            "event.cursor_invalid",
            "Malformed event cursor",
        )
    })?;
    let domain_seq = match data.get("ds") {
        None => 0,
        Some(value) => value.as_u64().ok_or_else(|| fail(StatusCode::BAD_REQUEST,"event.cursor_invalid","Malformed domain event cursor"))?,
    };
    Ok((seq,domain_seq))
}
fn run_time(raw: &Value) -> String {
    raw["ts"]
        .as_f64()
        .and_then(|seconds| {
            let whole = seconds.floor() as i64;
            let nanos = ((seconds - whole as f64) * 1_000_000_000.0).max(0.0) as u32;
            chrono::DateTime::<chrono::Utc>::from_timestamp(whole, nanos)
        })
        .map(|time| time.to_rfc3339())
        .unwrap_or_else(now)
}
fn stream(
    state: &AppState,
    aggregate_type: &str,
    aggregate_id: &str,
    after_seq: u64,
    limit: usize,
) -> Result<(Vec<EventRow>, u64), Failure> {
    if aggregate_type == "thread" {
        let doc = state
            .store
            .get("conversation_threads", aggregate_id)
            .map_err(storage)?
            .ok_or_else(|| {
                fail(
                    StatusCode::NOT_FOUND,
                    "thread.not_found",
                    "Thread not found",
                )
            })?;
        let head = doc["event_watermark"].as_u64().unwrap_or(0);
        let rows = state
            .store
            .chat_events(aggregate_id, None, after_seq, i64::MAX as u64, limit)
            .map_err(storage)?;
        return Ok((
            rows.into_iter()
                .filter_map(|row| row["seq"].as_u64().map(|seq| (seq, row)))
                .collect(),
            head,
        ));
    }
    if aggregate_type == "run" {
        let run = state
            .store
            .get("runs", aggregate_id)
            .map_err(storage)?
            .ok_or_else(|| fail(StatusCode::NOT_FOUND, "run.not_found", "Run not found"))?;
        let live = crate::runs::session_dir(state, aggregate_id).join("events.jsonl");
        let path = if live.is_file() {
            live
        } else if let Some(rel) = run["history_event_file"].as_str() {
            let root = state
                .store
                .get("system", "history_root")
                .map_err(storage)?
                .and_then(|v| v.as_str().map(PathBuf::from))
                .ok_or_else(|| {
                    fail(
                        StatusCode::NOT_FOUND,
                        "run.events_not_found",
                        "Recorded run events are unavailable",
                    )
                })?;
            let candidate = root.join(rel).canonicalize().map_err(storage)?;
            if !candidate.starts_with(root.canonicalize().map_err(storage)?) {
                return Err(fail(
                    StatusCode::FORBIDDEN,
                    "run.events_path_invalid",
                    "Recorded run event path is invalid",
                ));
            }
            candidate
        } else {
            return Ok((Vec::new(), 0));
        };
        let file = std::fs::File::open(path).map_err(storage)?;
        let mut rows = Vec::new();
        let mut head = 0;
        for line in BufReader::new(file).lines().map_while(Result::ok) {
            let Ok(raw) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            let seq = raw["seq"].as_u64().unwrap_or(head + 1);
            head = head.max(seq);
            if seq <= after_seq || rows.len() >= limit {
                continue;
            }
            let kind = text(&raw, "event_type");
            let kind = if ["core.", "run.", "ctf.", "pentest.", "src.", "ext."]
                .iter()
                .any(|prefix| kind.starts_with(prefix))
            {
                kind.to_owned()
            } else {
                format!("run.{kind}")
            };
            rows.push((seq,json!({"event_id":format!("evt-run-{aggregate_id}-{seq}"),"aggregate_type":"run","aggregate_id":aggregate_id,
                "stream_seq":seq,"event_type":kind,"occurred_at":run_time(&raw),
                "producer":raw["producer"].as_str().unwrap_or("builtin.single-security-task"),
                "actor_id":raw["actor_id"].as_str().unwrap_or("system"),
                "command_id":raw["command_id"],"causation_id":raw["causation_id"],
                "correlation_id":raw["correlation_id"],"idempotency_key":raw["idempotency_key"],
                "payload":raw["payload"]})));
        }
        return Ok((rows, head));
    }
    if aggregate_type == "task" {
        if state
            .store
            .get("src_tasks", aggregate_id)
            .map_err(storage)?
            .is_none()
        {
            let event = super::task_dispatch::event(state, aggregate_id)?
                .ok_or_else(|| fail(StatusCode::NOT_FOUND,"task.not_found","Task not found"))?;
            let seq = event["stream_seq"].as_u64().unwrap_or(1);
            let rows = if seq > after_seq && limit > 0 { vec![(seq,event)] } else { Vec::new() };
            return Ok((rows,seq));
        }
        let head = state
            .store
            .get("src_task_event_heads", aggregate_id)
            .map_err(storage)?
            .and_then(|row| row["version"].as_u64())
            .unwrap_or(0);
        let mut rows = state
            .store
            .list("src_task_events")
            .map_err(storage)?
            .into_iter()
            .filter(|row| row["aggregate_id"] == aggregate_id)
            .filter_map(|row| row["stream_seq"].as_u64().map(|seq| (seq, row)))
            .filter(|(seq, _)| *seq > after_seq)
            .collect::<Vec<_>>();
        rows.sort_by_key(|(seq, _)| *seq);
        rows.truncate(limit);
        return Ok((rows, head));
    }
    let mut events = state
        .store
        .list("competition_events_live")
        .map_err(storage)?;
    events.sort_by_key(|row| row["global_seq"].as_u64().unwrap_or(0));
    let mut rows = Vec::new();
    let mut head = 0;
    let mut stream_seq = 0;
    for raw in events.into_iter().filter(|row| {
        row["aggregate_type"] == aggregate_type && row["aggregate_id"] == aggregate_id
    }) {
        stream_seq += 1;
        let seq = raw["global_seq"].as_u64().unwrap_or(0);
        head = head.max(seq);
        if seq <= after_seq || rows.len() >= limit {
            continue;
        }
        let command_id = text(&raw, "command_id");
        rows.push((seq,json!({"event_id":format!("evt-comp-{aggregate_id}-{seq}"),"aggregate_type":aggregate_type,"aggregate_id":aggregate_id,
            "stream_seq":stream_seq,"event_type":raw["event_type"],"occurred_at":raw["occurred_at"],"producer":"builtin.competition",
            "actor_id":raw["actor_id"].as_str().filter(|actor|!actor.is_empty()).unwrap_or("system"),"command_id":if command_id.is_empty(){Value::Null}else{json!(command_id)},
            "causation_id":raw["causation_id"],"correlation_id":raw["correlation_id"],"idempotency_key":raw["idempotency_key"],"payload":raw["payload"]})));
    }
    Ok((rows, head))
}
pub(super) fn stream_version(
    state: &AppState,
    aggregate_type: &str,
    aggregate_id: &str,
) -> Result<u64, Failure> {
    if aggregate_id.is_empty() {
        return Ok(0);
    }
    if aggregate_type == "thread" {
        return Ok(state
            .store
            .get("conversation_threads", aggregate_id)
            .map_err(storage)?
            .and_then(|doc| doc["thread"]["state"]["head_stream_seq"].as_u64())
            .unwrap_or(0));
    }
    if aggregate_type == "run" {
        return stream(state, aggregate_type, aggregate_id, 0, 0).map(|(_, head)| head);
    }
    if aggregate_type == "task" {
        return Ok(state
            .store
            .get("src_task_event_heads", aggregate_id)
            .map_err(storage)?
            .and_then(|row| row["version"].as_u64())
            .unwrap_or(0));
    }
    let domain_head = state.store.list(super::domain_events::EVENTS).map_err(storage)?
        .iter().filter(|event| event["aggregate_type"] == aggregate_type && event["aggregate_id"] == aggregate_id)
        .filter_map(|event| event["stream_seq"].as_u64()).max().unwrap_or(0);
    Ok(domain_head.max(state
        .store
        .list("competition_events_live")
        .map_err(storage)?
        .iter()
        .filter(|event| {
            event["aggregate_type"] == aggregate_type && event["aggregate_id"] == aggregate_id
        })
        .count() as u64))
}
struct EventPage {
    events: Vec<Value>,
    position: (u64,u64),
    head: (u64,u64),
    more: bool,
}
fn page(state: &AppState, kind: &str, id: &str, after: (u64,u64), limit: usize) -> Result<EventPage, Failure> {
    use std::collections::VecDeque;
    let (native,native_head) = stream(state,kind,id,after.0,limit+1)?;
    let mut native: VecDeque<EventRow> = native.into();
    let mut domain: Vec<EventRow> = state.store.list(super::domain_events::EVENTS).map_err(storage)?
        .into_iter().filter(|event| event["aggregate_type"] == kind && event["aggregate_id"] == id)
        .filter_map(|event| event["stream_seq"].as_u64().map(|seq| (seq,event))).collect();
    domain.sort_by_key(|(seq,_)| *seq);
    let domain_head = domain.last().map(|(seq,_)| *seq).unwrap_or(0);
    let mut domain: VecDeque<EventRow> = domain.into_iter().filter(|(seq,_)| *seq > after.1).take(limit+1).collect();
    let mut position = after;
    let mut events = Vec::new();
    while events.len() < limit && (!native.is_empty() || !domain.is_empty()) {
        let take_domain = match (native.front(),domain.front()) {
            (None,_) => true,
            (_,None) => false,
            (Some((_,a)),Some((_,b))) => text(b,"occurred_at") < text(a,"occurred_at"),
        };
        let (seq,event) = if take_domain { domain.pop_front().unwrap() } else { native.pop_front().unwrap() };
        if take_domain { position.1 = seq; } else { position.0 = seq; }
        events.push(event);
    }
    Ok(EventPage { events, position, head:(native_head.max(after.0),domain_head.max(after.1)), more:!native.is_empty() || !domain.is_empty() })
}
pub(super) async fn read(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    gateway_candidate: Option<Extension<crate::access_api::GatewayCandidate>>,
    body: Option<Json<Value>>,
) -> Result<Json<Value>, Failure> {
    let (body,kind,id,limit,cursor) = request(body)?;
    let scope = platform_auth::authorize(&state,&headers,gateway_candidate.is_some(),&body,"event","",&kind,&id)?;
    let position = after(&state,cursor.as_deref(),&kind,&id,&scope)?;
    let page = page(&state,&kind,&id,position,limit)?;
    let next = if page.more { Some(signed_positions(&state,&kind,&id,page.position,&scope)?) } else { None };
    Ok(Json(json!({"events":page.events,"next_cursor":next})))
}

pub(super) async fn wait(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    gateway_candidate: Option<Extension<crate::access_api::GatewayCandidate>>,
    body: Option<Json<Value>>,
) -> Result<Json<Value>, Failure> {
    let (body, aggregate_type, aggregate_id, limit, cursor) = request(body)?;
    let timeout = body
        .get("timeout_seconds")
        .map_or(Ok(30.0), |value| {
            value
                .as_f64()
                .filter(|v| v.is_finite() && *v >= 0.0)
                .ok_or_else(|| {
                    fail(
                        StatusCode::BAD_REQUEST,
                        "platform.request.invalid",
                        "timeout_seconds must be a nonnegative number",
                    )
                })
        })?
        .min(MAX_WAIT);
    let scope = platform_auth::authorize(
        &state,
        &headers,
        gateway_candidate.is_some(),
        &body,
        "event",
        "",
        &aggregate_type,
        &aggregate_id,
    )?;
    let seq = after(
        &state,
        cursor.as_deref(),
        &aggregate_type,
        &aggregate_id,
        &scope,
    )?;
    let deadline = tokio::time::Instant::now() + Duration::from_secs_f64(timeout);
    loop {
        let page = page(&state,&aggregate_type,&aggregate_id,seq,limit)?;
        if !page.events.is_empty() {
            return Ok(Json(
                json!({"state":"events","events":page.events,
                "cursor":signed_positions(&state,&aggregate_type,&aggregate_id,page.position,&scope)?}),
            ));
        }
        if tokio::time::Instant::now() >= deadline {
            let cursor = if page.head != (0,0) {
                Some(signed_positions(
                    &state,
                    &aggregate_type,
                    &aggregate_id,
                    page.head,
                    &scope,
                )?)
            } else {
                cursor
            };
            return Ok(Json(json!({"state":"timeout","events":[],"cursor":cursor})));
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}
