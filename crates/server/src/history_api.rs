//! Historical read views and local workbench UI state overlays.
use crate::{
    AppState,
    history_import::{payload, read_db, table_rows},
    runs::{competition_api, src_api},
};
use anyhow::{Context, Result, bail};
use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use base64::Engine;
use futures_util::StreamExt;
use rusqlite::Connection;
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    fs,
    io::{BufRead, BufReader},
    path::PathBuf,
    sync::Arc,
};

type Params = HashMap<String, String>;
pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/runs/{id}", get(run_detail))
        .route("/api/runs/{id}/events", get(run_events))
        .route("/api/runs/{id}/events/history", get(run_history))
        .route("/api/runs/{id}/graph/export", get(graph_export))
        .route("/api/runs/{id}/pentest-report", get(pentest_report))
        .route("/api/runs/{id}/pentest-report.md", get(pentest_markdown))
        .route(
            "/api/runs/{id}/pentest-evidence/{seq}/artifacts/{artifact}",
            get(evidence),
        )
        .route(
            "/api/runs/{id}/pentest-evidence/{seq}/artifacts/{artifact}/{action}",
            get(evidence_action),
        )
        .route(
            "/api/runs/{id}/pentest-report-images/{poc}",
            get(report_image),
        )
        .route("/api/competitions/{id}", get(competition_detail))
        .route(
            "/api/competitions/{id}/challenges",
            get(competition_challenges),
        )
        .route(
            "/api/competitions/{id}/submissions",
            get(competition_submissions),
        )
        .route("/api/competitions/{id}/leases", get(competition_leases))
        .route(
            "/api/competitions/{id}/events/history",
            get(competition_history),
        )
        .route("/api/competitions/{id}/events", get(competition_events))
        .route("/api/platform-connections", get(connections))
        .route("/api/platform-kinds", get(platform_kinds))
        .route("/api/platform-secrets", get(platform_secrets))
        .route(
            "/api/platform-secrets/inspect",
            get(platform_secret_inspect),
        )
        .route("/api/src/tasks", get(src_tasks))
        .route(
            "/api/src/oast/callback/{token}",
            axum::routing::any(src_api::oast_callback)
                .layer(axum::extract::DefaultBodyLimit::disable()),
        )
        .route("/api/src/tasks/{id}", get(src_task))
        .route("/api/src/tasks/{id}/records", get(src_records))
        .route(
            "/api/src/tasks/{id}/records/{kind}/{record}",
            get(src_record),
        )
        .route("/api/src/tasks/{id}/policy", get(src_policy))
        .route("/api/src/tasks/{id}/capabilities", get(src_capabilities))
        .route(
            "/api/src/tasks/{id}/executions/{invocation}",
            get(src_execution),
        )
        .route("/api/src/tasks/{id}/runs", get(src_runs))
        .route("/api/src/tasks/{id}/runs", post(src_ensure_run))
        .route("/api/src/tasks/{id}/runs/{run}/start", post(src_start_run))
        .route(
            "/api/src/tasks/{id}/policy/preview",
            post(src_policy_preview),
        )
        .route("/api/src/tasks/{id}/queries/{kind}", post(src_query))
        .route(
            "/api/src/tasks/{id}/commands/{cmd}",
            post(src_command).layer(axum::extract::DefaultBodyLimit::disable()),
        )
        .route("/api/src/tasks/{id}/takeover", post(src_takeover))
        .route("/api/src/tasks/{id}/debug-logs", get(src_debug_logs))
}
fn response(result: Result<Value>) -> Response {
    match result {
        Ok(v) => Json(v).into_response(),
        Err(e) => {
            let code = if e.to_string().starts_with("unsupported") {
                StatusCode::NOT_IMPLEMENTED
            } else if e.to_string().contains("unavailable") {
                StatusCode::NOT_FOUND
            } else {
                tracing::error!(error=%e,"Historical read failed");
                StatusCode::INTERNAL_SERVER_ERROR
            };
            (code,Json(json!({"detail":e.to_string(),"error":{"code":"history.unavailable","message":e.to_string()}}))).into_response()
        }
    }
}
fn root(state: &AppState) -> Result<PathBuf> {
    let v = state
        .store
        .get("system", "history_root")?
        .context("Imported history unavailable")?;
    Ok(PathBuf::from(
        v.as_str().context("Historical directory unavailable")?,
    ))
}
fn db(state: &AppState, name: &str) -> Result<Connection> {
    read_db(&root(state)?.join("_platform").join(name))
}
fn rows(state: &AppState, table: &str) -> Result<Vec<Value>> {
    Ok(table_rows(&db(state, "competition.db")?, table)?
        .iter()
        .map(payload)
        .collect())
}
fn scoped(state: &AppState, table: &str, key: &str, id: &str) -> Result<Vec<Value>> {
    Ok(rows(state, table)?
        .into_iter()
        .filter(|v| v[key] == id)
        .collect())
}
fn one(state: &AppState, table: &str, key: &str, id: &str) -> Result<Value> {
    scoped(state, table, key, id)?
        .into_iter()
        .next()
        .context("Historical record unavailable")
}
fn redact(v: &mut Value) {
    match v {
        Value::Object(o) => {
            for key in [
                "password",
                "secret",
                "api_key",
                "access_token",
                "refresh_token",
                "private_key",
            ] {
                o.remove(key);
            }
            for value in o.values_mut() {
                redact(value);
            }
        }
        Value::Array(a) => {
            for value in a {
                redact(value)
            }
        }
        _ => {}
    }
}
fn connection_view(mut value: Value) -> Value {
    value["original_has_credential"] = json!(
        value["credential_ref"]
            .as_str()
            .is_some_and(|s| !s.is_empty())
    );
    value["has_credential"] = json!(false);
    value["credential_availability"] = json!("not_imported");
    value["historical"] = json!(true);
    value["read_only"] = json!(true);
    redact(&mut value);
    value
}
fn candidate_view(mut value: Value) -> Value {
    if let Some(o) = value.as_object_mut() {
        o.remove("value");
    }
    value
}
fn stored_run(state: &AppState, id: &str) -> Result<Value> {
    state
        .store
        .get("runs", id)?
        .context("Historical run unavailable")
}
async fn run_detail(State(s): State<Arc<AppState>>, Path(id): Path<String>) -> Response {
    if crate::runs::is_local_run(&s, &id) {
        if let Some(detail) = crate::runs::local_run_detail(&s, &id) {
            return Json(detail).into_response();
        }
    }
    response(stored_run(&s, &id))
}
fn open_history_event_file(state: &AppState, run: &Value) -> Result<fs::File> {
    let rel = run["history_event_file"]
        .as_str()
        .context("Historical event stream unavailable")?;
    let base = root(state)?;
    let path = base.join(rel).canonicalize()?;
    if !path.starts_with(base.canonicalize()?) {
        bail!("Historical event stream unavailable");
    }
    Ok(fs::File::open(path)?)
}
pub(crate) fn events(state: &AppState, id: &str) -> Result<Vec<Value>> {
    let run = stored_run(state, id)?;
    let file = open_history_event_file(state, &run)?;
    let mut result = Vec::new();
    let mut seq = 0;
    for line in BufReader::new(file).lines() {
        let line = line?;
        if let Ok(mut event) = serde_json::from_str::<Value>(&line) {
            seq = std::cmp::max(seq + 1, event["seq"].as_u64().unwrap_or(0));
            event["seq"] = json!(seq);
            result.push(event);
        }
    }
    Ok(result)
}
async fn run_history(
    State(s): State<Arc<AppState>>,
    Path(id): Path<String>,
    Query(q): Query<Params>,
) -> Response {
    let before = q
        .get("before")
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(u64::MAX);
    let limit = q
        .get("limit")
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(100)
        .clamp(1, 500);
    if crate::runs::is_local_run(&s, &id) {
        return match tokio::task::spawn_blocking(move || {
            crate::runs::local_run_history(&s, &id, before, limit)
        })
        .await
        {
            Ok(body) => Json(body).into_response(),
            Err(error) => response(Err(error.into())),
        };
    }
    let res = tokio::task::spawn_blocking(move || -> Result<Value> {
        let run = stored_run(&s, &id)?;
        let file = open_history_event_file(&s, &run)?;
        let mut page = std::collections::VecDeque::with_capacity(limit + 1);
        let mut seq = 0u64;
        for line in BufReader::new(file).lines() {
            let line = line?;
            if let Ok(mut event) = serde_json::from_str::<Value>(&line) {
                seq = std::cmp::max(seq + 1, event["seq"].as_u64().unwrap_or(0));
                if seq < before {
                    event["seq"] = json!(seq);
                    if page.len() == limit + 1 {
                        page.pop_front();
                    }
                    page.push_back(event);
                }
            }
        }
        let more = page.len() > limit;
        let page: Vec<_> = page.into_iter().rev().take(limit).collect();
        let next = if more {
            page.last().map(|v| v["seq"].clone())
        } else {
            None
        };
        Ok(json!({"events":page,"next_before":next,"historical":true}))
    })
    .await;
    match res {
        Ok(inner) => response(inner),
        Err(error) => response(Err(error.into())),
    }
}
fn sse(name: &str, id: Option<u64>, value: &Value) -> String {
    format!(
        "{}event: {}\ndata: {}\n\n",
        id.map(|n| format!("id: {n}\n")).unwrap_or_default(),
        name,
        value
    )
}
async fn run_events(
    State(s): State<Arc<AppState>>,
    Path(id): Path<String>,
    Query(q): Query<Params>,
) -> Response {
    let after = q
        .get("after")
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(0);
    let batch_mode = q.get("replay").is_some_and(|v| v == "batch");
    if crate::runs::is_local_run(&s, &id) {
        return crate::runs::stream_local_run_events(s, id, after, batch_mode);
    }
    let opened = (|| -> Result<(Value, fs::File)> {
        let run = stored_run(&s, &id)?;
        let file = open_history_event_file(&s, &run)?;
        Ok((run, file))
    })();
    let (run, file) = match opened {
        Ok(pair) => pair,
        Err(e) => return response(Err(e)),
    };
    let (sender, receiver) = tokio::sync::mpsc::channel::<String>(1);
    tokio::task::spawn_blocking(move || {
        let mut seq = 0u64;
        let mut batch = Vec::new();
        let mut reader = BufReader::new(file);
        let mut line = String::new();
        loop {
            if sender.is_closed() {
                return;
            }
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) => break,
                Err(error) => {
                    tracing::error!(run_id = %id, %error, "Historical event replay failed");
                    let _ = sender.blocking_send(sse(
                        "error", None,
                        &json!({"code":"history.read_failed","message":error.to_string(),"last_seq":seq}),
                    ));
                    return;
                }
                Ok(_) => {
                    if let Ok(mut event) = serde_json::from_str::<Value>(&line) {
                        seq = std::cmp::max(seq + 1, event["seq"].as_u64().unwrap_or(0));
                        if seq <= after {
                            continue;
                        }
                        event["seq"] = json!(seq);
                        if batch_mode {
                            batch.push(event);
                            if batch.len() >= 500 {
                                let last_seq =
                                    batch.last().and_then(|v| v["seq"].as_u64()).unwrap_or(seq);
                                if sender
                                    .blocking_send(sse(
                                        "replay.batch",
                                        Some(last_seq),
                                        &json!({"events":batch,"last_seq":last_seq}),
                                    ))
                                    .is_err()
                                {
                                    return;
                                }
                                batch.clear();
                            }
                        } else if sender
                            .blocking_send(sse(
                                event["event_type"].as_str().unwrap_or("history.event"),
                                Some(seq),
                                &event,
                            ))
                            .is_err()
                        {
                            return;
                        }
                    }
                }
            }
        }
        if run["status"] == "interrupted" {
            seq += 1;
            if seq > after {
                let synth = json!({"event_type":"run.finished","run_id":id,"seq":seq,"ts":run["ts"],"payload":{"solved":run["solved"],"flag":run["flag"],"flags":run["flags"],"expected_flags":run["expected_flags"],"multi_flag":run["multi_flag"],"reason":"interrupted","historical":true,"original_status":run["original_status"]}});
                if batch_mode {
                    batch.push(synth);
                } else if sender
                    .blocking_send(sse("run.finished", Some(seq), &synth))
                    .is_err()
                {
                    return;
                }
            }
        }
        if !batch.is_empty() {
            let last_seq = batch.last().and_then(|v| v["seq"].as_u64()).unwrap_or(seq);
            if sender
                .blocking_send(sse(
                    "replay.batch",
                    Some(last_seq),
                    &json!({"events":batch,"last_seq":last_seq}),
                ))
                .is_err()
            {
                return;
            }
        }
        let last = if seq > 0 { seq } else { after };
        let _ = sender.blocking_send(sse(
            "replay.complete",
            None,
            &json!({"last_seq":last,"historical":true}),
        ));
    });
    let initial = futures_util::stream::unfold(receiver, |mut receiver| async move {
        receiver
            .recv()
            .await
            .map(|frame| (Ok::<_, std::convert::Infallible>(frame), receiver))
    });
    let keepalive = tokio_stream::wrappers::IntervalStream::new(tokio::time::interval(
        std::time::Duration::from_secs(15),
    ))
    .map(|_| Ok::<_, std::convert::Infallible>(": imported history\n\n".to_owned()));
    (
        [
            (header::CONTENT_TYPE, "text/event-stream"),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        axum::body::Body::from_stream(initial.chain(keepalive)),
    )
        .into_response()
}
fn event_response(body: String) -> Response {
    let initial =
        futures_util::stream::once(async move { Ok::<_, std::convert::Infallible>(body) });
    let keepalive = tokio_stream::wrappers::IntervalStream::new(tokio::time::interval(
        std::time::Duration::from_secs(15),
    ))
    .map(|_| Ok::<_, std::convert::Infallible>(": imported history\n\n".to_owned()));
    (
        [
            (header::CONTENT_TYPE, "text/event-stream"),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        axum::body::Body::from_stream(initial.chain(keepalive)),
    )
        .into_response()
}

fn graph_path(state: &AppState, id: &str) -> Result<PathBuf> {
    stored_run(state, id)?;
    if !crate::import::valid_id(id) {
        bail!("Historical graph unavailable");
    }
    let path = root(state)?.join("graphs").join(id).join("shared_graph.db");
    if !path.is_file() {
        bail!("Historical graph unavailable");
    }
    Ok(path)
}
fn graph_events(state: &AppState, id: &str) -> Result<Vec<Value>> {
    let conn = read_db(&graph_path(state, id)?)?;
    table_rows(&conn, "events")
}
async fn graph_export(
    State(s): State<Arc<AppState>>,
    Path(id): Path<String>,
    Query(q): Query<Params>,
) -> Response {
    let format = q.get("format").map(String::as_str).unwrap_or("jsonl");
    if !matches!(format, "jsonl" | "timeline" | "sqlite") {
        return (StatusCode::BAD_REQUEST,
            Json(json!({"detail":"format must be jsonl, timeline, or sqlite"}))).into_response();
    }
    if crate::runs::is_local_run(&s, &id) {
        return match crate::runs::local_graph_export(&s, &id, format) {
            Ok(r) => r,
            Err(e) => graph_export_error(e),
        };
    }
    let result = (|| -> Result<Response> {
        let run = stored_run(&s, &id)?;
        let conn = read_db(&graph_path(&s, &id)?)
            .map_err(|error| anyhow::anyhow!("run graph unreadable: {error}"))?;
        let body = if format == "sqlite" {
            crate::runs::graph::export_sqlite_snapshot(&conn, true)
                .map_err(anyhow::Error::msg)?
        } else {
            crate::runs::graph::export_graph_text(&conn, &id, &run, format, true)
                .map_err(anyhow::Error::msg)?.into_bytes()
        };
        Ok(crate::runs::graph_export_response(&id, format, body))
    })();
    match result {
        Ok(r) => r,
        Err(e) => graph_export_error(e.to_string()),
    }
}
fn graph_export_error(detail: String) -> Response {
    let status = if detail == "format must be jsonl, timeline, or sqlite" {
        StatusCode::BAD_REQUEST
    } else if detail.contains("unavailable") || detail == "unknown run" {
        StatusCode::NOT_FOUND
    } else {
        StatusCode::CONFLICT
    };
    (status, Json(json!({"detail":detail}))).into_response()
}
fn challenge_views(state: &AppState, id: &str) -> Result<Vec<Value>> {
    let revisions = rows(state, "challenge_revisions")?;
    let bindings = scoped(state, "run_bindings", "competition_id", id)?;
    let leases = scoped(state, "instance_leases", "competition_id", id)?;
    let queue = scoped(state, "scheduler_queue", "competition_id", id)?;
    let candidates = scoped(state, "submission_candidates", "competition_id", id)?;
    let submissions = scoped(state, "platform_submissions", "competition_id", id)?;
    let mut result = Vec::new();
    for challenge in scoped(state, "competition_challenges", "competition_id", id)? {
        let cid = &challenge["challenge_id"];
        let revision = revisions
            .iter()
            .find(|v| v["revision_id"] == challenge["current_revision_id"]);
        let binding = bindings
            .iter()
            .rev()
            .find(|v| v["competition_challenge_id"] == *cid);
        let lease = leases
            .iter()
            .rev()
            .find(|v| v["competition_challenge_id"] == *cid)
            .cloned()
            .map(connection_view);
        let entry = queue.iter().find(|v| v["competition_challenge_id"] == *cid);
        let candidates: Vec<_> = candidates
            .iter()
            .filter(|v| v["competition_challenge_id"] == *cid)
            .cloned()
            .map(candidate_view)
            .collect();
        let submissions: Vec<_> = submissions
            .iter()
            .filter(|v| v["competition_challenge_id"] == *cid)
            .collect();
        result.push(json!({"challenge":challenge,"current_revision":revision,"active_binding":binding,"run_id":binding.map(|v|v["run_id"].clone()),"active_lease":lease,"queue_entry":entry,"candidates":candidates,"submissions":submissions,"historical":true}));
    }
    Ok(result)
}
fn competition_snapshot(state: &AppState, id: &str) -> Result<Value> {
    if let Some(snapshot) = competition_api::live_snapshot(state, id).map_err(anyhow::Error::msg)? {
        return Ok(snapshot);
    }
    let competition = state
        .store
        .get("competitions", id)?
        .context("Historical competition unavailable")?;
    let connection = competition["connection_id"]
        .as_str()
        .and_then(|v| one(state, "platform_connections", "connection_id", v).ok())
        .map(connection_view);
    let watermark: i64 = db(state, "competition.db")?.query_row(
        "SELECT COALESCE(MAX(seq),0) FROM competition_events",
        [],
        |r| r.get(0),
    )?;
    Ok(
        json!({"competition":competition,"connection":connection,"policy":one(state,"competition_policies","competition_id",id).ok(),"challenges":challenge_views(state,id)?,"queue":scoped(state,"scheduler_queue","competition_id",id)?,"budgets":scoped(state,"resource_budgets","competition_id",id)?,"event_watermark":watermark,"projection_watermarks":{},"pending_receipts":0,"pending_outbox":0,"historical":true,"read_only":true}),
    )
}
async fn competition_detail(State(s): State<Arc<AppState>>, Path(id): Path<String>) -> Response {
    response(competition_snapshot(&s, &id))
}
async fn competition_challenges(
    State(s): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Response {
    match competition_api::live_challenges(&s, &id) {
        Ok(Some(value)) => return Json(value).into_response(),
        Err(error) => return response(Err(anyhow::Error::msg(error))),
        Ok(None) => {}
    }
    response(challenge_views(&s, &id).map(|v| json!(v)))
}
async fn competition_submissions(
    State(s): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Response {
    match competition_api::live_submissions(&s, &id) {
        Ok(Some(value)) => return Json(value).into_response(),
        Err(error) => return response(Err(anyhow::Error::msg(error))),
        Ok(None) => {}
    }
    response((|| {
        Ok(
            json!({"competition_id":id,"submissions":scoped(&s,"platform_submissions","competition_id",&id)?,"candidates":scoped(&s,"submission_candidates","competition_id",&id)?.into_iter().map(candidate_view).collect::<Vec<_>>()}),
        )
    })())
}
async fn competition_leases(State(s): State<Arc<AppState>>, Path(id): Path<String>) -> Response {
    match competition_api::live_leases(&s, &id) {
        Ok(Some(value)) => return Json(value).into_response(),
        Err(error) => return response(Err(anyhow::Error::msg(error))),
        Ok(None) => {}
    }
    response(
        scoped(&s, "instance_leases", "competition_id", &id)
            .map(|v| json!(v.into_iter().map(connection_view).collect::<Vec<_>>())),
    )
}
async fn connections(State(s): State<Arc<AppState>>) -> Response {
    response((|| {
        let mut records = competition_api::live_connections(&s).map_err(anyhow::Error::msg)?;
        if root(&s).is_ok() {
            records.extend(
                rows(&s, "platform_connections")?
                    .into_iter()
                    .map(connection_view),
            );
        }
        Ok(json!(records))
    })())
}
async fn platform_kinds(State(s): State<Arc<AppState>>) -> Response {
    response((|| {
        let mut kinds = Vec::new();
        for (id, label, icon) in [
            ("ctfd", "CTFd", "flag"),
            ("rctf", "rCTF", "flag"),
            ("gzctf", "GZCTF", "flag"),
            ("generic_browser", "浏览器会话", "globe"),
        ] {
            let state = if matches!(id, "generic_browser" | "gzctf") {
                "limited"
            } else {
                "ready"
            };
            kinds.push(json!({"id":id,"label":label,"icon":icon,"origin":"builtin","source":"builtin","state":state,"available":true}));
        }
        for entry in crate::extensions::ready_capabilities(&s,"platform-adapter")
            .map_err(anyhow::Error::msg)? {
            let id = entry["id"].as_str().unwrap_or("");
            if !id.is_empty() && !kinds.iter().any(|kind| kind["id"] == id) {
                kinds.push(json!({"id":id,"label":entry["label"].as_str().unwrap_or(id),"icon":"plug",
                    "origin":"extension","source":"extension","extension_id":entry["extension_id"],
                    "extension_version":entry["extension_version"],"api_version":entry["api_version"],
                    "state":"ready","available":true}));
            }
        }
        let extensions = if root(&s).is_ok() {
            table_rows(&db(&s, "platform.db")?, "extensions")?
        } else {
            Vec::new()
        };
        for row in extensions {
            let manifest = payload(&row);
            if let Some(provides) = manifest["provides"].as_array() {
                for provide in provides {
                    if provide["type"] != "platform-adapter" {
                        continue;
                    }
                    if let Some(id) = provide["id"].as_str() {
                        if !kinds.iter().any(|v| v["id"] == id) {
                            kinds.push(json!({"id":id,"label":id,"icon":"plug","origin":"extension","source":"extension","extension_id":manifest["id"],"extension_version":manifest["version"],"api_version":provide["api_version"],"state":"unavailable","original_enabled":row["enabled"],"available":false,"historical":true,"read_only":true}));
                        }
                    }
                }
            }
        }
        let history_connections = if root(&s).is_ok() {
            rows(&s, "platform_connections")?
        } else {
            Vec::new()
        };
        for connection in history_connections {
            if let Some(id) = connection["platform_kind"]
                .as_str()
                .filter(|v| !v.is_empty())
            {
                if !kinds.iter().any(|v| v["id"] == id) {
                    kinds.push(json!({"id":id,"label":id,"icon":"flag","origin":"history","source":"connection","state":"unavailable","available":false,"historical":true,"read_only":true}));
                }
            }
        }
        Ok(json!({"kinds":kinds}))
    })())
}
fn secret_metadata(state: &AppState) -> Result<Vec<Value>> {
    let metadata = state
        .store
        .get("system", "history_platform_secret_metadata")?
        .unwrap_or_else(|| json!([]));
    let mut records = competition_api::live_platform_secrets(state).map_err(anyhow::Error::msg)?;
    records.extend(metadata.as_array().cloned().unwrap_or_default());
    let historical_connections = if root(state).is_ok() {
        rows(state, "platform_connections")?
    } else {
        Vec::new()
    };
    for connection in historical_connections {
        let Some(reference) = connection["credential_ref"]
            .as_str()
            .filter(|v| !v.is_empty())
        else {
            continue;
        };
        if records.iter().any(|v| v["reference"] == reference) {
            continue;
        }
        let Some(path) = reference.strip_prefix("secret://platform/") else {
            continue;
        };
        let Some((connection_id, key)) = path.split_once('/') else {
            continue;
        };
        if connection["connection_id"] != connection_id || key.is_empty() || key.contains('/') {
            continue;
        }
        records.push(json!({"reference":reference,"connection_id":connection_id,"key":key,"created_at":"","updated_at":"","expires_at":"","rotation":1,"status":"not_imported","original_status":null,"source_present":null,"present":false,"available":false,"historical":true,"read_only":true}));
    }
    records.sort_by(|a, b| a["reference"].as_str().cmp(&b["reference"].as_str()));
    Ok(records)
}
async fn platform_secrets(State(s): State<Arc<AppState>>, Query(q): Query<Params>) -> Response {
    response(secret_metadata(&s).map(|rows| {
        json!(
            rows.into_iter()
                .filter(|v| q
                    .get("connection_id")
                    .filter(|v| !v.is_empty())
                    .is_none_or(|id| v["connection_id"] == id.as_str()))
                .collect::<Vec<_>>()
        )
    }))
}
async fn platform_secret_inspect(
    State(s): State<Arc<AppState>>,
    Query(q): Query<Params>,
) -> Response {
    response((|| {
        let reference = q
            .get("reference")
            .context("Historical credential metadata unavailable")?;
        secret_metadata(&s)?
            .into_iter()
            .find(|v| v["reference"] == reference.as_str())
            .context("Historical credential metadata unavailable")
    })())
}
async fn competition_history(
    State(s): State<Arc<AppState>>,
    Path(id): Path<String>,
    Query(q): Query<Params>,
) -> Response {
    let before = q
        .get("before")
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(u64::MAX);
    let limit = q
        .get("limit")
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(100)
        .clamp(1, 200);
    match competition_api::live_history(&s, &id, before, limit) {
        Ok(Some(value)) => return Json(value).into_response(),
        Err(error) => return response(Err(anyhow::Error::msg(error))),
        Ok(None) => {}
    }
    response((|| {
        let conn = db(&s, "competition.db")?;
        let before = q
            .get("before")
            .and_then(|v| v.parse::<i64>().ok())
            .unwrap_or(i64::MAX);
        let limit = q
            .get("limit")
            .and_then(|v| v.parse::<i64>().ok())
            .unwrap_or(100)
            .clamp(1, 200);
        let mut stmt=conn.prepare("SELECT seq,payload FROM competition_events WHERE competition_id=?1 AND seq<?2 ORDER BY seq DESC LIMIT ?3")?;
        let mut rows = stmt.query((id, before, limit + 1))?;
        let mut events = Vec::new();
        while let Some(row) = rows.next()? {
            let seq: i64 = row.get(0)?;
            let mut value: Value = serde_json::from_str(&row.get::<_, String>(1)?)?;
            value["seq"] = json!(seq);
            redact(&mut value);
            events.push(value);
        }
        let more = events.len() > limit as usize;
        events.truncate(limit as usize);
        Ok(
            json!({"next_before":if more{events.last().map(|e|e["seq"].clone())}else{None},"events":events,"historical":true}),
        )
    })())
}
async fn competition_events(State(s): State<Arc<AppState>>, Path(id): Path<String>,
    Query(query): Query<Params>, headers: HeaderMap) -> Response {
    if let Some(stream) = competition_api::live_events_response_with_cursor(s.clone(), id.clone(),
        query.get("after").map(String::as_str), headers.get("last-event-id").and_then(|value|value.to_str().ok())).await {
        return stream;
    }
    match competition_snapshot(&s, &id) {
        Ok(v) => event_response(sse(
            "snapshot",
            v["event_watermark"].as_u64(),
            &json!({"watermark_seq":v["event_watermark"],"snapshot":v}),
        )),
        Err(e) => response(Err(e)),
    }
}
fn asset_view(record: &Value) -> Value {
    let mut asset = payload(record);
    let display = [
        &asset["display_name"],
        &asset["current_profile"]["title"],
        &asset["current_profile"]["display_name"],
        &asset["current_profile"]["name"],
        &asset["identity"]["url"],
        &asset["identity"]["name"],
        &asset["identity"]["address"],
        &asset["identity"]["host"],
        &asset["asset_id"],
    ]
    .into_iter()
    .filter_map(Value::as_str)
    .find(|v| !v.trim().is_empty())
    .unwrap_or("")
    .to_owned();
    asset["display_name"] = json!(display);
    if asset["canonical_key"]
        .as_str()
        .is_none_or(|v| v.is_empty() || v.starts_with("asset-key."))
    {
        let canonical = [
            &asset["current_profile"]["canonical_key"],
            &asset["identity"]["url"],
            &asset["identity"]["name"],
            &asset["identity"]["address"],
            &asset["identity"]["host"],
            &asset["display_name"],
        ]
        .into_iter()
        .filter_map(Value::as_str)
        .find(|v| !v.trim().is_empty())
        .unwrap_or("")
        .to_owned();
        asset["canonical_key"] = json!(canonical);
    }
    asset
}
fn overlay_records(state: &AppState, id: &str, rows: &mut Vec<Value>) -> Result<()> {
    for saved in state.store.list("history_workbench_overlays")? {
        let record = &saved["values"]["record"];
        if record["task_id"] != id {
            continue;
        }
        if let Some(existing) = rows
            .iter_mut()
            .find(|v| v["record_id"] == record["record_id"])
        {
            *existing = record.clone();
        } else {
            rows.push(record.clone());
        }
    }
    Ok(())
}
fn task_records(state: &AppState, id: &str, kind: &str) -> Result<Vec<Value>> {
    let mut rows: Vec<_> = state
        .store
        .list("history_src_records")?
        .into_iter()
        .filter(|v| v["task_id"] == id && (kind.is_empty() || v["kind"] == kind))
        .collect();
    if kind.is_empty() || kind == "workbench_slice" {
        overlay_records(state, id, &mut rows)?;
    }
    rows.sort_by_key(|v| v["_legacy_order"].as_u64().unwrap_or(0));
    Ok(rows)
}
fn task_view(state: &AppState, id: &str) -> Result<Value> {
    let mut task = state
        .store
        .get("history_src_tasks", id)?
        .context("Historical SRC task unavailable")?;
    let records = task_records(state, id, "")?;
    let binding = records
        .iter()
        .find(|v| v["kind"] == "task_binding")
        .map(payload)
        .unwrap_or(Value::Null);
    task["binding"] = binding;
    task["run_count"] = json!(
        records
            .iter()
            .filter(|v| v["kind"] == "run_binding")
            .count()
    );
    task["lifecycle"] = records
        .iter()
        .find(|v| v["kind"] == "task_lifecycle")
        .map(payload)
        .unwrap_or_else(|| json!({"state":"active"}));
    task["chat_configuration"] = records
        .iter()
        .rev()
        .find(|v| v["kind"] == "model_config" && v["payload"]["role"] == "chat")
        .cloned()
        .unwrap_or(Value::Null);
    task["worker_configurations"] = json!(
        records
            .iter()
            .filter(|v| v["kind"] == "model_config" && v["payload"]["role"] == "worker")
            .collect::<Vec<_>>()
    );
    task["watermarks"] = json!([]);
    task["historical"] = json!(true);
    task["read_only"] = json!(true);
    Ok(task)
}
async fn src_tasks(State(s): State<Arc<AppState>>) -> Response {
    response((|| {
        let mut tasks = src_api::tasks(&s).map_err(anyhow::Error::msg)?;
        let live_ids: std::collections::HashSet<String> = tasks.iter()
            .filter_map(|task| task["task_id"].as_str().map(str::to_owned)).collect();
        for task in s.store.list("history_src_tasks")? {
            let id = task["task_id"].as_str().context("SRC task ID")?;
            if !live_ids.contains(id) {
                tasks.push(task_view(&s, id)?);
            }
        }
        Ok(json!(tasks))
    })())
}
async fn src_task(State(s): State<Arc<AppState>>, Path(id): Path<String>) -> Response {
    match src_api::has_task(&s, &id) {
        Ok(true) => return src_api::task_view(&s, &id),
        Err(error) => return response(Err(error.into())),
        Ok(false) => {}
    }
    response(task_view(&s, &id))
}
fn record_page(state: &AppState, id: &str, params: &Value) -> Result<Value> {
    task_view(state, id)?;
    let kind = params["kind"].as_str().unwrap_or("");
    let parent = params["parent_id"].as_str();
    let limit = params["limit"]
        .as_u64()
        .or_else(|| params["limit"].as_str().and_then(|s| s.parse().ok()))
        .unwrap_or(100)
        .clamp(1, 1000) as usize;
    let offset = params["cursor"]
        .as_str()
        .filter(|s| !s.is_empty())
        .map(str::parse::<usize>)
        .transpose()?
        .unwrap_or(0);
    let rows: Vec<_> = task_records(state, id, kind)?
        .into_iter()
        .filter(|v| parent.is_none_or(|p| v["parent_id"] == p))
        .collect();
    let total = rows.len();
    let selected: Vec<_> = rows.into_iter().skip(offset).take(limit).collect();
    let returned = selected.len();
    let more = offset + returned < total;
    Ok(
        json!({"records":selected,"total":total,"returned":returned,"next_cursor":if more{Some((offset+returned).to_string())}else{None},"complete":!more,"partial_reason":null,"watermarks":[],"snapshot":{"event_watermark":0},"historical":true}),
    )
}
async fn src_records(
    State(s): State<Arc<AppState>>,
    Path(id): Path<String>,
    Query(q): Query<Params>,
) -> Response {
    match src_api::has_task(&s, &id) {
        Ok(true) => return src_api::record_page(&s, &id, &json!(q)),
        Err(error) => return response(Err(error.into())),
        Ok(false) => {}
    }
    response(record_page(&s, &id, &json!(q)))
}
fn get_record(state: &AppState, id: &str, kind: &str, record: &str) -> Result<Value> {
    if kind == "workbench_slice" {
        if let Some(saved) = state.store.get("history_workbench_overlays", record)? {
            let value = saved["values"]["record"].clone();
            if value["task_id"] != id {
                bail!("Historical SRC record unavailable");
            }
            return Ok(value);
        }
    }
    let value = state
        .store
        .get("history_src_records", record)?
        .context("Historical SRC record unavailable")?;
    if value["task_id"] != id || value["kind"] != kind {
        bail!("Historical SRC record unavailable");
    }
    Ok(value)
}
async fn src_record(
    State(s): State<Arc<AppState>>,
    Path((id, kind, record)): Path<(String, String, String)>,
) -> Response {
    match src_api::has_task(&s, &id) {
        Ok(true) => return src_api::record_view(&s, &id, &kind, &record),
        Err(error) => return response(Err(error.into())),
        Ok(false) => {}
    }
    response(get_record(&s, &id, &kind, &record))
}
async fn src_policy(State(s): State<Arc<AppState>>, Path(id): Path<String>) -> Response {
    match src_api::has_task(&s, &id) {
        Ok(true) => return src_api::policy_view(&s, &id),
        Err(error) => return response(Err(error.into())),
        Ok(false) => {}
    }
    response((|| {
        task_records(&s, &id, "execution_policy")?
            .last()
            .map(payload)
            .context("Historical execution policy unavailable")
    })())
}
async fn src_capabilities(State(s): State<Arc<AppState>>, Path(id): Path<String>) -> Response {
    match src_api::has_task(&s, &id) {
        Ok(true) => return src_api::capabilities_view(&s, &id),
        Err(error) => return response(Err(error.into())),
        Ok(false) => {}
    }
    response(task_view(&s,&id).map(|_|json!({"available":false,"read_only":true,"historical":true,"reason":"Imported history does not activate execution services","tools":{},"environment":{}})))
}
async fn src_execution(
    State(s): State<Arc<AppState>>,
    Path((id, invocation)): Path<(String, String)>,
) -> Response {
    match src_api::has_task(&s, &id) {
        Ok(true) => return src_api::execution_view(&s, &id, &invocation),
        Err(error) => return response(Err(error.into())),
        Ok(false) => {}
    }
    response((|| {
        task_records(&s, &id, "execution_receipt")?
            .into_iter()
            .find(|v| v["record_id"] == invocation || v["payload"]["invocation_id"] == invocation)
            .map(|v| payload(&v))
            .context("Historical execution receipt unavailable")
    })())
}
fn task_runs(state: &AppState, id: &str) -> Result<Value> {
    task_view(state, id)?;
    let mut values = Vec::new();
    for binding in task_records(state, id, "run_binding")? {
        let binding = payload(&binding);
        let rid = binding["run_id"].as_str().unwrap_or("");
        let run = state.store.get("runs", rid)?.unwrap_or(Value::Null);
        values.push(json!({"binding":binding,"run":{"run_id":rid,"state":run["status"],"generation":run["execution_generation"],"detail":run,"historical":true}}));
    }
    Ok(json!(values))
}
async fn src_runs(State(s): State<Arc<AppState>>, Path(id): Path<String>) -> Response {
    match src_api::has_task(&s, &id) {
        Ok(true) => return src_api::runs_view(&s, &id),
        Err(error) => return response(Err(error.into())),
        Ok(false) => {}
    }
    response(task_runs(&s, &id))
}
async fn src_ensure_run(
    State(s): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> Response {
    match src_api::has_task(&s, &id) {
        Ok(true) => src_api::command_view(s, &id, "src.run.ensure", &body).await,
        Ok(false) => crate::runs::unsupported_501("历史导入 SRC Task 不能启动新 Run"),
        Err(error) => response(Err(error.into())),
    }
}
async fn src_start_run(
    State(s): State<Arc<AppState>>,
    Path((id, run)): Path<(String, String)>,
    Json(mut body): Json<Value>,
) -> Response {
    body["run_id"] = json!(run);
    match src_api::has_task(&s, &id) {
        Ok(true) => src_api::command_view(s, &id, "src.run.start", &body).await,
        Ok(false) => crate::runs::unsupported_501("历史导入 SRC Task 不能启动新 Run"),
        Err(error) => response(Err(error.into())),
    }
}
async fn src_policy_preview(
    State(s): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> Response {
    match src_api::has_task(&s, &id) {
        Ok(true) => src_api::preview_policy(&s, &id, &body),
        Ok(false) => crate::runs::unsupported_501("历史导入 SRC Task 不提供实时策略预览"),
        Err(error) => response(Err(error.into())),
    }
}
async fn src_query(
    State(s): State<Arc<AppState>>,
    Path((id, kind)): Path<(String, String)>,
    Json(params): Json<Value>,
) -> Response {
    match src_api::has_task(&s, &id) {
        Ok(true) => return src_api::query_view(&s, &id, &kind, &params),
        Err(error) => return response(Err(error.into())),
        Ok(false) => {}
    }
    response(query_view(&s, &id, &kind, &params))
}
fn workbench_error(status: StatusCode, code: &str, message: &str, detail: Value) -> Response {
    (
        status,
        Json(json!({"state":"conflict","error":{"code":code,"message":message,"detail":detail}})),
    )
        .into_response()
}
async fn src_takeover(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    body: Option<Json<Value>>,
) -> Response {
    let payload = body.map(|Json(v)| v).unwrap_or_else(|| json!({}));
    src_api::takeover_historical_task(state, &id, &payload).await
}
async fn src_debug_logs(State(s): State<Arc<AppState>>, Path(id): Path<String>) -> Response {
    src_api::debug_logs::export_handler(s, id).await
}
async fn src_command(
    State(state): State<Arc<AppState>>,
    Path((id, cmd)): Path<(String, String)>,
    Json(body): Json<Value>,
) -> Response {
    match src_api::has_task(&state, &id) {
        Ok(true) => return src_api::command_view(state, &id, &cmd, &body).await,
        Err(error) => return response(Err(error.into())),
        Ok(false) => {}
    }
    match state.store.get("history_src_tasks", &id) {
        Ok(Some(_)) => {
            return workbench_error(
                StatusCode::CONFLICT,
                "historical_read_only",
                "历史归档 Task 为只读快照；如需继续或编辑，请先点击接管创建活跃 Task",
                json!({}),
            );
        }
        Ok(None) => {}
        Err(error) => return response(Err(error.into())),
    }
    if cmd != "src.workbench.slice.save" {
        return crate::runs::unsupported_501(&format!(
            "SRC 命令 `{cmd}` 将在阶段 7.6 接入；当前仅支持工作台切片保存 (`src.workbench.slice.save`)。"
        ));
    }
    response(Err(anyhow::anyhow!("Historical SRC task unavailable")))
}
fn query_view(state: &AppState, id: &str, kind: &str, params: &Value) -> Result<Value> {
    task_view(state, id)?;
    let records = task_records(state, id, "")?;
    let of_kind = |kind: &str| -> Vec<Value> {
        records
            .iter()
            .filter(|v| v["kind"] == kind)
            .cloned()
            .collect()
    };
    let payloads = |kind: &str| -> Vec<Value> { of_kind(kind).iter().map(payload).collect() };
    match kind {
        "src.task.get" => task_view(state, id),
        "src.run.list" => task_runs(state, id),
        "src.record.list" => record_page(state, id, params),
        "src.record.get" => get_record(
            state,
            id,
            params["kind"].as_str().unwrap_or(""),
            params["record_id"].as_str().unwrap_or(""),
        ),
        "src.policy.current" => payloads("execution_policy")
            .last()
            .cloned()
            .context("Historical policy unavailable"),
        "src.workbench.snapshot" => {
            let mut slices = serde_json::Map::new();
            for row in of_kind("workbench_slice") {
                if let Some(key) = row["natural_key"].as_str() {
                    slices.insert(key.into(),json!({"revision":row["revision"],"updated_at":row["updated_at"],"client_id":row["payload"]["client_id"],"data":row["payload"]["data"]}));
                }
            }
            Ok(json!({"slices":slices}))
        }
        "src.workbench.slice.get" => {
            let row = of_kind("workbench_slice")
                .into_iter()
                .find(|v| v["natural_key"] == params["slice"])
                .context("Historical workbench slice unavailable")?;
            Ok(
                json!({"slice":row["natural_key"],"revision":row["revision"],"updated_at":row["updated_at"],"client_id":row["payload"]["client_id"],"data":row["payload"]["data"]}),
            )
        }
        "src.asset.query" => {
            let expression = params["expression"].as_str().unwrap_or("");
            if !expression.is_empty() {
                bail!("unsupported historical asset query expression");
            }
            let mut items = Vec::new();
            for row in of_kind("asset") {
                let mut asset = asset_view(&row);
                if ["destroyed", "merged"]
                    .iter()
                    .any(|v| asset["lifecycle"] == *v)
                {
                    continue;
                }
                asset["task_id"] = json!(id);
                asset["updated_at"] = row["updated_at"].clone();
                asset["access"] = json!("task");
                asset["observations"] = json!(
                    payloads("asset_observation")
                        .into_iter()
                        .filter(|v| v["asset_id"] == asset["asset_id"])
                        .collect::<Vec<_>>()
                );
                items.push(asset);
            }
            let limit = params["limit"].as_u64().unwrap_or(50).clamp(1, 1000) as usize;
            let offset = params["cursor"]
                .as_str()
                .filter(|v| !v.is_empty())
                .map(str::parse::<usize>)
                .transpose()?
                .unwrap_or(0);
            let total = items.len();
            let items: Vec<_> = items.into_iter().skip(offset).take(limit).collect();
            let returned = items.len();
            Ok(
                json!({"items":items,"total":total,"returned":returned,"next_cursor":if offset+returned<total{Some((offset+returned).to_string())}else{None},"expression":expression,"watermarks":[],"semantics":{},"partial_reason":null,"historical":true}),
            )
        }
        "src.asset.detail" => {
            let aid = params["asset_id"].as_str().unwrap_or("");
            let entity = get_record(state, id, "asset", aid)?;
            let obs: Vec<_> = payloads("asset_observation")
                .into_iter()
                .filter(|v| v["asset_id"] == aid)
                .collect();
            let relations: Vec<_> = payloads("asset_relation")
                .into_iter()
                .filter(|v| v["from_asset_id"] == aid || v["to_asset_id"] == aid)
                .collect();
            Ok(
                json!({"asset":asset_view(&entity),"entity":asset_view(&entity),"observations":obs,"history":obs,"sources":obs.iter().map(|v|json!({"source_kind":v["source_kind"],"source_id":v["source_id"]})).collect::<Vec<_>>(),"relations":relations,"access":"task","historical":true}),
            )
        }
        "src.asset.pivot" => {
            let aid = params["asset_id"].as_str().unwrap_or("");
            let items: Vec<_> = payloads("asset_relation")
                .into_iter()
                .filter(|v| v["from_asset_id"] == aid || v["to_asset_id"] == aid)
                .collect();
            Ok(json!({"total":items.len(),"items":items,"next_cursor":null,"historical":true}))
        }
        "src.report.get" => {
            let rid = params["report_id"].as_str().unwrap_or("");
            Ok(payload(&get_record(state, id, "frozen_report", rid)?))
        }
        "src.report.list" => {
            let finding = params["finding_id"].as_str();
            Ok(json!(
                payloads("frozen_report")
                    .into_iter()
                    .filter(|v| finding.is_none_or(|f| v["finding_id"] == f))
                    .collect::<Vec<_>>()
            ))
        }
        "src.finding.claim.get" => {
            let rid = params["finding_id"]
                .as_str()
                .or_else(|| params["claim_id"].as_str())
                .unwrap_or("");
            Ok(payload(&get_record(state, id, "finding_claim", rid)?))
        }
        "src.finding.claims" | "src.finding.reviews" | "src.finding.deduplications" => {
            let record_kind = match kind {
                "src.finding.claims" => "finding_claim",
                "src.finding.reviews" => "finding_review",
                _ => "finding_dedup",
            };
            let finding = params["finding_id"].as_str();
            Ok(json!(
                payloads(record_kind)
                    .into_iter()
                    .filter(|v| finding.is_none_or(|f| v["finding_id"] == f))
                    .collect::<Vec<_>>()
            ))
        }
        "src.finding.status" => {
            let fid = params["finding_id"].as_str().unwrap_or("");
            let review = payloads("finding_review")
                .into_iter()
                .rev()
                .find(|v| v["finding_id"] == fid);
            let stale: Vec<_> = payloads("finding_stale")
                .into_iter()
                .filter(|v| v["finding_id"] == fid && v["cleared"] != true)
                .collect();
            let is_stale = review.is_some() && !stale.is_empty();
            Ok(
                json!({"finding_id":fid,"state":if is_stale {json!("stale")}else{review.as_ref().map(|v|v["readiness"].clone()).unwrap_or(json!("unreviewed"))},"review":review,"stale":is_stale,"stale_reasons":stale.iter().map(|v|v["reason"].clone()).collect::<Vec<_>>(),"stale_refs":stale}),
            )
        }
        "src.monitor.list" => Ok(json!(payloads("monitor"))),
        "src.feedback.platform_status" => {
            let report = params["report_id"].as_str().unwrap_or("");
            get_record(state, id, "frozen_report", report)?;
            Ok(payloads("platform_status").into_iter().filter(|v|v["report_id"]==report).max_by_key(|v|v["revision"].as_u64().unwrap_or(0)).unwrap_or_else(||json!({"task_id":id,"report_id":report,"revision":0,"status":"not_submitted","verification":"用户记录，未与平台核验"})))
        }
        "src.feedback.history" => Ok(json!(
            payloads("platform_status")
                .into_iter()
                .filter(|v| v["report_id"] == params["report_id"])
                .collect::<Vec<_>>()
        )),
        "src.feedback.diff" => {
            let old = payload(&get_record(
                state,
                id,
                "frozen_report",
                params["old_report_id"].as_str().unwrap_or(""),
            )?);
            let new = payload(&get_record(
                state,
                id,
                "frozen_report",
                params["new_report_id"].as_str().unwrap_or(""),
            )?);
            if old["finding_id"] != new["finding_id"] {
                bail!("Historical report comparison unavailable");
            }
            let mut changes = Vec::new();
            for (field, path) in [
                ("claims", "/claims"),
                ("impact", "/document/confirmed_impact"),
                ("conditional_impact", "/document/conditional_impact"),
                ("steps", "/document/steps"),
                ("cvss", "/cvss"),
                ("evidence", "/evidence_refs"),
                ("readiness", "/review/readiness"),
                ("template", "/template"),
                ("document", "/document"),
            ] {
                if old.pointer(path) != new.pointer(path) {
                    changes.push(json!({"field":field,"path":path,"before":old.pointer(path),"after":new.pointer(path)}));
                }
            }
            let feedback: Vec<_> = payloads("platform_feedback")
                .into_iter()
                .filter(|v| {
                    v["report_id"] == old["report_id"] || v["report_id"] == new["report_id"]
                })
                .collect();
            Ok(
                json!({"old_report_id":old["report_id"],"new_report_id":new["report_id"],"changes":changes,"feedback":feedback}),
            )
        }
        "src.retest.recover" => Ok(json!(
            payloads("finding_retest")
                .into_iter()
                .filter(|v| v["result"].is_null() && v["status"] == "dispatched")
                .map(|mut v| {
                    v["recovery_required"] = json!(true);
                    v["historical"] = json!(true);
                    v
                })
                .collect::<Vec<_>>()
        )),
        "src.report.templates" => Ok(src_templates()),
        "src.artifact.read" => {
            let reference = &params["ref_data"];
            let artifact_id = reference["sha256"].as_str().filter(|id| !id.is_empty())
                .or_else(|| reference["artifact_id"].as_str().filter(|id| !id.is_empty()))
                .context("Historical artifact reference unavailable")?;
            if reference["task_id"] != id
                || !crate::import::valid_id(id)
                || !crate::import::valid_id(artifact_id)
            {
                bail!("Historical artifact unavailable");
            }
            let bytes = fs::read(
                root(state)?
                    .join("_platform/src/tasks")
                    .join(id)
                    .join("cas")
                    .join(artifact_id),
            )?;
            Ok(
                json!({"encoding":"base64","data":base64::engine::general_purpose::STANDARD.encode(&bytes),"size":bytes.len()}),
            )
        }
        "src.workbench.asset.bulk" => {
            let items:Vec<_>=of_kind("asset").iter().filter(|v|!["destroyed","merged"].iter().any(|kind|v["payload"]["lifecycle"]==*kind)).map(|record|{let mut asset=asset_view(record);let aid=&asset["asset_id"];let obs:Vec<_>=payloads("asset_observation").into_iter().filter(|v|v["asset_id"]==*aid).collect();let relations:Vec<_>=payloads("asset_relation").into_iter().filter(|v|v["from_asset_id"]==*aid||v["to_asset_id"]==*aid).collect();asset["sources"]=json!(obs.iter().map(|v|json!({"source_kind":v["source_kind"],"source_id":v["source_id"]})).collect::<Vec<_>>());asset["observations"]=json!(obs);asset["relations"]=json!(relations);asset["_record_revision"]=record["revision"].clone();asset}).collect();
            Ok(json!({"total":items.len(),"items":items,"next_cursor":null,"complete":true}))
        }
        "src.connector.manifests" => Ok(src_connector_manifests()),
        "src.connector.manifest" => src_connector_manifests()
            .as_array()
            .and_then(|rows| {
                rows.iter()
                    .find(|v| v["connector_id"] == params["connector_id"])
                    .cloned()
            })
            .context("Historical connector declaration unavailable"),
        "src.workbench.duplicate.verdicts" => Ok(
            json!({"items":payloads("duplicate_verdict").into_iter().filter(|v|["dismissed","undetermined"].iter().any(|state|v["state"]==*state)).collect::<Vec<_>>()}),
        ),
        "src.workbench.asset.tasklinks" => Ok(json!({"items":payloads("task_asset_link")})),
        "src.workbench.history"
        | "src.workbench.todo.list"
        | "src.workbench.report.drafts"
        | "src.workbench.suggestions"
        | "src.workbench.finding.merges"
        | "src.workbench.finding.meta"
        | "src.workbench.asset.links" => {
            let record_kind = match kind {
                "src.workbench.history" => "workbench_history",
                "src.workbench.todo.list" => "workbench_todo",
                "src.workbench.report.drafts" => "workbench_report_draft",
                "src.workbench.suggestions" => "workbench_suggestion",
                "src.workbench.finding.merges" => "workbench_merge",
                "src.workbench.finding.meta" => "workbench_finding",
                _ => "workbench_link",
            };
            let mut items = payloads(record_kind);
            if kind == "src.workbench.suggestions" {
                items.retain(|v| v["decision"] == "pending");
            }
            if kind == "src.workbench.report.drafts" {
                items = of_kind("workbench_report_draft")
                    .iter()
                    .filter(|v| v["payload"]["lifecycle"] != "discarded")
                    .map(|v| {
                        let mut value = payload(v);
                        value["revision"] = v["revision"].clone();
                        value
                    })
                    .collect();
            }
            if kind == "src.workbench.finding.merges" {
                if let Some(filter) = params["kind"].as_str().filter(|v| !v.is_empty()) {
                    items.retain(|v| v["kind"] == filter);
                }
            }
            if kind == "src.workbench.history" {
                items.sort_by(|a, b| b["at"].as_str().cmp(&a["at"].as_str()));
                let start = params["cursor"].as_u64().unwrap_or(0) as usize;
                let size = params["limit"].as_u64().unwrap_or(500).clamp(1, 1000) as usize;
                let total = items.len();
                let page: Vec<_> = items.into_iter().skip(start).take(size).collect();
                let next = if start + page.len() < total {
                    Some(start + page.len())
                } else {
                    None
                };
                return Ok(json!({"items":page,"total":total,"next_cursor":next}));
            }
            Ok(json!({"total":items.len(),"items":items,"next_cursor":null}))
        }
        "src.research.plan" => payloads("research_plan")
            .last()
            .cloned()
            .context("Historical research plan unavailable"),
        "src.browser.service" => payloads("browser_service")
            .last()
            .cloned()
            .context("Historical browser service unavailable"),
        "src.execution.receipt" => {
            let iid = params["invocation_id"].as_str().unwrap_or("");
            payloads("execution_receipt")
                .into_iter()
                .find(|v| v["invocation_id"] == iid)
                .context("Historical execution receipt unavailable")
        }
        _ => bail!("unsupported historical query: {kind}"),
    }
}
fn report_view(state: &AppState, id: &str, params: &Params) -> Result<Value> {
    let rows = graph_events(state, id)?;
    let requested = params.get("report_seq").and_then(|v| v.parse::<i64>().ok());
    let generated: Vec<_> = rows
        .iter()
        .filter(|v| v["kind"] == "pentest_report_generated" && v["verified"].as_i64() == Some(1))
        .collect();
    let selected = generated
        .iter()
        .rev()
        .find(|v| requested.is_none_or(|n| v["seq"].as_i64() == Some(n)))
        .context("Historical generated pentest report unavailable")?;
    let p = &selected["payload"];
    let contract = rows
        .iter()
        .rev()
        .find(|v| v["kind"] == "pentest_contract")
        .map(|v| v["payload"].clone())
        .unwrap_or_else(|| json!({}));
    let mut findings = p["findings"].as_array().cloned().unwrap_or_default();
    for finding in &mut findings {
        if let Some(artifacts) = finding["selected_artifacts"].as_array_mut() {
            for artifact in artifacts {
                if artifact["artifact_id"].as_str().is_none_or(|aid| artifact_path(state, id, aid).is_err()) {
                    artifact["needs_reselect"] = json!(true);
                }
            }
        }
        if let Some(screenshots) = finding["screenshots"].as_array_mut() {
            for screenshot in screenshots {
                if screenshot["artifact_id"].as_str().is_none_or(|aid| artifact_path(state, id, aid).is_err()) {
                    screenshot["url"] = Value::Null;
                    screenshot["needs_reselect"] = json!(true);
                }
            }
        }
    }
    let versions:Vec<_>=generated.iter().rev().map(|v|json!({"seq":v["seq"],"created_at":v["ts"],"title":v["payload"]["title"],"finding_count":v["payload"]["findings"].as_array().map(Vec::len).unwrap_or(0)})).collect();
    let cutoff = p["source_graph_seq"]
        .as_u64()
        .or_else(|| selected["seq"].as_u64())
        .unwrap_or(0);
    let qualified = findings
        .iter()
        .filter(|v| v["review_status"] == "accepted")
        .count();
    let completed = rows.iter().any(|v| {
        v["kind"] == "pentest_goal_completed" && v["seq"].as_u64().is_some_and(|n| n <= cutoff)
    });
    let mut evidence_facts = Vec::new();
    for finding in &findings {
        if let Some(refs) = finding["fact_seqs"].as_array() {
            for reference in refs {
                if let Some(fact) = rows.iter().find(|v| v["seq"] == *reference) {
                    let data = &fact["payload"];
                    let provenance = &data["evidence_provenance"];
                    let mut screenshots = Vec::new();
                    if let Some(artifacts) = provenance["artifact_refs"].as_array() {
                        for artifact in artifacts {
                            if let Some(aid) = artifact["artifact_id"].as_str() {
                                if let Ok(path) = artifact_path(state, id, aid) {
                                    if path.extension().and_then(|v| v.to_str()).is_some_and(|v| {
                                        ["png", "jpg", "jpeg", "webp", "gif"].contains(&v)
                                    }) {
                                        screenshots.push(json!({"artifact_id":aid,"url":format!("/api/runs/{id}/pentest-evidence/{}/artifacts/{aid}/view",fact["seq"]),"filename":path.file_name().and_then(|v|v.to_str())}));
                                    }
                                }
                            }
                        }
                    }
                    evidence_facts.push(json!({"id":fact["seq"],"title":data["title"],"resource":data["resource_id"],"identity":data["identity"],"evidence":[{"fact_seq":fact["seq"],"provenance":provenance,"screenshots":screenshots}]}));
                }
            }
        }
    }
    let mut report = json!({"schema":"muteki.pentest.report.v3","contract_version":contract["version"].as_u64().unwrap_or(1),"report_seq":selected["seq"],"as_of_seq":p["source_graph_seq"].as_u64().or_else(||selected["seq"].as_u64()).unwrap_or(0),"current_graph_seq":rows.last().map(|v|v["seq"].clone()),"historical_snapshot":true,"report_versions":versions,"title":p["title"],"language":p["language"],"audience":p["audience"],"report_brief":contract["report_request"],"requested_sections":[],"custom_sections":p["sections"].as_array().cloned().unwrap_or_default(),"objective":contract["goal"],"report_goal_mode":contract["report_goal_mode"],"expected_findings":contract["expected_findings"],"qualified_findings":qualified,"executive_summary":p["summary"],"methodology":p["methodology"],"authorization":contract["authorization"],"execution_boundary":contract["target"],"objective_status":if completed {"met"} else {p["objective_status"].as_str().unwrap_or("unproven")},"coverage_status":p["coverage_status"].as_str().unwrap_or("unknown"),"terminal_reason":p["terminal_reason"],"delivery_status":"generated","finding_count":findings.len(),"findings":findings,"reader_findings":p["reader_findings"],"unaccepted_submissions":[],"evidence_facts":evidence_facts,"objective_evidence":[],"coverage":[],"limitations":p["limitations"].as_array().cloned().unwrap_or_default(),"summary_fact_seqs":p["summary_fact_seqs"],"methodology_fact_seqs":p["methodology_fact_seqs"],"goal_evidence_facts":[],"read_only":true});
    redact(&mut report);
    Ok(report)
}
async fn pentest_report(
    State(s): State<Arc<AppState>>,
    Path(id): Path<String>,
    Query(q): Query<Params>,
) -> Response {
    if crate::runs::is_local_run(&s, &id) {
        return crate::runs::pentest::get_report(State(s), Path(id), Query(q)).await;
    }
    response(report_view(&s, &id, &q))
}
async fn pentest_markdown(
    State(s): State<Arc<AppState>>,
    Path(id): Path<String>,
    Query(q): Query<Params>,
) -> Response {
    if crate::runs::is_local_run(&s, &id) {
        return crate::runs::pentest::get_markdown(State(s), Path(id), Query(q)).await;
    }
    match report_view(&s, &id, &q) {
        Ok(v) => {
            let mut text = format!(
                "# {}\n\n{}\n\n{}\n",
                v["title"].as_str().unwrap_or("Historical report"),
                v["executive_summary"].as_str().unwrap_or(""),
                v["methodology"].as_str().unwrap_or("")
            );
            if let Some(findings) = v["findings"].as_array() {
                for f in findings {
                    text.push_str(&format!(
                        "\n## {}\n\n```json\n{}\n```\n",
                        f["title"].as_str().unwrap_or("Finding"),
                        serde_json::to_string_pretty(f).unwrap_or_default()
                    ));
                }
            }
            (
                [(header::CONTENT_TYPE, "text/markdown; charset=utf-8")],
                text,
            )
                .into_response()
        }
        Err(e) => response(Err(e)),
    }
}
fn artifact_path(state: &AppState, id: &str, artifact: &str) -> Result<PathBuf> {
    stored_run(state, id)?;
    if !crate::import::valid_id(id)
        || artifact.len() != 12
        || !artifact.bytes().all(|v| v.is_ascii_hexdigit())
    {
        bail!("Historical artifact unavailable");
    }
    let base = root(state)?
        .join("sessions")
        .join(id)
        .join("workspace/arts");
    if !base.is_dir() {
        bail!("Historical artifact unavailable");
    }
    let matches: Vec<_> = fs::read_dir(&base)?
        .filter_map(|v| v.ok())
        .filter(|v| v.path().file_stem().is_some_and(|s| s == artifact))
        .collect();
    if matches.len() != 1 {
        bail!("Historical artifact unavailable");
    }
    let path = matches[0].path().canonicalize()?;
    if !path.starts_with(base.canonicalize()?) {
        bail!("Historical artifact unavailable");
    }
    Ok(path)
}
fn serve_evidence(state: &AppState, id: &str, seq: u64, artifact: &str, action: Option<&str>) -> Result<Response> {
    let rows = graph_events(state, id)?;
    let fact = rows
        .iter()
        .find(|v| v["seq"].as_u64() == Some(seq))
        .context("Historical evidence fact unavailable")?;
    let provenance = &fact["payload"]["evidence_provenance"];
    let allowed = provenance["artifact_id"] == artifact
        || provenance["artifact_refs"]
            .as_array()
            .is_some_and(|a| a.iter().any(|v| v["artifact_id"] == artifact));
    if !allowed {
        bail!("Historical evidence artifact unavailable");
    }
    let path = artifact_path(state, id, artifact)?;
    if let Some(action) = action {
        let mut response = serve_file(&path)?;
        if action == "download" {
            let suffix = path.extension().and_then(|value| value.to_str())
                .filter(|value| value.bytes().all(|byte| byte.is_ascii_alphanumeric()))
                .unwrap_or("bin");
            response.headers_mut().insert(header::CONTENT_DISPOSITION,
                format!("attachment; filename=\"evidence-{artifact}.{suffix}\"").parse()?);
        }
        return Ok(response);
    }
    let bytes = fs::read(&path)?;
    let content = if matches!(path.extension().and_then(|v| v.to_str()).unwrap_or(""),
        "txt" | "log" | "http" | "json" | "md") {
        Some(String::from_utf8_lossy(&bytes).into_owned())
    } else { None };
    Ok(([(header::CACHE_CONTROL, "no-store"), (header::X_CONTENT_TYPE_OPTIONS, "nosniff")],
        Json(json!({"fact_seq":seq,"artifact_id":artifact,
        "filename":path.file_name().and_then(|v| v.to_str()).unwrap_or("evidence"),
        "size":bytes.len(),"content":content}))).into_response())
}
fn serve_file(path: &std::path::Path) -> Result<Response> {
    let bytes = fs::read(path)?;
    let mime = match path
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_ascii_lowercase()
        .as_str()
    {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "json" => "application/json",
        "txt" | "log" | "http" | "md" => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    };
    Ok((
        [
            (header::CONTENT_TYPE, mime),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
            (header::CONTENT_SECURITY_POLICY, "sandbox"),
        ],
        bytes,
    )
        .into_response())
}
async fn evidence(
    State(s): State<Arc<AppState>>,
    Path((id, seq, artifact)): Path<(String, u64, String)>,
    Query(q): Query<Params>,
) -> Response {
    if crate::runs::is_local_run(&s, &id) {
        return crate::runs::pentest::get_evidence(State(s), Path((id, seq, artifact)), Query(q))
            .await;
    }
    match serve_evidence(&s, &id, seq, &artifact, None) {
        Ok(v) => v,
        Err(e) => response(Err(e)),
    }
}
async fn evidence_action(
    State(s): State<Arc<AppState>>,
    Path((id, seq, artifact, action)): Path<(String, u64, String, String)>,
    Query(q): Query<Params>,
) -> Response {
    if crate::runs::is_local_run(&s, &id) {
        return crate::runs::pentest::get_evidence_action(
            State(s),
            Path((id, seq, artifact, action)),
            Query(q),
        )
        .await;
    }
    if !["view", "download"].contains(&action.as_str()) {
        return response(Err(anyhow::anyhow!("Historical artifact unavailable")));
    }
    match serve_evidence(&s, &id, seq, &artifact, Some(&action)) {
        Ok(v) => v,
        Err(e) => response(Err(e)),
    }
}
async fn report_image(
    State(s): State<Arc<AppState>>,
    Path((id, poc)): Path<(String, String)>,
    Query(q): Query<Params>,
) -> Response {
    if crate::runs::is_local_run(&s, &id) {
        return crate::runs::pentest::get_image(State(s), Path((id, poc)), Query(q)).await;
    }
    let result = (|| -> Result<Response> {
        let conn = read_db(&graph_path(&s, &id)?)?;
        let row = table_rows(&conn, "pocs")?
            .into_iter()
            .find(|v| v["poc_id"] == poc)
            .context("Historical report image unavailable")?;
        let aid = row["artifact_id"]
            .as_str()
            .context("Historical image artifact unavailable")?;
        serve_file(&artifact_path(&s, &id, aid)?)
    })();
    match result {
        Ok(v) => v,
        Err(e) => response(Err(e)),
    }
}

fn src_templates() -> Value {
    serde_json::from_str(r###"[{"template_id": "generic-zh-v1", "platform": "generic", "template_version": "1.0.0", "mapping_version": "1.0.0", "source_url": "builtin://src/generic-zh-v1", "source_fetched_at": "2026-10-26T00:00:00Z", "field_mapping": {"title": "漏洞标题", "target": "影响目标", "attacker": "攻击者前提", "mechanism": "漏洞原理", "steps": "复现步骤", "confirmed_impact": "漏洞危害", "cvss": "CVSS 3.1", "remediation": "修复建议"}, "constraints": {"required": ["title", "target", "mechanism", "steps", "confirmed_impact"]}}, {"template_id": "enterprise-src-v1", "platform": "enterprise_src", "template_version": "1.0.0", "mapping_version": "1.0.0", "source_url": "builtin://src/enterprise-src-v1", "source_fetched_at": "2026-10-26T00:00:00Z", "field_mapping": {"title": "漏洞名称", "target": "受影响系统", "attacker": "利用条件", "mechanism": "漏洞分析", "steps": "验证过程", "confirmed_impact": "风险与影响", "cvss": "风险评分", "remediation": "整改建议"}, "constraints": {"required": ["title", "target", "attacker", "mechanism", "steps", "confirmed_impact", "remediation"]}}, {"template_id": "edusrc-v1", "platform": "edusrc", "template_version": "1.0.0", "mapping_version": "1.0.0", "source_url": "builtin://src/edusrc-v1", "source_fetched_at": "2026-10-26T00:00:00Z", "field_mapping": {"title": "漏洞标题", "target": "漏洞位置", "attacker": "利用前提", "mechanism": "漏洞成因", "steps": "测试过程", "confirmed_impact": "危害说明", "cvss": "CVSS", "remediation": "修复方案"}, "constraints": {"required": ["title", "target", "mechanism", "steps", "confirmed_impact"]}}]"###).expect("static legacy templates")
}

fn src_connector_manifests() -> Value {
    serde_json::from_str(r###"[{"connector_id":"aws_ec2","version":"aws-cli.describe-instances.v1","source_schema":"aws.ec2.describe-instances.response+json","capabilities":["query","read","paginate"],"asset_types":{"instance":"cloud_resource"},"field_mapping":{},"pagination":{"kind":"next_token","reliable_full":"when_all_pages_succeed"},"credentials":{"required":false,"reference_fields":["credential_ref"],"permission":"ec2:DescribeInstances"},"cost":{"actual_cost":"unknown_unless_upstream_receipt","retry":"submitted_unknown_not_replayed"},"runtime":{"kind":"aws_cli","required_executable":"aws","hooks":false},"evidence":{"raw_response":true,"stable_source_link":true,"screenshots":false},"available":false,"historical":true,"read_only":true},{"connector_id":"bbot","version":"source-file.v1","source_schema":"bbot.typed-events.v3+jsonl","capabilities":["import_file","read"],"asset_types":{"DNS_NAME":"domain","IP_ADDRESS":"ip","OPEN_TCP_PORT":"service","URL":"http_endpoint","FINDING":"external_observation"},"field_mapping":{},"pagination":{"kind":"complete_file","reliable_full":true},"credentials":{"required":false,"reference_fields":[]},"cost":{"actual_cost":"not_applicable","retry":"committed_items_not_repeated"},"runtime":{"kind":"file","executes_hooks":false},"evidence":{"raw_source":true,"stable_source_link":true,"screenshots":false},"available":false,"historical":true,"read_only":true},{"connector_id":"bbrf","version":"source-file.v1","source_schema":"bbrf.export+jsonl","capabilities":["import_file","read"],"asset_types":{"domain":"domain","ip":"ip","url":"http_endpoint"},"field_mapping":{},"pagination":{"kind":"complete_file","reliable_full":true},"credentials":{"required":false,"reference_fields":[]},"cost":{"actual_cost":"not_applicable","retry":"committed_items_not_repeated"},"runtime":{"kind":"file","executes_hooks":false},"evidence":{"raw_source":true,"stable_source_link":true,"screenshots":false},"available":false,"historical":true,"read_only":true},{"connector_id":"burp","version":"source-file.v1","source_schema":"burp.xml+har","capabilities":["import_file","read"],"asset_types":{"http_exchange":"http_exchange"},"field_mapping":{},"pagination":{"kind":"complete_file","reliable_full":true},"credentials":{"required":false,"reference_fields":[]},"cost":{"actual_cost":"not_applicable","retry":"committed_items_not_repeated"},"runtime":{"kind":"file","executes_hooks":false},"evidence":{"raw_source":true,"stable_source_link":true,"screenshots":false},"available":false,"historical":true,"read_only":true},{"connector_id":"caido","version":"source-file.v1","source_schema":"caido.har+jsonl","capabilities":["import_file","read"],"asset_types":{"http_exchange":"http_exchange"},"field_mapping":{},"pagination":{"kind":"complete_file","reliable_full":true},"credentials":{"required":false,"reference_fields":[]},"cost":{"actual_cost":"not_applicable","retry":"committed_items_not_repeated"},"runtime":{"kind":"file","executes_hooks":false},"evidence":{"raw_source":true,"stable_source_link":true,"screenshots":false},"available":false,"historical":true,"read_only":true},{"connector_id":"cli","version":"source-file.v1","source_schema":"cli.json+jsonl+csv","capabilities":["import_file","read"],"asset_types":{"asset":"source_observation"},"field_mapping":{},"pagination":{"kind":"complete_file","reliable_full":true},"credentials":{"required":false,"reference_fields":[]},"cost":{"actual_cost":"not_applicable","retry":"committed_items_not_repeated"},"runtime":{"kind":"file","executes_hooks":false},"evidence":{"raw_source":true,"stable_source_link":true,"screenshots":false},"available":false,"historical":true,"read_only":true},{"connector_id":"cloud_inventory","version":"source-file.v1","source_schema":"cloud.inventory+json+csv","capabilities":["import_file","read"],"asset_types":{"resource":"cloud_resource"},"field_mapping":{},"pagination":{"kind":"complete_file","reliable_full":true},"credentials":{"required":false,"reference_fields":[]},"cost":{"actual_cost":"not_applicable","retry":"committed_items_not_repeated"},"runtime":{"kind":"file","executes_hooks":false},"evidence":{"raw_source":true,"stable_source_link":true,"screenshots":false},"available":false,"historical":true,"read_only":true},{"connector_id":"external_event","version":"source-file.v1","source_schema":"external.source-event.v1","capabilities":["receive_event","idempotent_events","read"],"asset_types":{"event":"external_observation"},"field_mapping":{},"pagination":{"kind":"complete_file","reliable_full":true},"credentials":{"required":false,"reference_fields":[]},"cost":{"actual_cost":"not_applicable","retry":"committed_items_not_repeated"},"runtime":{"kind":"file","executes_hooks":false},"evidence":{"raw_source":true,"stable_source_link":true,"screenshots":false},"available":false,"historical":true,"read_only":true},{"connector_id":"fofa","version":"api-v1.search-all.1","source_schema":"fofa.search-all.response+json","capabilities":["query","read","paginate"],"asset_types":{"host":"service","ip":"ip","domain":"domain","icp":"ownership_candidate"},"field_mapping":{"host":"host","ip":"ip","port":"port","protocol":"protocol","title":"title","domain":"domain","icp":"icp"},"pagination":{"kind":"page","page_size_max":10000,"reliable_full":false,"source_delay":"upstream"},"credentials":{"required":true,"reference_fields":["email_ref","key_ref"],"permission":"search"},"cost":{"actual_cost":"unknown_unless_upstream_receipt","retry":"submitted_unknown_not_replayed","rate_limit":"upstream"},"runtime":{"kind":"https","base_url":"https://fofa.info"},"evidence":{"raw_response":true,"stable_source_link":true,"screenshots":false},"available":false,"historical":true,"read_only":true},{"connector_id":"github","version":"repository-read.v1","source_schema":"github.repository+commit","capabilities":["read_repository"],"asset_types":{"repository":"repository"},"field_mapping":{},"pagination":{"kind":"single_document","reliable_full":true},"credentials":{"required":false,"reference_fields":["token_ref"],"permission":"read_repository"},"cost":{"actual_cost":"unknown_unless_upstream_receipt","retry":"submitted_unknown_not_replayed"},"runtime":{"kind":"https","self_hosted_base_url_supported":true},"evidence":{"raw_response":true,"stable_source_link":true,"screenshots":false},"available":false,"historical":true,"read_only":true},{"connector_id":"gitlab","version":"repository-read.v1","source_schema":"gitlab.repository+commit","capabilities":["read_repository"],"asset_types":{"repository":"repository"},"field_mapping":{},"pagination":{"kind":"single_document","reliable_full":true},"credentials":{"required":false,"reference_fields":["token_ref"],"permission":"read_repository"},"cost":{"actual_cost":"unknown_unless_upstream_receipt","retry":"submitted_unknown_not_replayed"},"runtime":{"kind":"https","self_hosted_base_url_supported":true},"evidence":{"raw_response":true,"stable_source_link":true,"screenshots":false},"available":false,"historical":true,"read_only":true},{"connector_id":"quake","version":"api-v3.quake-service.1","source_schema":"quake.service.response+json","capabilities":["query","read","paginate"],"asset_types":{"service":"service","ip":"ip","domain":"domain","app":"product_candidate"},"field_mapping":{"service":"service","ip":"ip","port":"port","transport":"transport","domain":"domain"},"pagination":{"kind":"offset","page_size_max":500,"reliable_full":false,"source_delay":"upstream"},"credentials":{"required":true,"reference_fields":["token_ref"],"permission":"quake_service_search"},"cost":{"actual_cost":"unknown_unless_upstream_receipt","retry":"submitted_unknown_not_replayed","rate_limit":"upstream"},"runtime":{"kind":"https","base_url":"https://quake.360.net"},"evidence":{"raw_response":true,"stable_source_link":true,"screenshots":false},"available":false,"historical":true,"read_only":true},{"connector_id":"reconftw","version":"source-file.v1","source_schema":"reconftw.assets+jsonl","capabilities":["import_file","read"],"asset_types":{"domain":"domain","ip":"ip","url":"http_endpoint","service":"service"},"field_mapping":{},"pagination":{"kind":"complete_file","reliable_full":true},"credentials":{"required":false,"reference_fields":[]},"cost":{"actual_cost":"not_applicable","retry":"committed_items_not_repeated"},"runtime":{"kind":"file","executes_hooks":false},"evidence":{"raw_source":true,"stable_source_link":true,"screenshots":false},"available":false,"historical":true,"read_only":true},{"connector_id":"scopesentry","version":"source-file.v1","source_schema":"scopesentry.export+json","capabilities":["import_file","read","query_when_endpoint_configured"],"asset_types":{"domain":"domain","service":"service","app":"app_candidate","miniprogram":"miniprogram_candidate"},"field_mapping":{},"pagination":{"kind":"complete_file","reliable_full":true},"credentials":{"required":false,"reference_fields":[]},"cost":{"actual_cost":"not_applicable","retry":"committed_items_not_repeated"},"runtime":{"kind":"file","executes_hooks":false},"evidence":{"raw_source":true,"stable_source_link":true,"screenshots":false},"available":false,"historical":true,"read_only":true},{"connector_id":"shodan","version":"api.shodan-host-search.1","source_schema":"shodan.host-search.response+json","capabilities":["query","read","paginate"],"asset_types":{"matches":"service","ip_str":"ip","domains":"domain_candidates"},"field_mapping":{"matches":"matches","ip_str":"ip","port":"port","transport":"transport","domains":"domains"},"pagination":{"kind":"page","page_size_max":100,"reliable_full":false,"source_delay":"upstream"},"credentials":{"required":true,"reference_fields":["key_ref"],"permission":"host_search"},"cost":{"actual_cost":"unknown_unless_upstream_receipt","retry":"submitted_unknown_not_replayed","rate_limit":"upstream"},"runtime":{"kind":"https","base_url":"https://api.shodan.io"},"evidence":{"raw_response":true,"stable_source_link":true,"screenshots":false},"available":false,"historical":true,"read_only":true},{"connector_id":"zoomeye","version":"api.host-search.1","source_schema":"zoomeye.host-search.response+json","capabilities":["query","read","paginate"],"asset_types":{"data":"service","ip":"ip","site":"domain_candidate","app":"product_candidate"},"field_mapping":{"data":"data","ip":"ip","port":"port","site":"site","app":"app"},"pagination":{"kind":"page","page_size_max":100,"reliable_full":false,"source_delay":"upstream"},"credentials":{"required":true,"reference_fields":["key_ref"],"permission":"host_search"},"cost":{"actual_cost":"unknown_unless_upstream_receipt","retry":"submitted_unknown_not_replayed","rate_limit":"upstream"},"runtime":{"kind":"https","base_url":"https://api.zoomeye.org"},"evidence":{"raw_response":true,"stable_source_link":true,"screenshots":false},"available":false,"historical":true,"read_only":true}]"###).expect("static connector declarations")
}

#[cfg(test)]
mod historical_artifact_tests {
    use super::serve_file;

    #[test]
    fn historical_pentest_artifact_reads_original_path_without_digest() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("evidence.json");
        std::fs::write(&path, br#"{"source":"historical"}"#).unwrap();
        assert!(serve_file(&path).is_ok());
        std::fs::write(&path, br#"{"source":"changed"}"#).unwrap();
        assert!(serve_file(&path).is_ok());
    }
}
