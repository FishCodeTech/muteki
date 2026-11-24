//! Starts only our bundled/local backend and owns only the process it started.
use std::{
    fs::{File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    process::{Child, Stdio},
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

pub struct Backend(Arc<Mutex<Vec<Child>>>);
static OWNED: OnceLock<Arc<Mutex<Vec<Child>>>> = OnceLock::new();

extern "C" fn stop_at_exit() {
    if let Some(children) = OWNED.get() {
        stop_owned(children);
    }
}

fn stop_owned(child: &Mutex<Vec<Child>>) {
    let mut children = child.lock().unwrap_or_else(|poison| poison.into_inner());
    if !children.is_empty() {
        stop_local_runner();
    }
    for mut child in children.drain(..) {
        let _ = child.kill();
        let _ = child.wait();
    }
}

impl Drop for Backend {
    fn drop(&mut self) {
        stop_owned(&self.0);
    }
}

/// Set before services start: never reuse services already running (the
/// candidate check and a new version's validation need their own).
static ISOLATED: AtomicBool = AtomicBool::new(false);
/// Set before services start: the service starts with writes closed.
static MAINTENANCE: AtomicBool = AtomicBool::new(false);
/// The secret the service accepts for drain and activity; only set when this
/// process started the service.
static CONTROL: OnceLock<String> = OnceLock::new();

pub fn isolate(maintenance: bool) {
    ISOLATED.store(true, Ordering::SeqCst);
    MAINTENANCE.store(maintenance, Ordering::SeqCst);
}

pub fn control_token() -> Option<&'static str> {
    CONTROL.get().map(String::as_str)
}

pub fn backend_port() -> u16 {
    std::env::var("MUTEKI_BACKEND_PORT").ok().and_then(|port| port.parse().ok()).filter(|port| *port > 0).unwrap_or(8100)
}

pub fn backend_origin() -> String {
    format!("http://127.0.0.1:{}", backend_port())
}

/// A loopback port nothing listens on right now.
pub fn free_port() -> Result<u16, String> {
    std::net::TcpListener::bind(("127.0.0.1", 0))
        .and_then(|listener| listener.local_addr())
        .map(|address| address.port())
        .map_err(|error| format!("Cannot find a free local port: {error}"))
}

/// Closes (`false`) or reopens (`true`) the service's writes. Closing fails
/// while operations or tasks are still running.
pub fn set_writes(open: bool) -> Result<(), String> {
    let token = control_token().ok_or("This app did not start the running service")?;
    let response = reqwest::blocking::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(180))
        .build()
        .map_err(|error| error.to_string())?
        .post(format!("{}/api/desktop/drain", backend_origin()))
        .header("x-muteki-desktop-control", token)
        .json(&serde_json::json!({ "cancel": open }))
        .send()
        .map_err(|error| format!("Cannot reach the service: {error}"))?;
    if response.status().is_success() {
        return Ok(());
    }
    let status = response.status();
    let body: serde_json::Value = response.json().unwrap_or_default();
    Err(body["error"]["message"].as_str().map(str::to_owned).unwrap_or_else(|| format!("The service refused (HTTP {status}).")))
}

/// Asks the service this app started to stop its local Runner before the
/// service itself is killed; otherwise the Runner outlives the app as an
/// orphan. Plain blocking TCP: this also runs from `atexit` and signal paths.
fn stop_local_runner() {
    use std::io::{Read, Write};
    let Some(token) = control_token() else { return };
    let address = std::net::SocketAddr::from(([127, 0, 0, 1], backend_port()));
    let result = (|| -> std::io::Result<String> {
        let mut stream = std::net::TcpStream::connect_timeout(&address, Duration::from_secs(1))?;
        stream.set_read_timeout(Some(Duration::from_secs(5)))?;
        stream.set_write_timeout(Some(Duration::from_secs(2)))?;
        write!(stream, "POST /api/desktop/runner/stop HTTP/1.1\r\nHost: 127.0.0.1\r\nx-muteki-desktop-control: {token}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")?;
        let mut response = String::new();
        stream.take(64 * 1024).read_to_string(&mut response)?;
        Ok(response)
    })();
    match result {
        Ok(response) => {
            let status = response.lines().next().unwrap_or_default();
            let body = response.split_once("\r\n\r\n").map(|(_, body)| body).unwrap_or_default();
            eprintln!("Muteki: local Runner stop: {status} {body}");
        }
        Err(error) => eprintln!("Muteki: local Runner stop failed: {error}"),
    }
}

