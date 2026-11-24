//! Interactive workspace terminals: PTY shells per conversation thread,
//! listed and controlled over HTTP and streamed over a WebSocket.
//!
//! Sessions live only in this process. Ended sessions stay listed until the
//! process exits; only running ones count toward the per-thread limit.
use crate::{AppState, access_api::{Access, SessionBinding}, chat};
use axum::{
    Extension, Json, Router,
    extract::{
        Path, State,
        ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade},
    },
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{delete, get, post},
};
use base64::{Engine as _, engine::general_purpose::STANDARD as B64};
use serde_json::{Value, json};
use std::{
    collections::VecDeque,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::sync::broadcast;

const MAX_SESSIONS_PER_THREAD: usize = 6;
const DEFAULT_HISTORY_BYTES: usize = 256 * 1024;
const DEFAULT_IDLE_TTL_SECS: f64 = 2700.0;
const SUBSCRIBER_QUEUE: usize = 256;
const COMMAND_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_COMMAND_OUTPUT: usize = 256_000;

type Failure = (StatusCode, Json<Value>);

fn error(status: StatusCode, code: &str, category: &str, message: &str) -> Failure {
    (
        status,
        Json(json!({"error":{"code":code,"message":message,"category":category,"recovery_hint":"","correlation_id":null,"retryable":false,"detail":{}}})),
    )
}

fn surface(status: StatusCode, message: &str) -> Failure {
    error(status, "conversation.workspace.surface_failed", "validation", message)
}

#[derive(Debug, Clone, PartialEq)]
pub struct TerminalError {
    pub code: &'static str,
    pub message: String,
}

impl TerminalError {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self { code, message: message.into() }
    }

    fn failure(&self) -> Failure {
        let category = if self.code.contains("exited") || self.code.contains("changed") { "state" } else { "validation" };
        error(StatusCode::BAD_REQUEST, self.code, category, &self.message)
    }
}

fn unix_now() -> f64 {
    chrono::Utc::now().timestamp_millis() as f64 / 1000.0
}

fn env_number(name: &str, default: f64, min: f64) -> f64 {
    std::env::var(name)
        .ok()
        .and_then(|value| value.trim().parse::<f64>().ok())
        .map(|value| value.max(min))
        .unwrap_or(default)
}

struct SessionState {
    status: String,
    end_reason: Option<String>,
    end_code: Option<String>,
    cols: u16,
    rows: u16,
    last_active_at: f64,
    diverged: bool,
    history: VecDeque<u8>,
    truncated: bool,
    closed: bool,
    final_screen: Option<(String, bool, Option<i32>)>,
    /// Trailing bytes of an incomplete UTF-8 sequence held for the next frame.
    carry: Vec<u8>,
}

pub struct TerminalSession {
    control_owned: bool,
    session_id: String,
    thread_id: String,
    workspace_id: String,
    root_path: String,
    name: String,
    created_at: f64,
    history_limit: usize,
    state: Mutex<SessionState>,
    terminal: Mutex<Option<term::Terminal>>,
    frames: broadcast::Sender<Value>,
}

impl TerminalSession {
    fn public_locked(&self, state: &SessionState) -> Value {
        json!({
            "session_id": self.session_id, "thread_id": self.thread_id, "workspace_id": self.workspace_id,
            "root_path": self.root_path, "cwd_label": self.root_path, "name": self.name,
            "status": state.status, "end_reason": state.end_reason, "end_code": state.end_code,
            "cols": state.cols, "rows": state.rows, "created_at": self.created_at,
            "last_active_at": state.last_active_at, "history_bytes": state.history.len(),
            "history_truncated": state.truncated, "workspace_diverged": state.diverged,
        })
    }

    pub fn public(&self) -> Value {
        self.public_locked(&self.state.lock().unwrap())
    }

    fn status_frame(&self, state: &SessionState, message: Option<&str>) -> Value {
        let mut frame = json!({"type":"status","session":self.public_locked(state)});
        if let Some(message) = message {
            frame["message"] = json!(message);
        }
        frame
    }

    fn running(&self) -> bool {
        self.state.lock().unwrap().status == "running"
    }

    fn touch(state: &mut SessionState) {
        state.last_active_at = unix_now();
    }

