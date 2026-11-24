//! A minimal MCP client for package servers: stdio (inside the platform
//! sandbox), Streamable HTTP, or legacy HTTP/SSE. The host owns these connections; engines only
//! ever see the Muteki gateway, so package env values and headers never reach
//! an engine's configuration.

use super::inspect::{PluginError, Result, which};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::{Child, ChildStdin, Command},
    sync::{Mutex as AsyncMutex, oneshot},
};

pub const READY_TIMEOUT: Duration = Duration::from_secs(25);
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(90);
const PROTOCOL_VERSION: &str = "2025-06-18";

type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<std::result::Result<Value, Value>>>>>;

struct PendingGuard<'a> {
    pending: &'a Pending,
    id: u64,
    armed: bool,
}

impl<'a> PendingGuard<'a> {
    fn new(pending: &'a Pending, id: u64) -> Self {
        Self { pending, id, armed: true }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for PendingGuard<'_> {
    fn drop(&mut self) {
        if self.armed {
            self.pending.lock().unwrap().remove(&self.id);
        }
    }
}

struct Stdio {
    stdin: Arc<AsyncMutex<ChildStdin>>,
    pending: Pending,
    _child: AsyncMutex<Child>,
}

struct Http {
    client: reqwest::Client,
    url: String,
    headers: Vec<(String, String)>,
    session: Mutex<Option<String>>,
    protocol: Mutex<Option<String>>,
}

struct Sse {
    client: reqwest::Client,
    post_url: String,
    headers: Vec<(String, String)>,
    pending: Pending,
    reader_task: tokio::task::JoinHandle<()>,
}

impl Drop for Sse {
    fn drop(&mut self) {
        self.reader_task.abort();
        self.pending.lock().unwrap().clear();
    }
}

enum Transport {
    Stdio(Stdio),
    Http(Http),
    Sse(Sse),
}

fn append_utf8_chunk(raw: &mut Vec<u8>, text: &mut String, chunk: &[u8]) {
    raw.extend_from_slice(chunk);
    loop {
        match std::str::from_utf8(raw) {
            Ok(valid) => {
                text.push_str(&valid.replace("\r\n", "\n"));
                raw.clear();
                break;
            }
            Err(err) => {
                let valid_len = err.valid_up_to();
                if valid_len > 0 {
                    let prefix = std::str::from_utf8(&raw[..valid_len]).unwrap_or("");
                    text.push_str(&prefix.replace("\r\n", "\n"));
                    raw.drain(..valid_len);
                }
                if let Some(error_len) = err.error_len() {
                    text.push(std::char::REPLACEMENT_CHARACTER);
                    raw.drain(..error_len);
                } else {
                    break;
                }
            }
        }
    }
    if text.contains("\r\n") { *text = text.replace("\r\n", "\n"); }
}

fn parse_sse_frame(frame: &str) -> (String, String) {
    let mut event_name = String::from("message");
    let mut data_lines = Vec::new();
    for line in frame.lines() {
        if let Some(rest) = line.strip_prefix("event:") {
            let trimmed = rest.trim();
            if !trimmed.is_empty() {
                event_name = trimmed.to_owned();
            }
        } else if let Some(rest) = line.strip_prefix("data:") {
            data_lines.push(rest.strip_prefix(' ').unwrap_or(rest));
        }
    }
    (event_name, data_lines.join("\n"))
}

pub struct Client {
    transport: Transport,
    next: AtomicU64,
    alive: Arc<AtomicBool>,
    pub capabilities: Value,
}

fn failed(code: &str) -> PluginError {
    PluginError::new(code, "MCP 服务启动失败")
}

fn disconnected() -> PluginError {
    PluginError::new("worker_mcp.disconnected", "MCP 服务已停止")
}

fn render(value: &str, package: &Path, state: &Path) -> String {
    let package = package.to_string_lossy();
    value
        .replace("${PLUGIN_ROOT}", &package)
        .replace("${PLUGIN_DATA}", &state.to_string_lossy())
        .replace("${CLAUDE_PLUGIN_ROOT}", &package)
        .replace("${CODEX_PLUGIN_ROOT}", &package)
}

async fn write_line(stdin: &AsyncMutex<ChildStdin>, message: &Value) -> std::io::Result<()> {
    let mut line = serde_json::to_vec(message).unwrap_or_default();
    line.push(b'\n');
    let mut stdin = stdin.lock().await;
    stdin.write_all(&line).await?;
    stdin.flush().await
}

impl Client {
    /// Connect and complete the MCP handshake within [`READY_TIMEOUT`].
    pub async fn connect(config: &Value, package: &Path, state: &Path) -> Result<Self> {
        match tokio::time::timeout(READY_TIMEOUT, Self::open(config, package, state, false)).await {
            Ok(result) => result,
            Err(_) => Err(PluginError::new("worker_mcp.startup_timeout", "MCP 启动超时")),
        }
    }