/// Closes writes for quitting. Returns structured activity naming the
/// background work quitting would interrupt, or `None` when idle or when this
/// process does not manage the service.
pub fn quit_check() -> Option<serde_json::Value> {
    let token = control_token()?;
    let check = || -> Result<Option<serde_json::Value>, String> {
        let response = reqwest::blocking::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(30))
            .build()
            .map_err(|error| error.to_string())?
            .post(format!("{}/api/desktop/drain", backend_origin()))
            .header("x-muteki-desktop-control", token)
            .json(&serde_json::json!({}))
            .send()
            .map_err(|error| format!("无法确认后台任务状态：{error}"))?;
        if response.status().is_success() {
            return Ok(None);
        }
        let status = response.status();
        let body: serde_json::Value = response.json().map_err(|error| format!("退出前检查返回了无效响应：{error}"))?;
        if body["error"]["code"] == "desktop.active" && body["activity"]["tasks"].is_array() {
            return Ok(Some(body["activity"].clone()));
        }
        Err(body["error"]["message"].as_str().map(str::to_owned).unwrap_or_else(|| format!("服务拒绝退出前检查（HTTP {status}）。")))
    };
    check().unwrap_or_else(|error| Some(serde_json::json!({"error":error,"tasks":[]})))
}

pub fn describe_tasks(activity: &serde_json::Value) -> String {
    if let Some(error) = activity["error"].as_str() {
        return error.to_owned();
    }
    const SHOWN: usize = 8;
    let tasks = activity["tasks"].as_array().cloned().unwrap_or_default();
    let mut lines: Vec<String> = tasks
        .iter()
        .take(SHOWN)
        .map(|task| {
            let title = task["title"].as_str().filter(|title| !title.trim().is_empty());
            let id = task["id"].as_str().unwrap_or_default();
            match task["kind"].as_str() {
                Some("turn") => format!("• 对话：{}", title.unwrap_or("未命名对话")),
                _ => format!("• 任务：{}", title.unwrap_or(id)),
            }
        })
        .collect();
    if tasks.len() > SHOWN {
        lines.push(format!("……另有 {} 项，可在应用内查看。", tasks.len() - SHOWN));
    }
    if lines.is_empty() {
        lines.push(format!(
            "{} 个对话、{} 个任务正在运行。",
            activity["turns"].as_i64().unwrap_or(0),
            activity["runs"].as_i64().unwrap_or(0)
        ));
    }
    lines.join("\n")
}

fn probe(client: &reqwest::blocking::Client, expected_state: &Path) -> Result<bool, String> {
    let port = backend_port();
    let response = match client.get(format!("{}/api/health", backend_origin())).send() {
        Ok(response) => response,
        Err(error) if error.is_connect() => return Ok(false),
        Err(error) => return Err(format!("Cannot verify the service on port {port}: {error}")),
    };
    if !response.status().is_success() {
        return Err(format!(
            "Port {port} returned HTTP {} instead of Muteki health.",
            response.status()
        ));
    }
    let body: serde_json::Value = response
        .json()
        .map_err(|error| format!("Port {port} returned invalid health JSON: {error}"))?;
    if body.get("app").and_then(serde_json::Value::as_str) != Some("muteki-rust") {
        return Err(format!("Port {port} is occupied by a service that is not the Muteki Rust backend."));
    }
    use sha2::{Digest, Sha256};
    let root = expected_state.canonicalize().map_err(|error| error.to_string())?;
    let expected = format!("{:x}", Sha256::digest(root.to_string_lossy().as_bytes()));
    if body["storage_identity"].as_str() != Some(expected.as_str()) {
        return Err(format!("Port {port} serves another data directory or an older backend. Quit that service or choose another port before starting Muteki."));
    }
    Ok(true)
}