    /// Fold one PTY output chunk into history and fan it out.
    fn output(&self, bytes: &[u8]) {
        let mut state = self.state.lock().unwrap();
        Self::touch(&mut state);
        state.history.extend(bytes);
        let excess = state.history.len().saturating_sub(self.history_limit);
        if excess > 0 {
            state.history.drain(..excess);
            state.truncated = true;
        }
        let mut chunk = std::mem::take(&mut state.carry);
        chunk.extend_from_slice(bytes);
        let keep = incomplete_utf8_tail(&chunk);
        state.carry = chunk.split_off(chunk.len() - keep);
        if !chunk.is_empty() {
            let _ = self.frames.send(json!({"type":"stdout","data":B64.encode(&chunk)}));
        }
        if state.truncated {
            let _ = self.frames.send(json!({"type":"truncated","history_truncated":true,"history_bytes":state.history.len()}));
        }
    }

    fn exited(&self, code: Option<i32>) {
        let screen = self.terminal.lock().unwrap().as_ref().map(|terminal| { let snapshot=terminal.peek_snapshot(); (snapshot.text(), snapshot.exited, snapshot.exit_code) });
        let mut state = self.state.lock().unwrap();
        let carry = std::mem::take(&mut state.carry);
        if !carry.is_empty() {
            let _ = self.frames.send(json!({"type":"stdout","data":B64.encode(&carry)}));
        }
        if state.closed || state.status != "running" {
            return;
        }
        state.final_screen = screen;
        state.status = "exited".into();
        state.end_code = Some("exited".into());
        state.end_reason = Some(match code {
            Some(code) => format!("进程已退出 (code {code})"),
            None => "进程已退出".into(),
        });
        let frame = self.status_frame(&state, None);
        drop(state);
        let _ = self.frames.send(frame);
        self.terminal.lock().unwrap().take();
    }

    /// End the shell. `closed` is a user close; any other code is the status.
    fn close(&self, reason: &str, code: &str) {
        let screen = self.control_screen();
        let mut state = self.state.lock().unwrap();
        if state.closed {
            return;
        }
        state.closed = true;
        if screen.is_some() { state.final_screen = screen; }
        if state.status == "running" {
            state.status = if code == "closed" { "killed".into() } else { code.to_owned() };
            state.end_code = Some(code.to_owned());
            state.end_reason = Some(reason.to_owned());
        }
        let frame = self.status_frame(&state, None);
        drop(state);
        if let Some(terminal) = self.terminal.lock().unwrap().take() {
            terminal.kill();
        }
        let _ = self.frames.send(frame);
    }

    fn close_idle_shell(&self) {
        let terminal = self.terminal.lock().unwrap();
        let Some(terminal) = terminal.as_ref() else { return; };
        let mark = terminal.activity_mark();
        if terminal.idle_shell() != Some(true) || terminal.activity_mark() != mark { return; }
        let snapshot = terminal.peek_snapshot();
        terminal.kill();
        let mut state = self.state.lock().unwrap();
        state.final_screen = Some((snapshot.text(), snapshot.exited, snapshot.exit_code));
        state.closed = true;
        state.status = "killed".into();
        state.end_code = Some("settled".into());
        state.end_reason = Some("会话已收尾".into());
        let frame = self.status_frame(&state, None);
        drop(state);
        let _ = self.frames.send(frame);
    }

    /// Mark the session diverged when the thread's workspace moved under it.
    fn ensure_workspace(&self, root: Option<&str>, workspace_id: &str) {
        let resolved = root.and_then(|root| std::fs::canonicalize(root).ok());
        let same = resolved.is_some_and(|path| path == PathBuf::from(&self.root_path)) && workspace_id == self.workspace_id;
        let mut state = self.state.lock().unwrap();
        if same || state.diverged {
            return;
        }
        state.diverged = true;
        if state.end_code.is_none() {
            state.end_code = Some("workspace_changed".into());
        }
        if state.status == "running" {
            state.end_reason = Some("工作区已变更，请新开终端".into());
        }
        let frame = self.status_frame(&state, Some("工作区已变更，请新开终端"));
        drop(state);
        let _ = self.frames.send(frame);
    }