    /// Connect to the installed macOS computer-control service with its own
    /// application permissions and environment, outside package isolation.
    pub async fn connect_host(config: &Value, package: &Path, state: &Path, startup: Duration) -> Result<Self> {
        match tokio::time::timeout(startup, Self::open(config, package, state, true)).await {
            Ok(result) => result,
            Err(_) => Err(PluginError::new("chat.computer.startup_timeout", "电脑操控服务启动超时")),
        }
    }

    async fn open(config: &Value, package: &Path, state: &Path, host: bool) -> Result<Self> {
        std::fs::create_dir_all(state).map_err(|_| failed("worker_mcp.state_unavailable"))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(state, std::fs::Permissions::from_mode(0o700))
                .map_err(|_| failed("worker_mcp.state_unavailable"))?;
        }
        let alive = Arc::new(AtomicBool::new(true));
        let transport = if config["command"].as_str().is_some_and(|c| !c.is_empty()) {
            Transport::Stdio(spawn(config, package, state, alive.clone(), host).await?)
        } else {
            let kind = config["type"].as_str().or(config["transport"].as_str()).unwrap_or("").to_ascii_lowercase();
            let headers: Vec<(String, String)> = config["headers"]
                .as_object()
                .into_iter()
                .flatten()
                .filter_map(|(k, v)| Some((k.clone(), render(v.as_str()?, package, state))))
                .collect();
            let url = config["url"].as_str().unwrap_or("").to_owned();
            if kind == "sse" {
                Transport::Sse(Sse::connect(&url, headers, alive.clone()).await?)
            } else {
                Transport::Http(Http {
                    client: crate::network_proxy::http_client_builder()
                        .connect_timeout(Duration::from_secs(10))
                        .build()
                        .map_err(|_| failed("worker_mcp.launch_failed"))?,
                    url,
                    headers,
                    session: Mutex::new(None),
                    protocol: Mutex::new(None),
                })
            }
        };
        let mut client = Client { transport, next: AtomicU64::new(1), alive, capabilities: json!({}) };
        let initialized = client
            .call(
                "initialize",
                json!({
                    "protocolVersion": PROTOCOL_VERSION,
                    "capabilities": if host { json!({"elicitation": {"form": {}}}) } else { json!({}) },
                    "clientInfo": {"name": "muteki", "version": env!("CARGO_PKG_VERSION")},
                }),
            )
            .await
            .map_err(|error| match error.code.as_str() {
                "worker_mcp.disconnected" | "worker_mcp.request_failed" => failed("worker_mcp.protocol_failed"),
                _ => error,
            })?;
        if let Transport::Http(http) = &client.transport {
            *http.protocol.lock().unwrap() =
                Some(initialized["protocolVersion"].as_str().unwrap_or(PROTOCOL_VERSION).to_owned());
        }
        client.capabilities = initialized["capabilities"].clone();
        client.notify("notifications/initialized").await;
        Ok(client)
    }

    pub fn is_alive(&self) -> bool {
        self.alive.load(Ordering::SeqCst)
    }

    pub async fn stop(&self) {
        self.alive.store(false, Ordering::SeqCst);
        match &self.transport {
            Transport::Stdio(stdio) => {
                stdio.pending.lock().unwrap().clear();
                let _ = stdio._child.lock().await.kill().await;
            }
            Transport::Sse(sse) => {
                sse.reader_task.abort();
                sse.pending.lock().unwrap().clear();
            }
            Transport::Http(_) => {}
        }
    }

    pub fn supports(&self, capability: &str) -> bool {
        self.capabilities.get(capability).is_some_and(|v| !v.is_null())
    }

    /// One request, bounded by [`REQUEST_TIMEOUT`]. Server error text is not
    /// returned: it may echo headers or environment values.
    pub async fn request(&self, method: &str, params: Value) -> Result<Value> {
        self.request_with_timeout(method, params, REQUEST_TIMEOUT).await
    }

    pub async fn request_with_timeout(&self, method: &str, params: Value, timeout: Duration) -> Result<Value> {
        match tokio::time::timeout(timeout, self.call(method, params)).await {
            Ok(result) => result,
            Err(_) => Err(PluginError::new("worker_mcp.request_timeout", "MCP 工具请求超时")),
        }
    }

