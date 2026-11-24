//! What each chat's agent is driving in the current turn: its browser pages
//! and computer-use apps, each with its latest picture, for the live dock.
//! Pictures come from the agent's own screenshots and, while the dock is
//! watched, from fresh headless browser captures. Computer pictures come only
//! from explicit agent screenshots: CUA app lookup/capture can launch apps.

use crate::AppState;
use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::{HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
    routing::get,
};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

/// A fresh capture is taken at most this often per item while watched.
const REFRESH: Duration = Duration::from_secs(3);
const PICTURE_MAX_BYTES: u64 = 16 * 1024 * 1024;

#[derive(Clone)]
struct Picture {
    mime: String,
    bytes: Arc<Vec<u8>>,
    version: u64,
}

struct Item {
    key: String,
    kind: &'static str,
    label: String,
    detail: String,
    host: String,
    app_id: String,
    updated_ms: u64,
    picture: Option<Picture>,
    attempted: Option<Instant>,
    refreshing: bool,
}

#[derive(Default)]
struct Thread {
    turn: String,
    items: Vec<Item>,
}

#[derive(Default)]
pub struct Hub {
    threads: Mutex<HashMap<String, Thread>>,
    next: std::sync::atomic::AtomicU64,
}

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |elapsed| elapsed.as_millis() as u64)
}

fn mime_of(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") { Some("image/png") }
    else if bytes.starts_with(&[0xff, 0xd8, 0xff]) { Some("image/jpeg") }
    else if bytes.len() > 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" { Some("image/webp") }
    else { None }
}

async fn read_picture(path: &str) -> Option<Vec<u8>> {
    let metadata = tokio::fs::metadata(path).await.ok()?;
    if !metadata.is_file() || metadata.len() > PICTURE_MAX_BYTES { return None; }
    tokio::fs::read(path).await.ok()
}

/// The chat's running turn, or "" when it is idle.
pub fn running_turn(s: &AppState, thread_id: &str) -> String {
    s.store.get("conversation_threads", thread_id).ok().flatten()
        .and_then(|doc| doc["thread"]["state"]["running_turn_id"].as_str().map(str::to_owned))
        .unwrap_or_default()
}

/// "Window: \"title\", App: Name." heads each CUA app state.
fn window_and_app(text: &str) -> (Option<String>, Option<String>) {
    let line = text.lines().find(|line| line.contains("App: ")).unwrap_or("");
    let app = line.rsplit_once("App: ").map(|(_, rest)| rest.trim().trim_end_matches('.').trim().to_owned())
        .filter(|name| !name.is_empty());
    let window = line.split_once("Window: \"").and_then(|(_, rest)| rest.rsplit_once("\", App:").map(|(title, _)| title.to_owned()))
        .filter(|title| !title.is_empty());
    (window, app)
}

impl Hub {
    pub(crate) fn clear_computer(&self, thread_id: &str, turn: &str) {
        let mut threads = self.threads.lock().unwrap();
        if let Some(thread) = threads.get_mut(thread_id).filter(|thread| thread.turn == turn) {
            thread.items.retain(|item| item.kind != "computer");
        }
    }

    /// Drop the previous turn's pictures as soon as its state is committed.
    pub(crate) fn retain_turn(&self, thread_id: &str, turn: &str) {
        let mut threads = self.threads.lock().unwrap();
        if threads.get(thread_id).is_some_and(|thread| thread.turn != turn || turn.is_empty()) {
            threads.remove(thread_id);
        }
    }

    fn upsert(&self, thread_id: &str, turn: &str, key: &str, kind: &'static str, update: impl FnOnce(&mut Item)) {
        let mut threads = self.threads.lock().unwrap();
        let thread = threads.entry(thread_id.to_owned()).or_default();
        if thread.turn != turn {
            thread.turn = turn.to_owned();
            thread.items.clear();
        }
        let index = match thread.items.iter().position(|item| item.key == key) {
            Some(index) => index,
            None => {
                thread.items.push(Item { key: key.to_owned(), kind, label: String::new(), detail: String::new(),
                    host: String::new(), app_id: String::new(), updated_ms: 0, picture: None, attempted: None, refreshing: false });
                thread.items.len() - 1
            }
        };
        let item = &mut thread.items[index];
        item.updated_ms = now_ms();
        update(item);
    }

    fn picture(&self, bytes: Vec<u8>) -> Option<Picture> {
        let mime = mime_of(&bytes)?;
        let version = self.next.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
        Some(Picture { mime: mime.to_owned(), bytes: Arc::new(bytes), version })
    }

    /// One successful `chat_browser_*` result.
    pub async fn browser(&self, s: &AppState, thread_id: &str, turn: &str, result: &Value) {
        if turn.is_empty() { return; }
        let host = result["host"].as_str().unwrap_or("browser").to_owned();
        let url = result["url"].as_str().unwrap_or("").to_owned();
        let title = result["title"].as_str().unwrap_or("").to_owned();
        let shot = match result["path"].as_str().filter(|_| result["capture_id"].is_string()) {
            Some(path) => read_picture(path).await.and_then(|bytes| self.picture(bytes)),
            None => None,
        };
        let _guard = s.chat.active.lock().await;
        if running_turn(s, thread_id) != turn { return; }
        self.upsert(thread_id, turn, &format!("browser:{host}"), "browser", |item| {
            item.host = host;
            if !url.is_empty() {
                let site = url::Url::parse(&url).ok().and_then(|parsed| parsed.host_str().map(str::to_owned)).unwrap_or_default();
                item.label = if title.is_empty() { site } else { title };
                item.detail = url;
            }
            if shot.is_some() { item.picture = shot; }
        });
    }