    fn writable(&self) -> Result<(), TerminalError> {
        let state = self.state.lock().unwrap();
        if state.diverged {
            return Err(TerminalError::new("conversation.terminal.workspace_changed", "工作区已变更，请新开终端"));
        }
        if state.status != "running" || state.closed {
            return Err(TerminalError::new("conversation.terminal.exited", "终端会话已结束"));
        }
        Ok(())
    }

    fn write(&self, bytes: Vec<u8>) -> Result<(), TerminalError> {
        self.writable()?;
        let terminal = self.terminal.lock().unwrap();
        let terminal = terminal
            .as_ref()
            .ok_or_else(|| TerminalError::new("conversation.terminal.exited", "终端会话已结束"))?;
        terminal.write_input(bytes);
        Self::touch(&mut self.state.lock().unwrap());
        Ok(())
    }

    fn resize(&self, cols: u16, rows: u16) {
        let cols = cols.clamp(20, 500);
        let rows = rows.clamp(5, 200);
        if let Some(terminal) = self.terminal.lock().unwrap().as_ref() {
            terminal.resize(cols as usize, rows as usize);
        }
        let mut state = self.state.lock().unwrap();
        state.cols = cols;
        state.rows = rows;
        Self::touch(&mut state);
    }

    /// Interrupt the foreground job the way a typed Ctrl-C does.
    pub(crate) fn interrupt(&self) -> Result<(), TerminalError> {
        if self.state.lock().unwrap().diverged {
            return Err(TerminalError::new("conversation.terminal.workspace_changed", "工作区已变更，请新开终端"));
        }
        if !self.running() {
            return Ok(());
        }
        if let Some(terminal) = self.terminal.lock().unwrap().as_ref() {
            terminal.write_raw(vec![0x03]);
        }
        Self::touch(&mut self.state.lock().unwrap());
        Ok(())
    }

    pub(crate) fn control_screen(&self) -> Option<(String,bool,Option<i32>)> {
        if let Some(terminal)=self.terminal.lock().unwrap().as_ref(){let snapshot=terminal.peek_snapshot();return Some((snapshot.text(),snapshot.exited,snapshot.exit_code));}
        self.state.lock().unwrap().final_screen.clone()
    }

    pub(crate) fn is_running(&self) -> bool {
        self.running()
    }

    pub(crate) fn read_history_text(&self, max_bytes: usize) -> (String, bool) {
        let state = self.state.lock().unwrap();
        let total = state.history.len();
        let cap = max_bytes.clamp(256, DEFAULT_HISTORY_BYTES);
        let skip = total.saturating_sub(cap);
        let bytes: Vec<u8> = state.history.iter().skip(skip).copied().collect();
        (String::from_utf8_lossy(&bytes).into_owned(), state.truncated || skip > 0)
    }

    pub(crate) fn write_input_str(&self, input: &str) -> Result<(), TerminalError> {
        self.write(input.as_bytes().to_vec())
    }

    pub(crate) fn resize_session(&self, cols: u16, rows: u16) {
        self.resize(cols, rows);
    }

    pub(crate) fn close_session(&self, reason: &str) {
        self.close(reason, "closed");
    }
}

fn incomplete_utf8_tail(bytes: &[u8]) -> usize {
    for back in 1..=bytes.len().min(3) {
        let byte = bytes[bytes.len() - back];
        if byte & 0b1100_0000 == 0b1000_0000 {
            continue;
        }
        let width = match byte {
            0xC0..=0xDF => 2,
            0xE0..=0xEF => 3,
            0xF0..=0xF7 => 4,
            _ => return 0,
        };
        return if width > back { back } else { 0 };
    }
    0
}

#[derive(Default)]
pub struct Terminals {
    sessions: Mutex<Vec<Arc<TerminalSession>>>,
}

impl Terminals {
    fn reap_idle(&self) {
        let ttl = env_number("MUTEKI_TERMINAL_IDLE_TTL_SECONDS", DEFAULT_IDLE_TTL_SECS, 60.0);
        let now = unix_now();
        let idle: Vec<_> = self
            .sessions
            .lock()
            .unwrap()
            .iter()
            .filter(|session| {
                let state = session.state.lock().unwrap();
                !session.control_owned && state.status == "running" && now - state.last_active_at > ttl
            })
            .cloned()
            .collect();
        for session in idle {
            session.close("空闲超时，会话已关闭", "idle_timeout");
        }
    }