    async fn call(&self, method: &str, params: Value) -> Result<Value> {
        if !self.is_alive() {
            return Err(disconnected());
        }
        let id = self.next.fetch_add(1, Ordering::SeqCst);
        let message = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        let outcome = match &self.transport {
            Transport::Stdio(stdio) => {
                let (sender, receiver) = oneshot::channel();
                stdio.pending.lock().unwrap().insert(id, sender);
                let mut guard = PendingGuard::new(&stdio.pending, id);
                if write_line(&stdio.stdin, &message).await.is_err() {
                    self.alive.store(false, Ordering::SeqCst);
                    return Err(disconnected());
                }
                let res = receiver.await.map_err(|_| disconnected())?;
                guard.disarm();
                res
            }
            Transport::Http(http) => http.post(&message, id).await?,
            Transport::Sse(sse) => {
                let (sender, receiver) = oneshot::channel();
                sse.pending.lock().unwrap().insert(id, sender);
                let mut guard = PendingGuard::new(&sse.pending, id);
                if let Some(direct) = sse.post(&message, id).await? {
                    sse.pending.lock().unwrap().remove(&id);
                    guard.disarm();
                    direct
                } else {
                    let res = receiver.await.map_err(|_| disconnected())?;
                    guard.disarm();
                    res
                }
            }
        };
        outcome.map_err(|error| {
            tracing::warn!(method, code = %error["code"], "MCP request failed");
            PluginError::new("worker_mcp.request_failed", "MCP 工具请求失败")
        })
    }

    async fn notify(&self, method: &str) {
        let message = json!({"jsonrpc": "2.0", "method": method});
        match &self.transport {
            Transport::Stdio(stdio) => {
                let _ = write_line(&stdio.stdin, &message).await;
            }
            Transport::Http(http) => {
                let _ = http.send(&message).await;
            }
            Transport::Sse(sse) => {
                let _ = sse.send(&message).await;
            }
        }
    }
}

impl Sse {
    async fn connect(url: &str, headers: Vec<(String, String)>, alive: Arc<AtomicBool>) -> Result<Self> {
        let refused = || failed("worker_mcp.launch_failed");
        let base_url = reqwest::Url::parse(url).map_err(|_| refused())?;
        let client = crate::network_proxy::http_client_builder()
            .connect_timeout(Duration::from_secs(10))
            .build()
            .map_err(|_| refused())?;
        let mut request = client.get(url).header("accept", "text/event-stream");
        for (key, value) in &headers {
            request = request.header(key.as_str(), value.as_str());
        }
        let response = request.send().await.map_err(|_| refused())?;
        if !response.status().is_success() {
            return Err(refused());
        }
        let pending: Pending = Arc::default();
        let (endpoint_tx, endpoint_rx) = oneshot::channel::<String>();
        let reader_pending = pending.clone();
        let reader_alive = alive.clone();
        let reader_task = tokio::spawn(async move {
            use futures_util::StreamExt;
            let mut stream = response.bytes_stream();
            let mut raw_buf = Vec::new();
            let mut text_buf = String::new();
            let mut endpoint_tx = Some(endpoint_tx);
            while let Some(Ok(chunk)) = stream.next().await {
                append_utf8_chunk(&mut raw_buf, &mut text_buf, &chunk);
                while let Some(end) = text_buf.find("\n\n") {
                    let frame: String = text_buf.drain(..end + 2).collect();
                    let (event, data) = parse_sse_frame(&frame);
                    if event == "endpoint" {
                        let trimmed = data.trim();
                        if !trimmed.is_empty()
                            && let Some(tx) = endpoint_tx.take()
                        {
                            let resolved = base_url
                                .join(trimmed)
                                .map(|u| u.to_string())
                                .unwrap_or_else(|_| trimmed.to_owned());
                            let _ = tx.send(resolved);
                        }
                    } else if let Ok(message) = serde_json::from_str::<Value>(&data)
                        && let Some(id) = message["id"].as_u64()
                        && let Some(outcome) = reply(&message, id)
                        && let Some(sender) = reader_pending.lock().unwrap().remove(&id)
                    {
                        let _ = sender.send(outcome);
                    }
                }
            }
            reader_alive.store(false, Ordering::SeqCst);
            reader_pending.lock().unwrap().clear();
        });
        let mut connection = Self {
            client,
            post_url: String::new(),
            headers,
            pending,
            reader_task,
        };
        connection.post_url = endpoint_rx.await.map_err(|_| failed("worker_mcp.protocol_failed"))?;
        Ok(connection)
    }

    async fn send(&self, message: &Value) -> std::result::Result<reqwest::Response, reqwest::Error> {
        let mut request = self
            .client
            .post(&self.post_url)
            .header("content-type", "application/json")
            .header("accept", "application/json, text/event-stream")
            .json(message);
        for (key, value) in &self.headers {
            request = request.header(key.as_str(), value.as_str());
        }
        request.send().await
    }

    async fn post(&self, message: &Value, id: u64) -> Result<Option<std::result::Result<Value, Value>>> {
        let refused = || PluginError::new("worker_mcp.launch_failed", "MCP 服务连接失败");
        let response = self.send(message).await.map_err(|_| refused())?;
        if !response.status().is_success() {
            return Err(refused());
        }
        if let Ok(bytes) = response.bytes().await
            && !bytes.is_empty()
            && let Ok(body) = serde_json::from_slice::<Value>(&bytes)
            && let Some(found) = reply(&body, id)
        {
            return Ok(Some(found));
        }
        Ok(None)
    }
}