pub fn start(executable: &Path, bundled: bool) -> Result<Backend, String> {
    let mut log = startup_log(executable, bundled, true)?;
    let _ = writeln!(
        log,
        "Starting Muteki (pid {}, bundled {bundled})",
        std::process::id()
    );
    let _ = writeln!(
        log,
        "State: {}",
        state_directory(executable, bundled)?.display()
    );
    if bundled {
        use std::os::fd::AsRawFd;
        let diagnostics = startup_log(executable, bundled, false)?;
        // SAFETY: duplicate the owned append-only log descriptor onto stderr.
        // Native WebKit delegate diagnostics then survive LaunchServices launch.
        if unsafe { libc::dup2(diagnostics.as_raw_fd(), libc::STDERR_FILENO) } < 0 {
            return Err(format!(
                "Cannot capture native diagnostics: {}",
                std::io::Error::last_os_error()
            ));
        }
    }
    let client = reqwest::blocking::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(2))
        .build()
        .map_err(|error| format!("Cannot create the service health client: {error}"))?;
    let owned = Arc::new(Mutex::new(Vec::new()));
    OWNED
        .set(Arc::clone(&owned))
        .map_err(|_| "Services were already initialized")?;
    // AppKit Quit can call exit directly, bypassing the stack's Drop handlers.
    // SAFETY: the callback uses process-lifetime static storage only.
    if unsafe { libc::atexit(stop_at_exit) } != 0 {
        return Err("Cannot register application exit cleanup".into());
    }
    let on_signal = Arc::clone(&owned);
    ctrlc::set_handler(move || {
        stop_owned(&on_signal);
        std::process::exit(0);
    })
    .map_err(|error| format!("Cannot register application shutdown: {error}"))?;
    let state = state_directory(executable, bundled)?;
    std::fs::create_dir_all(&state).map_err(|error| error.to_string())?;
    crate::shell_env::init_from_cache(&state);
    let backend = Backend(owned);
    start_backend(&client, &backend, executable, bundled)?;
    start_frontend(&client, &backend, executable, bundled)?;
    let refresh_state = state.clone();
    let refresh_exe = executable.to_path_buf();
    let _ = std::thread::Builder::new()
        .name("muteki-shell-env".into())
        .spawn(move || {
            let outcome = crate::shell_env::refresh_blocking(Some(&refresh_state));
            if let Ok(mut log) = startup_log(&refresh_exe, bundled, false) {
                let _ = writeln!(
                    log,
                    "Shell environment discovery: source={}, discovered={}, changed={}",
                    outcome.source, outcome.discovered, outcome.changed
                );
            }
            if let Some(token) = CONTROL.get()
                && let Ok(push_client) = reqwest::blocking::Client::builder()
                    .no_proxy()
                    .timeout(Duration::from_secs(5))
                    .build()
            {
                let url = format!("{}/api/desktop/shell-environment", backend_origin());
                let _ = push_client
                    .post(url)
                    .header("x-muteki-desktop-control", token.as_str())
                    .json(&crate::shell_env::export_payload())
                    .send();
            }
        });
    Ok(backend)
}

fn resolve_node_binary(executable: &Path, bundled: bool) -> PathBuf {
    if bundled
        && let Some(resources) = executable
            .parent()
            .and_then(Path::parent)
            .map(|contents| contents.join("Resources"))
    {
        let bundled_node = resources.join("node/bin/node");
        if crate::shell_env::is_executable_file(&bundled_node) {
            return bundled_node;
        }
    }
    if let Some(explicit) = std::env::var_os("MUTEKI_NODE")
        && !explicit.is_empty()
    {
        return PathBuf::from(explicit);
    }
    crate::shell_env::resolve_program(std::ffi::OsStr::new("node"))
}