    pub(crate) fn get(&self, thread_id: &str, session_id: &str) -> Option<Arc<TerminalSession>> {
        self.reap_idle();
        self.sessions
            .lock()
            .unwrap()
            .iter()
            .find(|session| session.session_id == session_id && session.thread_id == thread_id)
            .cloned()
    }

    pub(crate) fn list(&self, thread_id: &str) -> Vec<Arc<TerminalSession>> {
        self.reap_idle();
        self.sessions
            .lock()
            .unwrap()
            .iter()
            .filter(|session| session.thread_id == thread_id)
            .cloned()
            .collect()
    }

    pub(crate) fn settle_for_thread(&self, thread_id: &str) -> usize {
        let targets = self.list(thread_id);
        let checked = targets.len();
        for session in targets { session.close_idle_shell(); }
        checked
    }

    pub(crate) fn create(
        &self,
        thread_id: &str,
        workspace_id: &str,
        root: &str,
        name: &str,
        cols: u16,
        rows: u16,
    ) -> Result<Arc<TerminalSession>, Failure> {
        self.create_inner(thread_id,workspace_id,root,name,cols,rows,false)
    }
    pub(crate) fn create_control(&self,thread_id:&str,workspace_id:&str,root:&str)->Result<Arc<TerminalSession>,Failure>{
        self.create_inner(thread_id,workspace_id,root,"Terminal",100,30,true)
    }
    fn create_inner(&self,thread_id:&str,workspace_id:&str,root:&str,name:&str,cols:u16,rows:u16,control_owned:bool)->Result<Arc<TerminalSession>,Failure>{
        self.reap_idle();
        let running = self
            .sessions
            .lock()
            .unwrap()
            .iter()
            .filter(|session| session.thread_id == thread_id && session.running())
            .count();
        if !control_owned && running >= MAX_SESSIONS_PER_THREAD {
            return Err(TerminalError::new("conversation.terminal.limit", "每个会话最多 6 个终端标签").failure());
        }
        let root = std::fs::canonicalize(shellexpand_home(root))
            .map_err(|error| surface(StatusCode::BAD_REQUEST, &format!("工作区目录不可用：{error}")))?;
        let (terminal, output) = term::Terminal::spawn_with_output(&root)
            .map_err(|error| TerminalError::new("conversation.terminal.error", format!("无法启动终端：{error}")).failure())?;
        terminal.resize(cols as usize, rows as usize);
        let session_id = uuid::Uuid::new_v4().simple().to_string();
        let name = match name.trim() {
            "" => session_id[..8].to_owned(),
            name => name.to_owned(),
        };
        let now = unix_now();
        let (frames, _) = broadcast::channel(SUBSCRIBER_QUEUE);
        let session = Arc::new(TerminalSession {
            control_owned,
            session_id,
            thread_id: thread_id.to_owned(),
            workspace_id: workspace_id.to_owned(),
            root_path: root.to_string_lossy().into_owned(),
            name,
            created_at: now,
            history_limit: env_number("MUTEKI_TERMINAL_HISTORY_BYTES", DEFAULT_HISTORY_BYTES as f64, 1024.0) as usize,
            state: Mutex::new(SessionState {
                status: "running".into(),
                end_reason: None,
                end_code: None,
                cols,
                rows,
                last_active_at: now,
                diverged: false,
                history: VecDeque::new(),
                truncated: false,
                closed: false,
                final_screen: None,
                carry: Vec::new(),
            }),
            terminal: Mutex::new(Some(terminal)),
            frames,
        });
        let reader = session.clone();
        tokio::spawn(async move {
            while let Ok(bytes) = output.recv().await {
                if bytes.is_empty() {
                    break;
                }
                reader.output(&bytes);
            }
            let code = reader
                .terminal
                .lock()
                .unwrap()
                .as_ref()
                .and_then(|terminal| terminal.grid().snapshot().exit_code);
            reader.exited(code);
        });
        self.sessions.lock().unwrap().push(session.clone());
        Ok(session)
    }

    pub fn close_all(&self) {
        let sessions: Vec<_> = self.sessions.lock().unwrap().clone();
        for session in sessions {
            session.close("服务已停止", "closed");
        }
    }
}