impl Http {
    async fn send(&self, message: &Value) -> std::result::Result<reqwest::Response, reqwest::Error> {
        let mut request = self
            .client
            .post(&self.url)
            .header("content-type", "application/json")
            .header("accept", "application/json, text/event-stream")
            .json(message);
        for (key, value) in &self.headers {
            request = request.header(key.as_str(), value.as_str());
        }
        if let Some(session) = self.session.lock().unwrap().clone() {
            request = request.header("mcp-session-id", session);
        }
        if let Some(protocol) = self.protocol.lock().unwrap().clone() {
            request = request.header("mcp-protocol-version", protocol);
        }
        request.send().await
    }

    async fn post(&self, message: &Value, id: u64) -> Result<std::result::Result<Value, Value>> {
        let refused = || PluginError::new("worker_mcp.launch_failed", "MCP 服务连接失败");
        let response = self.send(message).await.map_err(|_| refused())?;
        if let Some(session) = response.headers().get("mcp-session-id").and_then(|v| v.to_str().ok()) {
            *self.session.lock().unwrap() = Some(session.to_owned());
        }
        if !response.status().is_success() {
            tracing::warn!(status = %response.status(), "MCP HTTP request was refused");
            return Err(refused());
        }
        let event_stream = response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.starts_with("text/event-stream"));
        if !event_stream {
            let body: Value = response.json().await.map_err(|_| refused())?;
            return reply(&body, id).ok_or_else(refused);
        }
        use futures_util::StreamExt;
        let mut stream = response.bytes_stream();
        let mut raw_buf = Vec::new();
        let mut buffer = String::new();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|_| refused())?;
            append_utf8_chunk(&mut raw_buf, &mut buffer, &chunk);
            while let Some(end) = buffer.find("\n\n") {
                let event: String = buffer.drain(..end + 2).collect();
                let (_, data) = parse_sse_frame(&event);
                if let Ok(value) = serde_json::from_str::<Value>(&data)
                    && let Some(found) = reply(&value, id)
                {
                    return Ok(found);
                }
            }
        }
        Err(refused())
    }
}

fn reply(message: &Value, id: u64) -> Option<std::result::Result<Value, Value>> {
    if message["id"].as_u64() != Some(id) || message.get("method").is_some() {
        return None;
    }
    Some(match message.get("error") {
        Some(error) if !error.is_null() => Err(error.clone()),
        _ => Ok(message.get("result").cloned().unwrap_or(Value::Null)),
    })
}

async fn spawn(config: &Value, package: &Path, state: &Path, alive: Arc<AtomicBool>, host: bool) -> Result<Stdio> {
    let mut env: Vec<(String, String)> = [
        "PATH",
        "__MISE_DIFF",
        "LANG",
        "LC_ALL",
        "LC_CTYPE",
        "SSL_CERT_FILE",
        "SSL_CERT_DIR",
        "NODE_EXTRA_CA_CERTS",
        "SYSTEMROOT",
        "HOME",
        "TMPDIR",
        "USER",
    ]
    .iter()
    .filter_map(|key| crate::shell_env::var(key).map(|value| ((*key).to_owned(), value)))
    .collect();
    for (key, value) in config["env"].as_object().into_iter().flatten() {
        env.retain(|(k, _)| k != key);
        env.push((key.clone(), render(value.as_str().unwrap_or(""), package, state)));
    }
    for (key, value) in [
        ("HOME", state.to_path_buf()),
        ("TMPDIR", state.to_path_buf()),
        ("XDG_CONFIG_HOME", state.join("config")),
        ("XDG_CACHE_HOME", state.join("cache")),
    ].into_iter().filter(|_| !host) {
        env.retain(|(k, _)| k != key);
        env.push((key.to_owned(), value.to_string_lossy().into_owned()));
    }
    let mut argv = vec![render(config["command"].as_str().unwrap_or(""), package, state)];
    argv.extend(
        config["args"].as_array().into_iter().flatten().filter_map(Value::as_str).map(|a| render(a, package, state)),
    );
    if host && !argv[0].is_empty() && !argv[0].contains('/') && !argv[0].contains('\\') {
        let env_path = env
            .iter()
            .rev()
            .find(|(key, _)| key == "PATH")
            .map(|(_, value)| std::ffi::OsStr::new(value.as_str()));
        let resolved = crate::shell_env::resolve_program_in_path(
            std::ffi::OsStr::new(&argv[0]),
            env_path,
        );
        argv[0] = resolved.to_string_lossy().into_owned();
    }
    let network = config["network"].as_array().is_some_and(|n| !n.is_empty());
    let command = if host { argv } else { isolated(&argv, package, state, &env, network).await? };
    let log = state.join("mcp-stderr.log");
    let stderr = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log)
        .map_err(|_| failed("worker_mcp.state_unavailable"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&log, std::fs::Permissions::from_mode(0o600));
    }
    // Intentionally clears the environment before applying the scoped MCP env.
    #[allow(clippy::disallowed_methods)]
    let mut child = Command::new(&command[0])
        .args(&command[1..])
        .env_clear()
        .envs(env)
        .current_dir(state)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(stderr)
        .kill_on_drop(true)
        .spawn()
        .map_err(|_| failed("worker_mcp.launch_failed"))?;
    let stdin = Arc::new(AsyncMutex::new(child.stdin.take().ok_or_else(|| failed("worker_mcp.launch_failed"))?));
    let stdout = child.stdout.take().ok_or_else(|| failed("worker_mcp.launch_failed"))?;
    let pending: Pending = Arc::default();
    {
        let pending = pending.clone();
        let stdin = stdin.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let Ok(message) = serde_json::from_str::<Value>(&line) else { continue };
                if let Some(method) = message["method"].as_str() {
                    if message.get("id").is_some_and(|id| !id.is_null()) {
                        let response = match method {
                            "ping" => json!({"jsonrpc":"2.0","id":message["id"],"result":{}}),
                            "roots/list" => json!({"jsonrpc":"2.0","id":message["id"],"result":{"roots":[]}}),
                            "elicitation/create" if host => {
                                let params = &message["params"];
                                if params["mode"] == "url" {
                                    json!({"jsonrpc":"2.0","id":message["id"],"result":{"action":"decline"}})
                                } else {
                                    let defaults: serde_json::Map<String, Value> = params["requestedSchema"]["properties"]
                                        .as_object().into_iter().flatten()
                                        .filter_map(|(name, spec)| spec.get("default").map(|value| (name.clone(), value.clone())))
                                        .collect();
                                    json!({"jsonrpc":"2.0","id":message["id"],"result":{"action":"accept","content":defaults,"_meta":{"persist":"session"}}})
                                }
                            }
                            _ => json!({"jsonrpc":"2.0","id":message["id"],"error":{"code":-32601,"message":"method not supported by Muteki"}}),
                        };
                        let _ = write_line(&stdin, &response).await;
                    }
                    continue;
                }
                let Some(id) = message["id"].as_u64() else { continue };
                if let Some(sender) = pending.lock().unwrap().remove(&id) {
                    let outcome = match message.get("error") {
                        Some(error) if !error.is_null() => Err(error.clone()),
                        _ => Ok(message.get("result").cloned().unwrap_or(Value::Null)),
                    };
                    let _ = sender.send(outcome);
                }
            }
            alive.store(false, Ordering::SeqCst);
            pending.lock().unwrap().clear();
        });
    }
    Ok(Stdio { stdin, pending, _child: AsyncMutex::new(child) })
}