fn start_backend(
    client: &reqwest::blocking::Client,
    backend: &Backend,
    executable: &Path,
    bundled: bool,
) -> Result<(), String> {
    let state = state_directory(executable, bundled)?;
    std::fs::create_dir_all(&state).map_err(|error| error.to_string())?;
    if probe(client, &state)? {
        if ISOLATED.load(Ordering::SeqCst) {
            return Err(format!("Port {} already serves a Muteki backend; this run needs its own.", backend_port()));
        }
        let _ = writeln!(
            startup_log(executable, bundled, false)?,
            "Reusing backend on port {}",
            backend_port()
        );
        return Ok(());
    }
    let state = state_directory(executable, bundled)?;
    std::fs::create_dir_all(&state)
        .map_err(|error| format!("Cannot create {}: {error}", state.display()))?;
    let server = executable
        .parent()
        .ok_or("The executable has no directory")?
        .join("muteki-server");
    if bundled && !workspace_package(executable) && std::env::var_os("MUTEKI_STATE_DIR").is_none() {
        crate::storage::prepare_legacy(&state, &server)?;
    }
    let mut log = startup_log(executable, bundled, false)?;
    let _ = writeln!(log, "Starting backend: {}", server.display());
    let control = CONTROL.get_or_init(|| format!("{}{}", uuid::Uuid::new_v4().simple(), uuid::Uuid::new_v4().simple()));
    let node_binary = resolve_node_binary(executable, bundled);
    let mut command = crate::shell_env::command(&server);
    if MAINTENANCE.load(Ordering::SeqCst) {
        command.env("MUTEKI_DESKTOP_MAINTENANCE", "1");
    }
    let child = command
        .args(["--host", "127.0.0.1", "--port", &backend_port().to_string(), "--state-dir"])
        .arg(&state)
        .current_dir(&state)
        .env("MUTEKI_MANAGED_DESKTOP", "1")
        .env("MUTEKI_DESKTOP_CONTROL_TOKEN", control)
        .env("MUTEKI_NODE", &node_binary)
        .env("MUTEKI_DESKTOP_SHELL_DISCOVERY_PENDING", "1")
        .stdin(Stdio::null())
        .stdout(
            log.try_clone()
                .map_err(|error| format!("Cannot copy startup log: {error}"))?,
        )
        .stderr(log)
        .spawn()
        .map_err(|error| {
            format!(
                "Cannot start {}: {error}. Build both muteki-desktop and muteki-server.",
                server.display()
            )
        })?;
    backend
        .0
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .push(child);
    let deadline = Instant::now() + Duration::from_secs(180);
    loop {
        if let Some(status) = backend
            .0
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .last_mut()
            .expect("owned service")
            .try_wait()
            .map_err(|error| format!("Cannot inspect the Muteki service: {error}"))?
        {
            return Err(format!(
                "The Muteki service exited during startup ({status})."
            ));
        }
        if probe(client, &state)? {
            if MAINTENANCE.load(Ordering::SeqCst){return Ok(());}
            let response=client.get(format!("{}/api/readiness",backend_origin())).send().map_err(|error|error.to_string())?;
            let ready:serde_json::Value=response.json().map_err(|error|error.to_string())?;
            if ready["ready"]==true{return Ok(());}
            if ready["initialization"]["state"]=="failed"{return Err(format!("Service initialization failed: {}",ready["initialization"]["error"]));}
        }
        if Instant::now() >= deadline {
            return Err("The Muteki service did not become ready within 180 seconds.".into());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn startup_log(executable: &Path, bundled: bool, truncate: bool) -> Result<File, String> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let state = state_directory(executable, bundled)?;
    std::fs::create_dir_all(&state)
        .map_err(|error| format!("Cannot create {}: {error}", state.display()))?;
    let path = state.join("desktop-startup.log");
    let file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(truncate)
        .append(!truncate)
        .mode(0o600)
        .open(&path)
        .map_err(|error| format!("Cannot open {}: {error}", path.display()))?;
    file.set_permissions(std::fs::Permissions::from_mode(0o600))
        .map_err(|error| format!("Cannot protect {}: {error}", path.display()))?;
    Ok(file)
}

pub fn record_startup_error(executable: &Path, bundled: bool, error: &str) {
    if let Ok(mut log) = startup_log(executable, bundled, false) {
        let _ = writeln!(log, "Cannot start Muteki: {error}");
    }
}

/// Whether `executable` is this workspace's own `dist/Muteki.app`.
pub fn workspace_package(executable: &Path) -> bool {
    let local = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../dist/Muteki.app/Contents/MacOS/Muteki");
    matches!((executable.canonicalize(), local.canonicalize()), (Ok(actual), Ok(local)) if actual == local)
}

pub fn state_directory(executable: &Path, bundled: bool) -> Result<PathBuf, String> {
    if let Some(directory) = std::env::var_os("MUTEKI_STATE_DIR") {
        return std::path::absolute(directory).map_err(|error| error.to_string());
    }
    if bundled {
        if workspace_package(executable) {
            return PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../../.state")
                .canonicalize()
                .map_err(|error| format!("Cannot locate migrated state: {error}"));
        }
        crate::storage::installed_root()
    } else {
        Ok(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../.state"))
    }
}

pub fn ui_origin() -> Result<String, String> {
    let origin =
        std::env::var("MUTEKI_UI_ORIGIN").unwrap_or_else(|_| "http://127.0.0.1:3101".into());
    let url = reqwest::Url::parse(&origin)
        .map_err(|error| format!("Invalid MUTEKI_UI_ORIGIN: {error}"))?;
    if url.scheme() != "http"
        || url.host_str() != Some("127.0.0.1")
        || !url.username().is_empty()
        || url.password().is_some()
        || url.path() != "/"
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err("MUTEKI_UI_ORIGIN must be a plain http://127.0.0.1:PORT origin.".into());
    }
    Ok(url.origin().ascii_serialization())
}

fn frontend_ready(client: &reqwest::blocking::Client, origin: &str) -> Result<bool, String> {
    let response = match client.get(origin).send() {
        Ok(response) => response,
        Err(error) if error.is_connect() => return Ok(false),
        Err(error) => return Err(format!("Cannot verify the frontend at {origin}: {error}")),
    };
    let html = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.contains("text/html"));
    if !response.status().is_success() || !html {
        return Err(format!(
            "{origin} did not return the Muteki HTML frontend (HTTP {}).",
            response.status()
        ));
    }
    let body = response
        .text()
        .map_err(|error| format!("Cannot read frontend: {error}"))?;
    if !body.contains("/_next/") || !body.to_ascii_lowercase().contains("muteki") {
        return Err(format!(
            "{origin} is occupied by a different web application."
        ));
    }
    let response = client
        .get(format!("{origin}/api/health"))
        .send()
        .map_err(|error| format!("Cannot verify frontend API proxy: {error}"))?;
    if !response.status().is_success()
        || response
            .json::<serde_json::Value>()
            .map_err(|error| format!("Invalid frontend API health: {error}"))?
            .get("app")
            .and_then(serde_json::Value::as_str)
            != Some("muteki-rust")
    {
        return Err("The frontend API proxy is not connected to the Muteki Rust backend.".into());
    }
    Ok(true)
}

fn start_frontend(
    client: &reqwest::blocking::Client,
    services: &Backend,
    executable: &Path,
    bundled: bool,
) -> Result<(), String> {
    let origin = ui_origin()?;
    if frontend_ready(client, &origin)? {
        if ISOLATED.load(Ordering::SeqCst) {
            return Err(format!("{origin} already serves a Muteki frontend; this run needs its own."));
        }
        return Ok(());
    }
    let url = reqwest::Url::parse(&origin).expect("validated UI origin");
    let port = url.port_or_known_default().expect("http port").to_string();
    let node = resolve_node_binary(executable, bundled);
    let frontend = if bundled {
        let resources = executable
            .parent()
            .and_then(Path::parent)
            .ok_or("Cannot locate application resources")?
            .join("Resources");
        crate::storage::frontend_cache(&resources)?
    } else {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../apps/frontend")
    };
    let mut command = crate::shell_env::command(node);
    if bundled {
        command.arg(frontend.join("server.js"));
    } else {
        command
            .arg(frontend.join("node_modules/next/dist/bin/next"))
            .args(["start", "-H", "127.0.0.1", "-p", &port]);
    }
    let mut log = startup_log(executable, bundled, false)?;
    let _ = writeln!(log, "Starting frontend: {} at {origin}", frontend.display());
    let child = command.current_dir(&frontend)
        .env("HOSTNAME", "127.0.0.1").env("PORT", &port)
        .env("MUTEKI_UI_HOST", "127.0.0.1")
        .env("NODE_ENV", "production").env("NEXT_TELEMETRY_DISABLED", "1")
        .env("MUTEKI_API_UPSTREAM", backend_origin())
        .stdin(Stdio::null())
        .stdout(log.try_clone().map_err(|error| format!("Cannot copy startup log: {error}"))?)
        .stderr(log)
        .spawn().map_err(|error| format!("Cannot start Next frontend: {error}. Build apps/frontend first and make Node available."))?;
    services
        .0
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .push(child);
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if let Some(status) = services
            .0
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .last_mut()
            .expect("owned frontend")
            .try_wait()
            .map_err(|error| format!("Cannot inspect frontend: {error}"))?
        {
            return Err(format!("The frontend exited during startup ({status})."));
        }
        if frontend_ready(client, &origin)? {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err("The frontend did not become ready within 30 seconds.".into());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}