fn shellexpand_home(path: &str) -> PathBuf {
    match path.strip_prefix("~/") {
        Some(rest) => std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default().join(rest),
        None => PathBuf::from(path),
    }
}

/// The thread's workspace id and root, or a 404 naming what is missing.
fn thread_workspace(s: &AppState, tid: &str) -> Result<(String, String), Failure> {
    let v = s
        .store
        .get(chat::COLLECTION, tid)
        .ok()
        .flatten()
        .ok_or_else(|| surface(StatusCode::NOT_FOUND, "当前会话不存在"))?;
    let workspace_id = v["thread"]["workspace_id"].as_str().unwrap_or("").to_owned();
    Ok((workspace_id, chat::workspace_root(s, &v)))
}

pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/threads/{id}/workspace/terminal", post(run_command))
        .route("/api/threads/{id}/workspace/terminal/sessions", get(list).post(create))
        .route("/api/threads/{id}/workspace/terminal/sessions/{sid}", delete(close))
        .route("/api/threads/{id}/workspace/terminal/sessions/{sid}/interrupt", post(interrupt))
        .route("/api/threads/{id}/workspace/terminal/sessions/{sid}/stream", get(stream))
}

async fn create(
    State(s): State<Arc<AppState>>,
    Path(tid): Path<String>,
    body: Option<Json<Value>>,
) -> Result<Json<Value>, Failure> {
    let body = body.map(|Json(body)| body).unwrap_or_else(|| json!({}));
    let number = |key: &str, default: i64, min: i64, max: i64| -> Result<u16, Failure> {
        match body.get(key) {
            None | Some(Value::Null) => Ok(default as u16),
            Some(value) => value
                .as_i64()
                .filter(|n| (min..=max).contains(n))
                .map(|n| n as u16)
                .ok_or_else(|| error(StatusCode::UNPROCESSABLE_ENTITY, "conversation.terminal.invalid", "validation", &format!("{key} 必须是 {min} 到 {max} 之间的整数"))),
        }
    };
    let cols = number("cols", 80, 20, 500)?;
    let rows = number("rows", 24, 5, 200)?;
    let name = match body.get("name") {
        None | Some(Value::Null) => "",
        Some(Value::String(name)) if name.chars().count() <= 80 => name.as_str(),
        Some(_) => return Err(error(StatusCode::UNPROCESSABLE_ENTITY, "conversation.terminal.invalid", "validation", "name 最多 80 个字符")),
    };
    let (workspace_id, root) = thread_workspace(&s, &tid)?;
    if root.trim().is_empty() {
        return Err(surface(StatusCode::NOT_FOUND, "当前会话未绑定工作区"));
    }
    let session = s.terminals.create(&tid, &workspace_id, &root, name, cols, rows)?;
    Ok(Json(session.public()))
}

async fn list(State(s): State<Arc<AppState>>, Path(tid): Path<String>) -> Result<Json<Value>, Failure> {
    let (workspace_id, root) = thread_workspace(&s, &tid)?;
    let root = Some(root.as_str()).filter(|root| !root.is_empty());
    let sessions: Vec<Value> = s
        .terminals
        .list(&tid)
        .iter()
        .map(|session| {
            session.ensure_workspace(root, &workspace_id);
            session.public()
        })
        .collect();
    Ok(Json(json!({"sessions":sessions})))
}

fn session(s: &AppState, tid: &str, sid: &str) -> Result<Arc<TerminalSession>, Failure> {
    s.terminals
        .get(tid, sid)
        .ok_or_else(|| surface(StatusCode::NOT_FOUND, "终端会话不存在"))
}

async fn interrupt(State(s): State<Arc<AppState>>, Path((tid, sid)): Path<(String, String)>) -> Result<Json<Value>, Failure> {
    let session = session(&s, &tid, &sid)?;
    let (workspace_id, root) = thread_workspace(&s, &tid)?;
    session.ensure_workspace(Some(root.as_str()).filter(|root| !root.is_empty()), &workspace_id);
    session.interrupt().map_err(|error| error.failure())?;
    Ok(Json(session.public()))
}