fn find_in(name: &str, path_var: &str) -> Option<PathBuf> {
    if name.contains('/') {
        return which(name);
    }
    std::env::split_paths(path_var)
        .map(|dir| dir.join(name))
        .find(|candidate| which(&candidate.to_string_lossy()).is_some())
}

/// Wrap a stdio server's argv in the platform's filesystem sandbox: it may
/// read its package and system runtimes and write only its private state.
async fn isolated(argv: &[String], package: &Path, state: &Path, env: &[(String, String)], network: bool) -> Result<Vec<String>> {
    let path_var = env.iter().find(|(k, _)| k == "PATH").map(|(_, v)| v.clone()).unwrap_or_default();
    let Some(executable) = find_in(&argv[0], &path_var) else {
        return Err(PluginError::new("worker_mcp.isolation_unavailable", "MCP 启动程序不存在"));
    };
    platform_sandbox(argv, &executable, package, state, env, network).await
}

#[cfg(target_os = "linux")]
async fn platform_sandbox(
    argv: &[String],
    executable: &Path,
    package: &Path,
    state: &Path,
    _env: &[(String, String)],
    network: bool,
) -> Result<Vec<String>> {
    let Some(bwrap) = which("bwrap") else {
        return Err(PluginError::new(
            "worker_mcp.isolation_unavailable",
            "当前系统没有 stdio MCP 隔离器；可使用 HTTP MCP，Linux 可配置 bubblewrap",
        ));
    };
    let unavailable = || PluginError::new("worker_mcp.isolation_unavailable", "MCP executable is outside its package, private state, and supported runtimes");
    let package = std::fs::canonicalize(package).map_err(|_| unavailable())?;
    let state = std::fs::canonicalize(state).map_err(|_| unavailable())?;
    let resolved = std::fs::canonicalize(executable).map_err(|_| unavailable())?;
    let system = ["/usr", "/bin", "/sbin", "/lib", "/lib64"];
    let mut readable: Vec<PathBuf> = system.iter().map(PathBuf::from).collect();
    readable.push(package.clone());
    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
        for manager in [".nvm/versions/node", ".pyenv/versions", ".local/share/uv/python"] {
            if let Ok(base) = std::fs::canonicalize(home.join(manager))
                && let Ok(relative) = resolved.strip_prefix(&base)
                && let Some(std::path::Component::Normal(version)) = relative.components().next()
            {
                readable.push(base.join(version));
            }
        }
    }
    if !resolved.starts_with(&state) && !readable.iter().any(|root| resolved.starts_with(root)) {
        return Err(unavailable());
    }
    if let Some(venv) = executable.parent().filter(|p| p.file_name().is_some_and(|n| n == "bin"))
        .and_then(Path::parent).filter(|p| p.join("pyvenv.cfg").is_file())
    {
        let venv = std::fs::canonicalize(venv).map_err(|_| unavailable())?;
        for part in ["pyvenv.cfg", "bin", "lib", "lib64", "share"] {
            let path = venv.join(part);
            if path.exists() {
                if !std::fs::canonicalize(&path).map_err(|_| unavailable())?.starts_with(&venv) {
                    return Err(unavailable());
                }
                readable.push(path);
            }
        }
    }
    for path in ["/etc/ld.so.cache", "/etc/ld.so.conf", "/etc/ld.so.conf.d", "/etc/resolv.conf", "/etc/hosts", "/etc/nsswitch.conf", "/etc/passwd", "/etc/group", "/etc/ssl/certs", "/etc/pki/tls/certs"] {
        readable.push(PathBuf::from(path));
    }
    let mut command: Vec<String> = vec![bwrap.to_string_lossy().into_owned(), "--die-with-parent".into(), "--new-session".into(), "--unshare-all".into()];
    if network {
        command.push("--share-net".into());
    }
    command.extend(["--tmpfs".into(), "/tmp".into()]);
    let entry = std::path::absolute(executable).map_err(|_| unavailable())?;
    let entry_mounted = entry.starts_with(&state) || readable.iter().any(|root| entry.starts_with(root));
    for root in readable {
        let root = root.to_string_lossy();
        command.extend(["--ro-bind-try".into(), root.to_string(), root.to_string()]);
    }
    if !entry_mounted {
        command.extend(["--ro-bind".into(), resolved.to_string_lossy().into_owned(), entry.to_string_lossy().into_owned()]);
    }
    let state = state.to_string_lossy();
    for part in ["--bind", &state, &state, "--proc", "/proc", "--dev", "/dev", "--chdir", &state, "--"] {
        command.push(part.to_owned());
    }
    command.push(entry.to_string_lossy().into_owned());
    command.extend(argv[1..].iter().cloned());
    Ok(command)
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
async fn platform_sandbox(
    _argv: &[String],
    _executable: &Path,
    _package: &Path,
    _state: &Path,
    _env: &[(String, String)],
    _network: bool,
) -> Result<Vec<String>> {
    Err(PluginError::new("worker_mcp.isolation_unavailable", "当前系统没有 stdio MCP 隔离器；可使用 HTTP MCP"))
}