    /// One `chat_computer_js` result: `surface` is the CUA tool surface meta.
    pub async fn computer(&self, s: &AppState, thread_id: &str, turn: &str, surface: &Value, result: &Value) {
        let Some(app_id) = surface["app"]["appId"].as_str().filter(|id| !id.is_empty()).map(str::to_owned) else { return };
        if turn.is_empty() { return; }
        let blocks = result["content"].as_array().cloned().unwrap_or_default();
        let (window, app) = blocks.iter().filter_map(|block| block["text"].as_str()).map(window_and_app)
            .find(|(window, app)| window.is_some() || app.is_some()).unwrap_or_default();
        let mut shot = None;
        for path in blocks.iter().rev().filter(|block| block["type"] == "image").filter_map(|block| block["path"].as_str()) {
            if let Some(bytes) = read_picture(path).await { shot = self.picture(bytes); break; }
        }
        let _guard = s.chat.active.lock().await;
        if running_turn(s, thread_id) != turn { return; }
        self.upsert(thread_id, turn, &format!("computer:{app_id}"), "computer", |item| {
            item.host = "computer".into();
            item.app_id = app_id.clone();
            if let Some(app) = app { item.label = app; } else if item.label.is_empty() { item.label = app_id; }
            if let Some(window) = window { item.detail = window; }
            if shot.is_some() { item.picture = shot; }
        });
    }

    fn snapshot(&self, thread_id: &str, turn: &str) -> Value {
        let threads = self.threads.lock().unwrap();
        let items: Vec<Value> = threads.get(thread_id).filter(|thread| !turn.is_empty() && thread.turn == turn)
            .map(|thread| thread.items.iter().map(|item| json!({"key": item.key, "kind": item.kind, "label": item.label,
                "detail": item.detail, "host": item.host, "app_id": item.app_id, "updated_at": item.updated_ms,
                "picture": item.picture.as_ref().map(|picture| picture.version)})).collect())
            .unwrap_or_default();
        json!({"turn_id": turn, "items": items})
    }

    /// Items whose picture is older than `REFRESH`, claimed for one capture each.
    fn due(&self, thread_id: &str, turn: &str) -> Vec<String> {
        let mut threads = self.threads.lock().unwrap();
        let Some(thread) = threads.get_mut(thread_id).filter(|thread| !turn.is_empty() && thread.turn == turn) else { return Vec::new() };
        let mut due = Vec::new();
        for item in thread.items.iter_mut() {
            let capturable = item.kind == "browser" && item.host == "headless";
            if !capturable || item.refreshing || item.attempted.is_some_and(|at| at.elapsed() < REFRESH) { continue; }
            item.refreshing = true;
            item.attempted = Some(Instant::now());
            due.push(item.key.clone());
        }
        due
    }

    fn captured(&self, thread_id: &str, turn: &str, key: &str, bytes: Option<Vec<u8>>) {
        let picture = bytes.and_then(|bytes| self.picture(bytes));
        let mut threads = self.threads.lock().unwrap();
        if let Some(item) = threads.get_mut(thread_id).filter(|thread| thread.turn == turn)
            .and_then(|thread| thread.items.iter_mut().find(|item| item.key == key)) {
            item.refreshing = false;
            if picture.is_some() { item.picture = picture; }
        }
    }
}

async fn capture(s: Arc<AppState>, thread_id: String, turn: String, key: String) {
    let result = s.browser.thumbnail(&thread_id).await;
    let bytes = match result {
        Ok(bytes) => Some(bytes),
        Err(error) => {
            tracing::debug!(thread = %thread_id, key = %key, code = %error["code"].as_str().unwrap_or(""), "Activity picture not refreshed");
            None
        }
    };
    s.activity.captured(&thread_id, &turn, &key, bytes);
}

pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/threads/{id}/activity", get(list))
        .route("/api/threads/{id}/activity/picture", get(picture))
}

fn missing() -> Response {
    (StatusCode::NOT_FOUND, Json(json!({"error": {"code": "chat.activity.not_found", "message": "对话或活动画面不存在"}}))).into_response()
}

async fn list(State(s): State<Arc<AppState>>, Path(thread_id): Path<String>) -> Response {
    if !s.store.get("conversation_threads", &thread_id).ok().flatten().is_some() { return missing(); }
    let turn = running_turn(&s, &thread_id);
    for key in s.activity.due(&thread_id, &turn) {
        tokio::spawn(capture(s.clone(), thread_id.clone(), turn.clone(), key));
    }
    Json(s.activity.snapshot(&thread_id, &turn)).into_response()
}

async fn picture(State(s): State<Arc<AppState>>, Path(thread_id): Path<String>, Query(query): Query<HashMap<String, String>>) -> Response {
    let key = query.get("key").map(String::as_str).unwrap_or("");
    let picture = {
        let threads = s.activity.threads.lock().unwrap();
        threads.get(&thread_id).and_then(|thread| thread.items.iter().find(|item| item.key == key)).and_then(|item| item.picture.clone())
    };
    let Some(picture) = picture else { return missing() };
    let mut response = (*picture.bytes).clone().into_response();
    let headers = response.headers_mut();
    if let Ok(value) = HeaderValue::from_str(&picture.mime) { headers.insert(header::CONTENT_TYPE, value); }
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("private, no-store"));
    response
}