async fn close(State(s): State<Arc<AppState>>, Path((tid, sid)): Path<(String, String)>) -> Result<Json<Value>, Failure> {
    let session = session(&s, &tid, &sid)?;
    session.close("用户关闭终端", "closed");
    Ok(Json(session.public()))
}

async fn stream(
    State(s): State<Arc<AppState>>,
    Path((tid, sid)): Path<(String, String)>,
    Extension(access): Extension<Arc<Access>>,
    binding: Option<Extension<SessionBinding>>,
    upgrade: WebSocketUpgrade,
) -> Response {
    let found = s.terminals.get(&tid, &sid);
    if let Some(session) = &found
        && let Ok((workspace_id, root)) = thread_workspace(&s, &tid)
    {
        session.ensure_workspace(Some(root.as_str()).filter(|root| !root.is_empty()), &workspace_id);
    }
    let binding = binding.map(|Extension(binding)| binding);
    upgrade
        .on_upgrade(move |socket| async move {
            match found {
                Some(session) => attach(socket, session, access, binding).await,
                None => {
                    let mut socket = socket;
                    let _ = socket
                        .send(Message::Close(Some(CloseFrame { code: 4404, reason: "Terminal session not found".into() })))
                        .await;
                }
            }
        })
        .into_response()
}

async fn send(socket: &mut WebSocket, frame: &Value) -> bool {
    socket.send(Message::Text(frame.to_string().into())).await.is_ok()
}

async fn attach(mut socket: WebSocket, session: Arc<TerminalSession>, access: Arc<Access>, binding: Option<SessionBinding>) {
    let (hello, history, status, mut frames) = {
        let state = session.state.lock().unwrap();
        let public = session.public_locked(&state);
        let bytes: Vec<u8> = state.history.iter().copied().collect();
        (
            json!({"type":"hello","session":public}),
            json!({"type":"history","data":B64.encode(&bytes),"truncated":state.truncated}),
            json!({"type":"status","session":public}),
            session.frames.subscribe(),
        )
    };
    for frame in [&hello, &history, &status] {
        if !send(&mut socket, frame).await {
            return;
        }
    }
    let mut check = tokio::time::interval(Duration::from_secs(1));
    loop {
        tokio::select! {
            incoming = socket.recv() => {
                let Some(Ok(message)) = incoming else { return };
                let text = match message {
                    Message::Text(text) => text.to_string(),
                    Message::Close(_) => return,
                    _ => continue,
                };
                if let Some(reply) = client_frame(&session, &text)
                    && !send(&mut socket, &reply).await
                {
                    return;
                }
            }
            frame = frames.recv() => match frame {
                Ok(frame) => if !send(&mut socket, &frame).await { return },
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => return,
            },
            _ = check.tick() => {
                if let Some(binding) = &binding
                    && !access.still_valid(&binding.token, &binding.revision)
                {
                    let _ = socket.send(Message::Close(Some(CloseFrame { code: 4401, reason: "Session expired".into() }))).await;
                    return;
                }
            }
        }
    }
}

/// Apply one client frame; returns a status frame to send back on failure.
fn client_frame(session: &TerminalSession, text: &str) -> Option<Value> {
    let message: Value = serde_json::from_str(text).ok()?;
    let failed = |error: TerminalError| {
        let state = session.state.lock().unwrap();
        Some(session.status_frame(&state, Some(&error.message)))
    };
    match message["type"].as_str()? {
        "stdin" => {
            let data = message["data"].as_str()?;
            let bytes = B64.decode(data).unwrap_or_else(|_| data.as_bytes().to_vec());
            session.write(bytes).err().and_then(failed)
        }
        "resize" => {
            let (cols, rows) = {
                let state = session.state.lock().unwrap();
                (state.cols, state.rows)
            };
            let read = |key: &str, current: u16| -> Option<u16> {
                match &message[key] {
                    Value::Null => Some(current),
                    value => value.as_f64().map(|n| n.clamp(0.0, u16::MAX as f64) as u16).or_else(|| value.as_str()?.trim().parse().ok()),
                }
            };
            let (Some(cols), Some(rows)) = (read("cols", cols), read("rows", rows)) else { return None };
            session.resize(if cols == 0 { 80 } else { cols }, if rows == 0 { 24 } else { rows });
            None
        }
        "interrupt" => session.interrupt().err().and_then(failed),
        _ => None,
    }
}