#[cfg(target_os = "macos")]
async fn platform_sandbox(
    argv: &[String],
    entry: &Path,
    package: &Path,
    state: &Path,
    env: &[(String, String)],
    network: bool,
) -> Result<Vec<String>> {
    let unavailable = |message: &str| PluginError::new("worker_mcp.isolation_unavailable", message.to_owned());
    let sandbox = PathBuf::from("/usr/bin/sandbox-exec");
    if !sandbox.is_file() {
        return Err(unavailable("sandbox-exec is unavailable"));
    }
    let entry = std::path::absolute(entry).unwrap_or_else(|_| entry.to_path_buf());
    let venv_root = entry
        .parent()
        .filter(|bin| bin.file_name().is_some_and(|n| n == "bin" || n == "Scripts"))
        .and_then(Path::parent)
        .filter(|root| root.join("pyvenv.cfg").is_file())
        .map(Path::to_path_buf);
    let mut resolved = if venv_root.is_some() { entry.clone() } else { std::fs::canonicalize(&entry).unwrap_or(entry.clone()) };
    if venv_root.is_none() && argv[0].rsplit('/').next().is_some_and(|n| n.starts_with("python")) {
        #[allow(clippy::disallowed_methods)]
        let probe = tokio::time::timeout(
            Duration::from_secs(5),
            Command::new(&entry)
                .args(["-c", "import sys;print(sys.executable)"])
                .env_clear()
                .envs(env.iter().cloned())
                .output(),
        )
        .await;
        if let Ok(Ok(output)) = probe
            && output.status.success()
            && let Some(line) = String::from_utf8_lossy(&output.stdout).lines().next().map(str::trim)
            && !line.is_empty()
        {
            resolved = std::fs::canonicalize(line).unwrap_or_else(|_| PathBuf::from(line));
        }
    }
    let executable = std::fs::canonicalize(&resolved).unwrap_or(resolved.clone());
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
    let trusted: Vec<PathBuf> = ["/usr", "/bin", "/sbin", "/System", "/Library", "/opt/homebrew", "/usr/local"]
        .iter()
        .map(PathBuf::from)
        .chain(std::iter::once(home.join(".pyenv/versions")))
        .collect();
    let mut runtime_root = executable.parent().and_then(Path::parent).map(Path::to_path_buf).unwrap_or_default();
    if let Some(root) = trusted.iter().find(|root| executable.starts_with(root))
        && !runtime_root.starts_with(root)
    {
        runtime_root = root.clone();
    }
    let package = std::fs::canonicalize(package).unwrap_or(package.to_path_buf());
    let state = std::fs::canonicalize(state).unwrap_or(state.to_path_buf());
    let runtime_allowed = runtime_root.starts_with(&package)
        || runtime_root.starts_with(&state)
        || trusted.iter().any(|root| runtime_root.starts_with(root));
    if !runtime_allowed && venv_root.is_none() {
        return Err(unavailable("MCP executable is outside its package, private state, and supported runtimes"));
    }
    let mut read_roots: std::collections::BTreeSet<String> = [
        package.to_string_lossy().into_owned(),
        state.to_string_lossy().into_owned(),
        "/System".into(),
        "/usr".into(),
        "/bin".into(),
        "/sbin".into(),
        "/Library".into(),
        "/private/var/db/timezone".into(),
        "/dev".into(),
    ]
    .into_iter()
    .collect();
    for ancestor in executable.ancestors() {
        if ancestor.file_name().is_some_and(|n| n == "Cellar" || n == "opt")
            && let Some(prefix) = ancestor.parent().filter(|p| p.is_dir())
        {
            read_roots.insert(prefix.to_string_lossy().into_owned());
            break;
        }
    }
    for candidate in [
        "/opt/homebrew/opt/openssl@3",
        "/opt/homebrew/opt/openssl",
        "/usr/local/opt/openssl@3",
        "/usr/local/opt/openssl",
        "/private/etc/ssl",
        "/etc/ssl",
    ] {
        if let Ok(path) = std::fs::canonicalize(candidate) {
            read_roots.insert(path.to_string_lossy().into_owned());
        }
    }
    if runtime_allowed {
        read_roots.insert(runtime_root.to_string_lossy().into_owned());
    }
    if let Some(venv) = &venv_root {
        let venv = std::fs::canonicalize(venv).unwrap_or(venv.clone());
        for relative in ["pyvenv.cfg", "bin", "lib", "lib64", "Lib", "share"] {
            let candidate = venv.join(relative);
            if candidate.exists() {
                let resolved = std::fs::canonicalize(&candidate).unwrap_or(candidate);
                if !resolved.starts_with(&venv) {
                    return Err(unavailable("virtualenv dependency path leaves its environment"));
                }
                read_roots.insert(resolved.to_string_lossy().into_owned());
            }
        }
    }
    let quote = |value: &str| serde_json::to_string(value).unwrap_or_default();
    let mut lines: Vec<String> = vec![
        "(version 1)".into(),
        "(deny default)".into(),
        "(import \"system.sb\")".into(),
        "(allow process-info*)".into(),
        "(allow process-fork)".into(),
        "(allow sysctl-read)".into(),
        "(allow mach-lookup)".into(),
        "(allow file-read-metadata)".into(),
    ];
    if runtime_allowed {
        lines.push(format!("(allow process-exec (subpath {}))", quote(&runtime_root.to_string_lossy())));
    }
    if let Some(venv) = &venv_root {
        lines.push(format!("(allow process-exec (subpath {}))", quote(&venv.join("bin").to_string_lossy())));
    }
    for root in &read_roots {
        lines.push(format!("(allow file-read* (subpath {}))", quote(root)));
    }
    lines.push(format!("(allow file-write* (subpath {}))", quote(&state.to_string_lossy())));
    lines.push("(allow file-write* (literal \"/dev/null\"))".into());
    lines.push("(allow file-write* (literal \"/dev/tty\"))".into());
    if network {
        lines.push("(allow network-outbound (literal \"/private/var/run/mDNSResponder\"))".into());
        lines.push("(allow network-outbound (remote ip \"*:*\"))".into());
    }
    let package_text = package.to_string_lossy().into_owned();
    let state_text = state.to_string_lossy().into_owned();
    for root in ["/opt/homebrew", "/usr/local", "/bin", "/usr", &package_text, &state_text] {
        lines.push(format!("(allow file-read* (subpath {}))", quote(root)));
        lines.push(format!("(allow process-exec (subpath {}))", quote(root)));
    }
    let profile = state.join(".extension-sandbox.sb");
    std::fs::write(&profile, lines.join("\n") + "\n").map_err(|_| failed("worker_mcp.isolation_failed"))?;
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&profile, std::fs::Permissions::from_mode(0o600));
    }
    let mut command = vec![
        sandbox.to_string_lossy().into_owned(),
        "-f".into(),
        profile.to_string_lossy().into_owned(),
        resolved.to_string_lossy().into_owned(),
    ];
    command.extend(argv[1..].iter().cloned());
    Ok(command)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SERVER: &str = r#"
import json, sys
for line in sys.stdin:
    msg = json.loads(line)
    if "id" not in msg:
        continue
    if msg["method"] == "initialize":
        result = {"protocolVersion": "2025-06-18", "capabilities": {"tools": {}}, "serverInfo": {"name": "t", "version": "1"}}
    elif msg["method"] == "tools/list":
        result = {"tools": [{"name": "echo", "description": "Echo", "inputSchema": {"type": "object"}}]}
    elif msg["method"] == "tools/call":
        result = {"content": [{"type": "text", "text": msg["params"]["arguments"]["text"]}]}
    else:
        print(json.dumps({"jsonrpc": "2.0", "id": msg["id"], "error": {"code": -32601, "message": "no"}}), flush=True)
        continue
    print(json.dumps({"jsonrpc": "2.0", "id": msg["id"], "result": result}), flush=True)
"#;

    #[tokio::test]
    async fn a_sandboxed_stdio_server_answers_list_and_call() {
        if which("python3").is_none() || !cfg!(target_os = "macos") {
            return;
        }
        let package = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        std::fs::write(package.path().join("server.py"), SERVER).unwrap();
        let config = json!({"command": "python3", "args": ["${PLUGIN_ROOT}/server.py"]});
        let client = Client::connect(&config, package.path(), &state.path().join("s")).await.unwrap();
        assert!(client.supports("tools"));
        let tools = client.request("tools/list", json!({})).await.unwrap();
        assert_eq!(tools["tools"][0]["name"], "echo");
        let called = client.request("tools/call", json!({"name":"echo","arguments":{"text":"hi"}})).await.unwrap();
        assert_eq!(called["content"][0]["text"], "hi");
        let refused = client.request("resources/list", json!({})).await.unwrap_err();
        assert_eq!(refused.code, "worker_mcp.request_failed");
    }

    #[tokio::test]
    async fn a_missing_executable_is_an_isolation_failure_not_a_hang() {
        let package = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        let config = json!({"command": "definitely-not-a-real-mcp-binary"});
        let error = Client::connect(&config, package.path(), state.path()).await.err().unwrap();
        assert_eq!(error.code, "worker_mcp.isolation_unavailable");
    }

    #[tokio::test]
    async fn legacy_sse_transport_connects_and_decodes_split_utf8() {
        use axum::{
            Router,
            body::{Body, Bytes},
            extract::State,
            http::{HeaderMap, StatusCode},
            response::{IntoResponse, Response},
            routing::{get, post},
        };
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<std::result::Result<Bytes, std::io::Error>>();
        let _ = tx.send(Ok(Bytes::from("event: endpoint\ndata: /messages?session=1\n\n")));
        let app = Router::new()
            .route(
                "/sse",
                get(|State(rx): State<Arc<AsyncMutex<Option<tokio::sync::mpsc::UnboundedReceiver<std::result::Result<Bytes, std::io::Error>>>>>>| async move {
                    let rx = rx.lock().await.take().unwrap();
                    let stream = tokio_stream::wrappers::UnboundedReceiverStream::new(rx);
                    Response::builder()
                        .status(StatusCode::OK)
                        .header("content-type", "text/event-stream")
                        .body(Body::from_stream(stream))
                        .unwrap()
                }),
            )
            .route(
                "/messages",
                post(move |headers: HeaderMap, body: Bytes| {
                    let tx = tx.clone();
                    async move {
                        let _ = headers;
                        let msg: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
                        let Some(id) = msg.get("id").cloned() else {
                            return StatusCode::ACCEPTED.into_response();
                        };
                        let method = msg["method"].as_str().unwrap_or("");
                        let result = match method {
                            "initialize" => json!({"protocolVersion": "2025-06-18", "capabilities": {"tools": {}}, "serverInfo": {"name": "sse-t", "version": "1"}}),
                            "tools/list" => json!({"tools": [{"name": "echo", "description": "Echo", "inputSchema": {"type": "object"}}]}),
                            "tools/call" => json!({"content": [{"type": "text", "text": msg["params"]["arguments"]["text"]}]}),
                            _ => json!({}),
                        };
                        let frame = format!("event: message\r\ndata: {}\r\n\r\n", json!({"jsonrpc": "2.0", "id": id, "result": result}));
                        let bytes = frame.into_bytes();
                        for byte in bytes {
                            let _ = tx.send(Ok(Bytes::copy_from_slice(&[byte])));
                        }
                        StatusCode::ACCEPTED.into_response()
                    }
                }),
            )
            .with_state(Arc::new(AsyncMutex::new(Some(rx))));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        let package = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        let config = json!({"url": format!("http://{addr}/sse"), "type": "sse"});
        let client = Client::connect(&config, package.path(), state.path()).await.unwrap();
        assert!(client.supports("tools"));
        let called = client.request("tools/call", json!({"name": "echo", "arguments": {"text": "你好，世界"}})).await.unwrap();
        assert_eq!(called["content"][0]["text"], "你好，世界");
        client.stop().await;
    }
}