async fn run_command(
    State(s): State<Arc<AppState>>,
    Path(tid): Path<String>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, Failure> {
    let command = body["command"].as_str().unwrap_or("");
    if command.is_empty() || command.chars().count() > 8000 {
        return Err(error(StatusCode::UNPROCESSABLE_ENTITY, "conversation.terminal.invalid", "validation", "command 长度必须在 1 到 8000 个字符之间"));
    }
    let (_, root) = thread_workspace(&s, &tid).map_err(|_| surface(StatusCode::NOT_FOUND, "当前会话未绑定工作区"))?;
    if root.trim().is_empty() {
        return Err(surface(StatusCode::NOT_FOUND, "当前会话未绑定工作区"));
    }
    let root = std::fs::canonicalize(shellexpand_home(&root)).map_err(|error| surface(StatusCode::BAD_REQUEST, &error.to_string()))?;
    let value = command.trim();
    if value.is_empty() {
        return Err(surface(StatusCode::BAD_REQUEST, "请输入命令"));
    }
    let shell = std::env::var("SHELL").ok().filter(|shell| !shell.is_empty()).unwrap_or_else(|| "/bin/zsh".into());
    let child = crate::host_process::command(shell)
        .args(["-lc", value])
        .current_dir(&root)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|error| surface(StatusCode::BAD_REQUEST, &error.to_string()))?;
    let output = tokio::time::timeout(COMMAND_TIMEOUT, child.wait_with_output())
        .await
        .map_err(|_| surface(StatusCode::BAD_REQUEST, "命令执行超过 30 秒"))?
        .map_err(|error| surface(StatusCode::BAD_REQUEST, &error.to_string()))?;
    let mut combined = output.stdout;
    combined.extend(output.stderr);
    let truncated = combined.len() > MAX_COMMAND_OUTPUT;
    combined.truncate(MAX_COMMAND_OUTPUT);
    Ok(Json(json!({
        "command": value, "cwd": root.to_string_lossy(), "exit_code": output.status.code(),
        "output": String::from_utf8_lossy(&combined), "truncated": truncated,
    })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_split_codepoint_waits_for_its_remaining_bytes() {
        let text = "终".as_bytes();
        assert_eq!(incomplete_utf8_tail(b"abc"), 0);
        assert_eq!(incomplete_utf8_tail(&text[..1]), 1);
        assert_eq!(incomplete_utf8_tail(&text[..2]), 2);
        assert_eq!(incomplete_utf8_tail(text), 0);
        assert_eq!(incomplete_utf8_tail(&[b'a', 0xF0, 0x9F, 0x98]), 3);
    }

    #[tokio::test]
    async fn a_shell_echoes_input_and_reports_its_exit() {
        let dir = std::env::temp_dir().join(format!("muteki-term-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        let terminals = Terminals::default();
        let session = terminals
            .create("thread", "ws", dir.to_str().unwrap(), "", 80, 24)
            .unwrap();
        assert_eq!(session.public()["name"].as_str().unwrap().len(), 8);
        let mut frames = session.frames.subscribe();
        session.write(b"echo muteki-$((40+2))\n".to_vec()).unwrap();
        let mut seen = String::new();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
        while !seen.contains("muteki-42") {
            let frame = tokio::time::timeout_at(deadline, frames.recv()).await.expect("shell output").unwrap();
            if frame["type"] == "stdout" {
                seen.push_str(&String::from_utf8_lossy(&B64.decode(frame["data"].as_str().unwrap()).unwrap()));
            }
        }
        session.write(b"exit 3\n".to_vec()).unwrap();
        loop {
            let frame = tokio::time::timeout_at(deadline, frames.recv()).await.expect("exit status").unwrap();
            if frame["type"] == "status" {
                assert_eq!(frame["session"]["status"], "exited");
                assert_eq!(frame["session"]["end_code"], "exited");
                break;
            }
        }
        assert_eq!(session.write(b"x".to_vec()).unwrap_err().code, "conversation.terminal.exited");
        session.ensure_workspace(Some("/nonexistent"), "ws");
        assert_eq!(session.public()["workspace_diverged"], true);
        assert_eq!(terminals.list("thread").len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
