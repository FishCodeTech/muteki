//! The Muteki-owned background browser for chats no client is showing.
//!
//! One headless Chrome process serves every thread; each thread gets its own
//! browser context (cookies, storage, history) and tabs. DevTools runs over
//! `--remote-debugging-pipe`, so Chrome exits when this service does. Page
//! work reuses the desktop preview's page functions, so both hosts read and
//! target elements the same way.

use crate::browser::failure;
#[path = "../../desktop/src/browser_policy.rs"]
pub mod browser_policy;
pub(crate) use browser_policy::control_origins;
use base64::Engine;
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, Weak,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::sync::oneshot;

const PAGE_JS: &str = include_str!("../../desktop/src/browser_page.js");
const CAPTURE_JS: &str = include_str!("../../desktop/src/preview.js");
const VIEWPORT: (u32, u32) = (1280, 800);
pub const IDLE_CLOSE: Duration = Duration::from_secs(5 * 60);
const REAP_INTERVAL: Duration = Duration::from_secs(30);
const MAX_TABS: usize = 8;
const MAX_THREADS: usize = 8;
const LAUNCH_TIMEOUT: Duration = Duration::from_secs(20);
const CALL_TIMEOUT: Duration = Duration::from_secs(15);
const EVALUATE_MAX_CHARS: usize = 200_000;

fn crashed() -> Value {
    failure("chat.browser.headless_crashed", "后台浏览器进程已退出，页面状态丢失；请重新调用 chat_browser_open", "state", true)
}

fn not_open() -> Value {
    failure("chat.browser.not_open", "此对话在后台浏览器中没有打开的页面（可能已空闲关闭）；请先调用 chat_browser_open", "state", true)
}

/// The Chromium-family browser this host can run headless.
pub fn executable() -> Result<PathBuf, Value> {
    let missing = |message: String| failure("chat.browser.headless_unavailable", message, "state", false);
    #[cfg(target_os = "linux")]
    if unsafe { libc::geteuid() } == 0 {
        return Err(failure("chat.browser.headless_root_disallowed",
            "Linux 后台浏览器需要以非 root 用户运行 Muteki 并保留 Chromium 沙箱；不会自动关闭沙箱", "permission", false));
    }
    if !cfg!(unix) {
        return Err(missing("后台浏览器只支持 macOS/Linux 服务主机".into()));
    }
    if let Some(path) = std::env::var_os("MUTEKI_BROWSER_EXECUTABLE").filter(|value| !value.is_empty()).map(PathBuf::from) {
        return if path.is_file() { Ok(path) } else {
            Err(missing(format!("MUTEKI_BROWSER_EXECUTABLE 指向的 {} 不存在", path.display())))
        };
    }
    let bundles = ["Google Chrome.app/Contents/MacOS/Google Chrome", "Chromium.app/Contents/MacOS/Chromium",
        "Microsoft Edge.app/Contents/MacOS/Microsoft Edge", "Brave Browser.app/Contents/MacOS/Brave Browser"];
    if cfg!(target_os = "macos") {
        let mut roots = vec![PathBuf::from("/Applications")];
        if let Some(home) = std::env::var_os("HOME") { roots.push(PathBuf::from(home).join("Applications")); }
        for root in &roots {
            for bundle in bundles {
                let path = root.join(bundle);
                if path.is_file() { return Ok(path); }
            }
        }
    }
    let names = ["google-chrome", "google-chrome-stable", "chromium", "chromium-browser", "microsoft-edge", "microsoft-edge-stable"];
    for dir in std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()) {
        for name in names {
            let path = dir.join(name);
            if path.is_file() { return Ok(path); }
        }
    }
    Err(missing("服务主机上没有找到 Chrome、Chromium 或 Edge，后台浏览器不可用；安装其中之一或设置 MUTEKI_BROWSER_EXECUTABLE".into()))
}

pub(crate) fn protected_ports(api_port: u16) -> Vec<u16> {
    browser_policy::protected_ports(api_port, None)
}

pub(crate) fn browser_origins(api_port: u16) -> Vec<String> {
    browser_policy::browser_origins(api_port, None)
}

pub(crate) fn authorize_url(_url: &str, _api_port: u16) -> Result<(), Value> {
    Ok(())
}

#[allow(dead_code)]
fn interception(api_port: u16) -> Value {
    let mut urls = Vec::new();
    for port in protected_ports(api_port) {
        urls.push(format!("*://*:{port}/*"));
        if port == 80 { urls.extend(["http://*".to_owned(), "ws://*".to_owned()]); }
        if port == 443 { urls.extend(["https://*".to_owned(), "wss://*".to_owned()]); }
    }
    let mut allowed = Vec::new();
    for origin in browser_origins(api_port) {
        let pattern = if let Some((prefix, suffix)) = origin.split_once('[')
            && let Some((address, port)) = suffix.split_once(']')
        {
            format!("{prefix}[{}]{port}/*", address.replace(':', "\\:"))
        } else {
            format!("{origin}/*")
        };
        allowed.push(json!({"urlPattern": pattern, "block": false}));
    }
    json!({"urls": urls, "urlPatterns": allowed})
}

fn auto_attach() -> Value {
    json!({"autoAttach": true, "waitForDebuggerOnStart": true, "flatten": true,
        "filter": [{"type":"page"}, {"type":"iframe"}, {"type":"worker"},
            {"type":"shared_worker"}, {"type":"service_worker"}, {"exclude":true}]})
}

#[allow(dead_code)]
fn blocked_requests(api_port: u16) -> Value {
    json!({"patterns": interception(api_port)["urls"].as_array().into_iter().flatten()
        .map(|url| json!({"urlPattern": url, "requestStage": "Request"})).collect::<Vec<_>>()})
}


type Waiters = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value, Value>>>>>;

#[derive(Clone)]
struct Cdp {
    writer: std::sync::mpsc::Sender<Vec<u8>>,
    pending: Waiters,
    next: Arc<AtomicU64>,
    alive: Arc<AtomicBool>,
    dialogs: Arc<Mutex<HashMap<String, Vec<Value>>>>,
}

impl Cdp {
    async fn call(&self, session: Option<&str>, method: &str, params: Value, timeout: Duration) -> Result<Value, Value> {
        if !self.alive.load(Ordering::SeqCst) {
            return Err(crashed());
        }
        let id = self.next.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(id, tx);
        let mut message = json!({"id": id, "method": method, "params": params});
        if let Some(session) = session {
            message["sessionId"] = json!(session);
        }
        if self.writer.send(serde_json::to_vec(&message).unwrap_or_default()).is_err() {
            self.pending.lock().unwrap().remove(&id);
            return Err(crashed());
        }
        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(reply)) => reply.map_err(|error| {
                let mut result = failure("chat.browser.headless.protocol_failed",
                    format!("{method}: {}", error["message"].as_str().unwrap_or("后台浏览器拒绝了请求")), "runtime", false);
                result["protocol_code"] = error["code"].clone();
                result
            }),
            Ok(Err(_)) => Err(crashed()),
            Err(_) => {
                self.pending.lock().unwrap().remove(&id);
                Err(failure("chat.browser.timeout", format!("后台浏览器在 {} 秒内没有完成 {method}，结果未知", timeout.as_secs()), "timeout", true))
            }
        }
    }

    fn take_dialogs(&self, session: &str) -> Vec<Value> {
        self.dialogs.lock().unwrap().remove(session).unwrap_or_default()
    }
}

struct Chrome {
    cdp: Cdp,
    child: Mutex<Option<std::process::Child>>,
    dir: PathBuf,
    executable: PathBuf,
    log: PathBuf,
    started: Instant,
    /// Emulation.setUserAgentOverride parameters every tab gets.
    identity: Value,
    api_port: u16,
}

impl Chrome {
    fn shutdown(&self) {
        self.cdp.alive.store(false, Ordering::SeqCst);
        if let Some(mut child) = self.child.lock().unwrap().take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

impl Drop for Chrome {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[cfg(unix)]
async fn launch(state_dir: &Path, api_port: u16) -> Result<Chrome, Value> {
    use std::os::unix::{io::FromRawFd, process::CommandExt};
    let executable = executable()?;
    let unavailable = |message: String| failure("chat.browser.headless_launch_failed", message, "runtime", true);
    let root = state_dir.join("browser-headless");
    let dir = root.join(uuid::Uuid::new_v4().simple().to_string());
    std::fs::create_dir_all(&dir).map_err(|error| unavailable(format!("后台浏览器目录 {} 无法创建：{error}", dir.display())))?;
    let log = root.join("chrome-stderr.log");
    let stderr = std::fs::File::create(&log).map_err(|error| unavailable(format!("后台浏览器日志无法创建：{error}")))?;
    let mut to_child = [0; 2];
    let mut from_child = [0; 2];
    // SAFETY: plain pipe(2)/fcntl(2) on fresh descriptors owned by this function.
    unsafe {
        if libc::pipe(to_child.as_mut_ptr()) != 0 || libc::pipe(from_child.as_mut_ptr()) != 0 {
            return Err(unavailable(format!("无法创建 DevTools 管道：{}", std::io::Error::last_os_error())));
        }
        for fd in to_child.iter().chain(&from_child) {
            libc::fcntl(*fd, libc::F_SETFD, libc::FD_CLOEXEC);
        }
    }
    let (child_read, child_write) = (to_child[0], from_child[1]);
    let mut command = crate::host_process::std_command(&executable);
    command.args(["--headless=new", "--remote-debugging-pipe", "--no-first-run", "--no-default-browser-check",
        "--disable-background-networking", "--disable-sync", "--disable-extensions", "--disable-component-update",
        "--disable-default-apps", "--mute-audio", "--hide-scrollbars", "--password-store=basic", "--use-mock-keychain",
        // Without this a page reads navigator.webdriver === true.
        "--disable-blink-features=AutomationControlled"])
        .arg(format!("--user-data-dir={}", dir.display()))
        .arg(format!("--window-size={},{}", VIEWPORT.0, VIEWPORT.1))
        .arg("about:blank")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(stderr);
    // SAFETY: only async-signal-safe fcntl/dup2 run between fork and exec.
    unsafe {
        command.pre_exec(move || {
            // Lift both ends above 3/4 first so dup2 cannot clobber one with the other.
            let read = libc::fcntl(child_read, libc::F_DUPFD_CLOEXEC, 10);
            let write = libc::fcntl(child_write, libc::F_DUPFD_CLOEXEC, 10);
            if read < 0 || write < 0 || libc::dup2(read, 3) < 0 || libc::dup2(write, 4) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let spawned = command.spawn();
    // SAFETY: the child holds its own copies; the parent no longer uses these ends.
    unsafe {
        libc::close(child_read);
        libc::close(child_write);
    }
    let child = match spawned {
        Ok(child) => child,
        Err(error) => {
            // SAFETY: closing the parent ends that would otherwise leak.
            unsafe { libc::close(to_child[1]); libc::close(from_child[0]); }
            let _ = std::fs::remove_dir_all(&dir);
            return Err(unavailable(format!("{} 无法启动：{error}", executable.display())));
        }
    };
    // SAFETY: each descriptor is owned by exactly one File from here on.
    let mut writer_file = unsafe { std::fs::File::from_raw_fd(to_child[1]) };
    let reader_file = unsafe { std::fs::File::from_raw_fd(from_child[0]) };
    let (writer, outbox) = std::sync::mpsc::channel::<Vec<u8>>();
    let pending: Waiters = Arc::default();
    let next = Arc::new(AtomicU64::new(1));
    let alive = Arc::new(AtomicBool::new(true));
    let dialogs: Arc<Mutex<HashMap<String, Vec<Value>>>> = Arc::default();
    let popup_identity: Arc<Mutex<Value>> = Arc::default();
    let runtime = tokio::runtime::Handle::current();
    std::thread::Builder::new().name("muteki-browser-out".into()).spawn(move || {
        use std::io::Write;
        while let Ok(mut message) = outbox.recv() {
            message.push(0);
            if writer_file.write_all(&message).and_then(|_| writer_file.flush()).is_err() { break; }
        }
    }).map_err(|error| unavailable(error.to_string()))?;
    {
        let (pending, alive, dialogs, next, writer, popup_identity) =
            (pending.clone(), alive.clone(), dialogs.clone(), next.clone(), writer.clone(), popup_identity.clone());
        std::thread::Builder::new().name("muteki-browser-in".into()).spawn(move || {
            use std::io::BufRead;
            let mut reader = std::io::BufReader::new(reader_file);
            let mut frame = Vec::new();
            loop {
                frame.clear();
                match reader.read_until(0, &mut frame) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {}
                }
                if frame.last() == Some(&0) { frame.pop(); }
                let Ok(message) = serde_json::from_slice::<Value>(&frame) else { continue };
                if let Some(id) = message["id"].as_u64() {
                    if let Some(waiter) = pending.lock().unwrap().remove(&id) {
                        let _ = waiter.send(match message.get("error") {
                            Some(error) if !error.is_null() => Err(error.clone()),
                            _ => Ok(message["result"].clone()),
                        });
                    }
                    continue;
                }
                if message["method"] == "Fetch.requestPaused"
                    && let (Some(session), Some(request)) = (message["sessionId"].as_str(), message["params"]["requestId"].as_str())
                {
                    let id = next.fetch_add(1, Ordering::SeqCst);
                    let reply = json!({"id":id,"sessionId":session,"method":"Fetch.continueRequest","params":{"requestId":request}});
                    let _ = writer.send(serde_json::to_vec(&reply).unwrap_or_default());
                }
                if message["method"] == "Page.javascriptDialogOpening"
                    && let Some(session) = message["sessionId"].as_str()
                {
                    let params = &message["params"];
                    let accept = matches!(params["type"].as_str(), Some("alert" | "beforeunload"));
                    dialogs.lock().unwrap().entry(session.to_owned()).or_default().push(json!({
                        "type": params["type"], "message": params["message"], "url": params["url"],
                        "handled": if accept { "accepted" } else { "dismissed" }}));
                    let id = next.fetch_add(1, Ordering::SeqCst);
                    let reply = json!({"id": id, "sessionId": session, "method": "Page.handleJavaScriptDialog",
                        "params": {"accept": accept}});
                    let _ = writer.send(serde_json::to_vec(&reply).unwrap_or_default());
                }
                if message["method"] == "Target.attachedToTarget" && message["params"]["waitingForDebugger"] == true
                    && let Some(session) = message["params"]["sessionId"].as_str()
                {
                    let identity = popup_identity.lock().unwrap().clone();
                    let session = session.to_owned();
                    let target = message["params"]["targetInfo"]["targetId"].clone();
                    let page = matches!(message["params"]["targetInfo"]["type"].as_str(), Some("page" | "iframe"));
                    let cdp = Cdp { writer: writer.clone(), pending: pending.clone(), next: next.clone(), alive: alive.clone(), dialogs: dialogs.clone() };
                    runtime.spawn(async move {
                        let installed: Result<(), Value> = async {
                            let (network, children, ua, page_enabled, capture, resumed) = tokio::join!(biased;
                                cdp.call(Some(&session), "Network.enable", json!({}), CALL_TIMEOUT),
                                cdp.call(Some(&session), "Target.setAutoAttach", auto_attach(), CALL_TIMEOUT),
                                async { if page && !identity.is_null() {
                                    cdp.call(Some(&session), "Emulation.setUserAgentOverride", identity, CALL_TIMEOUT).await
                                } else { Ok(Value::Null) } },
                                async { if page { cdp.call(Some(&session), "Page.enable", json!({}), CALL_TIMEOUT).await } else { Ok(Value::Null) } },
                                async { if page { cdp.call(Some(&session), "Page.addScriptToEvaluateOnNewDocument",
                                    json!({"source": CAPTURE_JS, "runImmediately": true}), CALL_TIMEOUT).await } else { Ok(Value::Null) } },
                                cdp.call(Some(&session), "Runtime.runIfWaitingForDebugger", json!({}), CALL_TIMEOUT)
                            );
                            for result in [network, children, ua, page_enabled, capture, resumed] { result?; }
                            Ok(())
                        }.await;
                        if let Err(error) = installed {
                            tracing::error!(%error, "Browser target could not install its network policy; closing it");
                            let _ = cdp.call(None, "Target.closeTarget", json!({"targetId": target}), CALL_TIMEOUT).await;
                        }
                    });
                }
            }
            alive.store(false, Ordering::SeqCst);
            for (_, waiter) in pending.lock().unwrap().drain() {
                let _ = waiter.send(Err(json!({"message": "后台浏览器进程已退出"})));
            }
        }).map_err(|error| unavailable(error.to_string()))?;
    }
    let chrome = Chrome {
        cdp: Cdp { writer, pending, next, alive, dialogs },
        child: Mutex::new(Some(child)),
        dir,
        executable,
        log,
        started: Instant::now(),
        identity: Value::Null,
        api_port,
    };
    match chrome.cdp.call(None, "Browser.getVersion", json!({}), LAUNCH_TIMEOUT).await {
        Ok(version) => {
            tracing::info!(executable = %chrome.executable.display(), product = %version["product"].as_str().unwrap_or(""),
                "Background browser started");
            let mut chrome = chrome;
            chrome.identity = identity(&version, &chrome.executable);
            *popup_identity.lock().unwrap() = chrome.identity.clone();
            chrome.cdp.call(None, "Target.setAutoAttach", auto_attach(), LAUNCH_TIMEOUT).await
                .map_err(|error| { chrome.shutdown(); unavailable(format!("后台浏览器无法监听新页面：{}", error["message"].as_str().unwrap_or(""))) })?;
            Ok(chrome)
        }
        Err(error) => {
            chrome.shutdown();
            Err(unavailable(format!("{} 没有完成 DevTools 握手（{}）；stderr 见 {}", chrome.executable.display(),
                error["message"].as_str().unwrap_or(""), chrome.log.display())))
        }
    }
}

/// The browser as a normal desktop install presents itself: headless Chrome
/// says "HeadlessChrome" in its user agent and client hints and sends
/// English-only Accept-Language, which sites treat as a bot.
/// The client-hint brand list Chrome builds for itself (Chromium's
/// GenerateBrandVersionList): a GREASE brand and the brand order both derive
/// from the major version, so every tab matches what the browser sends natively.
fn brand_list(major: &str, full: &str, brand: Option<&str>) -> (Value, Value) {
    const CHARS: [char; 11] = [' ', '(', ':', '-', '.', '/', ')', ';', '=', '?', '_'];
    const VERSIONS: [&str; 3] = ["8", "99", "24"];
    const ORDERS: [[usize; 3]; 6] = [[0, 1, 2], [0, 2, 1], [1, 0, 2], [1, 2, 0], [2, 0, 1], [2, 1, 0]];
    let seed: usize = major.parse().unwrap_or(0);
    let grease = format!("Not{}A{}Brand", CHARS[seed % CHARS.len()], CHARS[(seed + 1) % CHARS.len()]);
    let grease_version = VERSIONS[seed % VERSIONS.len()];
    let mut entries = vec![(grease, grease_version.to_owned(), format!("{grease_version}.0.0.0")),
        ("Chromium".to_owned(), major.to_owned(), full.to_owned())];
    if let Some(brand) = brand { entries.push((brand.to_owned(), major.to_owned(), full.to_owned())); }
    let order: Vec<usize> = if entries.len() == 3 { ORDERS[seed % 6].to_vec() } else { vec![seed % 2, (seed + 1) % 2] };
    let mut placed: Vec<(usize, &(String, String, String))> = entries.iter().enumerate().map(|(index, entry)| (order[index], entry)).collect();
    placed.sort_by_key(|(position, _)| *position);
    (json!(placed.iter().map(|(_, (name, version, _))| json!({"brand": name, "version": version})).collect::<Vec<_>>()),
     json!(placed.iter().map(|(_, (name, _, full))| json!({"brand": name, "version": full})).collect::<Vec<_>>()))
}

fn identity(version: &Value, executable: &Path) -> Value {
    let agent = version["userAgent"].as_str().unwrap_or("").replace("HeadlessChrome/", "Chrome/");
    let full = version["product"].as_str().and_then(|product| product.split_once('/')).map_or("", |(_, version)| version).to_owned();
    let major = full.split('.').next().unwrap_or("").to_owned();
    let name = executable.file_name().map(|name| name.to_string_lossy().to_lowercase()).unwrap_or_default();
    let brand = if name.contains("edge") { Some("Microsoft Edge") } else if name.contains("brave") { Some("Brave") }
        else if name.contains("chromium") { None } else { Some("Google Chrome") };
    let (brands, full_list) = brand_list(&major, &full, brand);
    let (platform, navigator_platform, platform_version) = if cfg!(target_os = "macos") {
        let version = crate::host_process::std_command("sw_vers").arg("-productVersion").output().ok()
            .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned()).unwrap_or_default();
        let mut parts: Vec<&str> = version.split('.').filter(|part| !part.is_empty()).collect();
        while !parts.is_empty() && parts.len() < 3 { parts.push("0"); }
        ("macOS", "MacIntel", parts.join("."))
    } else {
        ("Linux", if cfg!(target_arch = "aarch64") { "Linux aarch64" } else { "Linux x86_64" }, String::new())
    };
    let mut out = json!({"userAgent": agent, "platform": navigator_platform, "userAgentMetadata": {
        "brands": brands, "fullVersionList": full_list, "fullVersion": full, "platform": platform, "platformVersion": platform_version,
        "architecture": if cfg!(target_arch = "aarch64") { "arm" } else { "x86" }, "model": "", "mobile": false, "bitness": "64", "wow64": false}});
    if let Some(languages) = accept_language() { out["acceptLanguage"] = json!(languages); }
    out
}

/// Accept-Language as a fresh desktop Chrome sends it: the host's first
/// preferred language and its base language. Chrome adds the q-values itself.
fn accept_language() -> Option<String> {
    let tag = if cfg!(target_os = "macos") {
        let output = crate::host_process::std_command("defaults").args(["read", "-g", "AppleLanguages"]).output().ok()?;
        String::from_utf8_lossy(&output.stdout).split('"').nth(1).map(str::to_owned)
    } else {
        crate::shell_env::var("LANG").map(|lang| lang.split('.').next().unwrap_or("").replace('_', "-"))
    }?;
    let parts: Vec<&str> = tag.split('-').filter(|part| part.len() != 4).collect();
    let base = parts.first().filter(|base| base.len() >= 2 && base.chars().all(|c| c.is_ascii_alphabetic()))?.to_lowercase();
    let full = parts.join("-");
    Some(if full == base { base } else { format!("{full},{base}") })
}
#[cfg(not(unix))]
async fn launch(_state_dir: &Path, _api_port: u16) -> Result<Chrome, Value> {
    Err(executable().err().unwrap_or_else(|| failure("chat.browser.headless_unavailable", "后台浏览器只支持 macOS/Linux 服务主机", "state", false)))
}


struct Tab {
    target_id: String,
    session_id: String,
}

struct Workspace {
    chrome: Arc<Chrome>,
    context_id: String,
    viewport: (u32, u32),
    tabs: Vec<Tab>,
    active: usize,
    last_used: Instant,
}

struct Entry {
    alive: Arc<AtomicBool>,
    workspace: Arc<tokio::sync::Mutex<Workspace>>,
}

#[derive(Default)]
struct Shared {
    chrome: tokio::sync::Mutex<Option<Arc<Chrome>>>,
    threads: Mutex<HashMap<String, Entry>>,
    creating: tokio::sync::Mutex<()>,
    reaper: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

#[derive(Default)]
pub struct Headless {
    shared: Arc<Shared>,
}

async fn attach_tab(chrome: &Chrome, context_id: &str, viewport: (u32, u32)) -> Result<Tab, Value> {
    let target = chrome.cdp.call(None, "Target.createTarget", json!({"url": "about:blank", "browserContextId": context_id}), CALL_TIMEOUT).await?;
    prepare_tab(chrome, target["targetId"].as_str().unwrap_or("").to_owned(), viewport).await
}

/// Drop tabs whose target is gone: the page closed itself (window.close) or
/// Chrome discarded it. A closed active tab hands over to the tab that was
/// opened most recently. Returns how many tabs left and whether the active one did.
async fn prune_tabs(workspace: &mut Workspace) -> Result<(usize, bool), Value> {
    let targets = workspace.chrome.cdp.call(None, "Target.getTargets", json!({}), CALL_TIMEOUT).await?;
    let live: std::collections::HashSet<&str> = targets["targetInfos"].as_array().into_iter().flatten()
        .filter_map(|info| info["targetId"].as_str()).collect();
    let active = workspace.tabs.get(workspace.active).map(|tab| tab.target_id.clone());
    let before = workspace.tabs.len();
    workspace.tabs.retain(|tab| live.contains(tab.target_id.as_str()));
    let closed = before - workspace.tabs.len();
    let position = active.as_deref().and_then(|id| workspace.tabs.iter().position(|tab| tab.target_id == id));
    workspace.active = position.unwrap_or(workspace.tabs.len().saturating_sub(1));
    Ok((closed, active.is_some() && position.is_none()))
}

async fn prepare_tab(chrome: &Chrome, target_id: String, viewport: (u32, u32)) -> Result<Tab, Value> {
    let cdp = &chrome.cdp;
    let attached = cdp.call(None, "Target.attachToTarget", json!({"targetId": target_id, "flatten": true}), CALL_TIMEOUT).await?;
    let session_id = attached["sessionId"].as_str().unwrap_or("").to_owned();
    if target_id.is_empty() || session_id.is_empty() {
        return Err(failure("chat.browser.headless.protocol_failed", "后台浏览器没有返回标签页标识", "runtime", false));
    }
    let session = Some(session_id.as_str());
    cdp.call(session, "Network.enable", json!({}), CALL_TIMEOUT).await?;
    if !chrome.identity.is_null() {
        cdp.call(session, "Emulation.setUserAgentOverride", chrome.identity.clone(), CALL_TIMEOUT).await?;
    }
    cdp.call(session, "Page.enable", json!({}), CALL_TIMEOUT).await?;
    cdp.call(session, "Page.addScriptToEvaluateOnNewDocument", json!({"source": CAPTURE_JS}), CALL_TIMEOUT).await?;
    cdp.call(session, "Emulation.setDeviceMetricsOverride",
        json!({"width": viewport.0, "height": viewport.1, "deviceScaleFactor": 1, "mobile": false}), CALL_TIMEOUT).await?;
    cdp.call(session, "Emulation.setFocusEmulationEnabled", json!({"enabled": true}), CALL_TIMEOUT).await?;
    Ok(Tab { target_id, session_id })
}

impl Headless {
    pub fn has_page(&self, thread_id: &str) -> bool {
        self.shared.threads.lock().unwrap().get(thread_id).is_some_and(|entry| entry.alive.load(Ordering::SeqCst))
    }

    async fn chrome(&self, state_dir: &Path, api_port: u16) -> Result<Arc<Chrome>, Value> {
        let mut slot = self.shared.chrome.lock().await;
        if let Some(chrome) = slot.as_ref().filter(|chrome| chrome.cdp.alive.load(Ordering::SeqCst)) {
            return Ok(chrome.clone());
        }
        if slot.take().is_some() {
            tracing::warn!("Background browser exited; starting a new one");
        }
        let chrome = Arc::new(launch(state_dir, api_port).await?);
        *slot = Some(chrome.clone());
        self.spawn_reaper();
        Ok(chrome)
    }

    async fn workspace(&self, state_dir: &Path, api_port: u16, thread_id: &str, create: bool)
        -> Result<(Arc<tokio::sync::Mutex<Workspace>>, bool), Value> {
        let live = |shared: &Shared| shared.threads.lock().unwrap().get(thread_id)
            .filter(|entry| entry.alive.load(Ordering::SeqCst)).map(|entry| entry.workspace.clone());
        if let Some(workspace) = live(&self.shared) { return Ok((workspace, false)); }
        if !create { return Err(not_open()); }
        let _creating = self.shared.creating.lock().await;
        if let Some(workspace) = live(&self.shared) { return Ok((workspace, false)); }
        self.shared.threads.lock().unwrap().remove(thread_id);
        self.evict_for_room().await;
        let chrome = self.chrome(state_dir, api_port).await?;
        let context = chrome.cdp.call(None, "Target.createBrowserContext", json!({"disposeOnDetach": false}), CALL_TIMEOUT).await?;
        let context_id = context["browserContextId"].as_str().unwrap_or("").to_owned();
        let tab = attach_tab(&chrome, &context_id, VIEWPORT).await?;
        let alive = chrome.cdp.alive.clone();
        let workspace = Arc::new(tokio::sync::Mutex::new(Workspace { chrome, context_id, viewport: VIEWPORT, tabs: vec![tab], active: 0, last_used: Instant::now() }));
        self.shared.threads.lock().unwrap().insert(thread_id.to_owned(), Entry { alive, workspace: workspace.clone() });
        Ok((workspace, true))
    }

    async fn evict_for_room(&self) {
        let candidates: Vec<(String, Arc<tokio::sync::Mutex<Workspace>>)> = {
            let threads = self.shared.threads.lock().unwrap();
            if threads.len() < MAX_THREADS { return; }
            threads.iter().map(|(id, entry)| (id.clone(), entry.workspace.clone())).collect()
        };
        let oldest = candidates.iter()
            .filter_map(|(id, workspace)| workspace.try_lock().ok().map(|guard| (id.clone(), guard.last_used)))
            .min_by_key(|(_, used)| *used);
        if let Some((id, _)) = oldest {
            tracing::info!(thread = %id, "Background browser closed the least recently used chat to stay within its limit");
            self.close_thread(&id).await;
        }
    }

    pub async fn close_thread(&self, thread_id: &str) {
        let Some(entry) = self.shared.threads.lock().unwrap().remove(thread_id) else { return };
        let workspace = entry.workspace.lock().await;
        if workspace.chrome.cdp.alive.load(Ordering::SeqCst) {
            let _ = workspace.chrome.cdp.call(None, "Target.disposeBrowserContext",
                json!({"browserContextId": workspace.context_id}), CALL_TIMEOUT).await;
        }
    }

    fn spawn_reaper(&self) {
        let mut reaper = self.shared.reaper.lock().unwrap();
        if reaper.as_ref().is_some_and(|task| !task.is_finished()) { return; }
        let weak: Weak<Shared> = Arc::downgrade(&self.shared);
        *reaper = Some(tokio::spawn(async move {
            let mut interval = tokio::time::interval(REAP_INTERVAL);
            interval.tick().await;
            loop {
                interval.tick().await;
                let Some(shared) = weak.upgrade() else { break };
                let headless = Headless { shared };
                if headless.reap().await { break; }
            }
        }));
    }

    /// Close idle threads, then the process once no thread uses it. Returns
    /// whether the browser is gone and the reaper can stop.
    async fn reap(&self) -> bool {
        let candidates: Vec<(String, bool, Arc<tokio::sync::Mutex<Workspace>>)> = self.shared.threads.lock().unwrap().iter()
            .map(|(id, entry)| (id.clone(), entry.alive.load(Ordering::SeqCst), entry.workspace.clone())).collect();
        for (id, alive, workspace) in candidates {
            let idle = workspace.try_lock().is_ok_and(|guard| guard.last_used.elapsed() >= IDLE_CLOSE);
            if !alive {
                self.shared.threads.lock().unwrap().remove(&id);
            } else if idle {
                tracing::info!(thread = %id, "Background browser closed an idle chat");
                self.close_thread(&id).await;
            }
        }
        if !self.shared.threads.lock().unwrap().is_empty() { return false; }
        let mut slot = self.shared.chrome.lock().await;
        if slot.as_ref().is_some_and(|chrome| chrome.started.elapsed() < IDLE_CLOSE && chrome.cdp.alive.load(Ordering::SeqCst)) {
            return false;
        }
        if let Some(chrome) = slot.take() {
            chrome.shutdown();
            tracing::info!("Background browser stopped: no chat uses it");
        }
        true
    }

    pub async fn close(&self) {
        let ids: Vec<String> = self.shared.threads.lock().unwrap().keys().cloned().collect();
        for id in ids { self.close_thread(&id).await; }
        if let Some(chrome) = self.shared.chrome.lock().await.take() { chrome.shutdown(); }
        if let Some(task) = self.shared.reaper.lock().unwrap().take() { task.abort(); }
    }

    pub async fn status(&self, thread_id: &str) -> Value {
        let available = executable();
        let running = self.shared.chrome.try_lock().ok()
            .and_then(|slot| slot.as_ref().map(|chrome| chrome.cdp.alive.load(Ordering::SeqCst)));
        let mut out = json!({
            "available": available.is_ok(),
            "executable": available.as_ref().ok(),
            "error": available.as_ref().err(),
            "running": running.unwrap_or(false),
            "chats_open": self.shared.threads.lock().unwrap().len(),
            "idle_close_s": IDLE_CLOSE.as_secs(),
            "viewport": {"width": VIEWPORT.0, "height": VIEWPORT.1},
        });
        if let Ok((workspace, _)) = self.workspace(Path::new(""), 0, thread_id, false).await
            && let Ok(mut workspace) = tokio::time::timeout(Duration::from_secs(2), workspace.lock()).await
        {
            let _ = prune_tabs(&mut workspace).await;
            let state = match workspace.tabs.get(workspace.active) {
                Some(tab) => state(&workspace.chrome.cdp, &tab.session_id).await.unwrap_or(Value::Null),
                None => Value::Null,
            };
            out["this_chat"] = json!({"tabs": workspace.tabs.len(), "active_tab": workspace.active,
                "viewport": {"width": workspace.viewport.0, "height": workspace.viewport.1},
                "page": state, "idle_s": workspace.last_used.elapsed().as_secs()});
        }
        out
    }

    /// The active tab's picture. It never waits for, or delays, an agent action.
    pub async fn thumbnail(&self, thread_id: &str) -> Result<Vec<u8>, Value> {
        let (workspace, _) = self.workspace(Path::new(""), 0, thread_id, false).await?;
        let workspace = workspace.try_lock()
            .map_err(|_| failure("chat.browser.headless_busy", "后台浏览器正在执行操作", "state", true))?;
        let session = workspace.tabs.get(workspace.active).ok_or_else(not_open)?.session_id.clone();
        let shot = workspace.chrome.cdp.call(Some(&session), "Page.captureScreenshot",
            json!({"format": "jpeg", "quality": 60}), CALL_TIMEOUT).await?;
        shot["data"].as_str().and_then(|data| base64::engine::general_purpose::STANDARD.decode(data).ok())
            .ok_or_else(|| failure("chat.browser.screenshot_invalid", "后台浏览器没有返回截图", "runtime", false))
    }

    pub async fn perform(&self, state_dir: &Path, api_port: u16, thread_id: &str, action: &str, input: &Value, budget: Duration) -> Result<Value, Value> {
        let end = Instant::now() + budget;
        let opening = action == "open";
        let (workspace, created) = self.workspace(state_dir, api_port, thread_id, opening).await?;
        let mut workspace = workspace.lock().await;
        workspace.last_used = Instant::now();
        let mut extra = serde_json::Map::new();
        let mut fresh = created;
        if !created {
            let (closed, active_closed) = prune_tabs(&mut workspace).await?;
            if closed > 0 { extra.insert("closed_tabs".into(), json!(closed)); }
            if workspace.tabs.is_empty() {
                if !opening {
                    return Err(failure("chat.browser.not_open", "此对话的后台标签页都已被网页自己关闭；调用 chat_browser_open 重新打开", "state", true));
                }
                let tab = attach_tab(&workspace.chrome, &workspace.context_id.clone(), workspace.viewport).await?;
                workspace.tabs.push(tab);
                workspace.active = 0;
                fresh = true;
            } else if active_closed {
                extra.insert("active_tab_closed".into(), json!(true));
                if matches!(action, "click" | "type" | "press" | "scroll" | "evaluate") {
                    let session = workspace.tabs[workspace.active].session_id.clone();
                    let page = state(&workspace.chrome.cdp, &session).await.unwrap_or(Value::Null);
                    let mut error = failure("chat.browser.tab_closed", format!(
                        "上一个标签页已被网页自己关闭（例如 window.close），当前改为标签页 {}（{}）；本次 {action} 没有执行，先 chat_browser_read 确认页面后再操作",
                        workspace.active, page["url"].as_str().unwrap_or("")), "state", true);
                    error["page"] = page;
                    error["tab"] = json!({"index": workspace.active, "count": workspace.tabs.len()});
                    return Err(error);
                }
            }
        }
        if action == "open" && input["new_tab"] == true && !fresh {
            if workspace.tabs.len() >= MAX_TABS {
                let oldest = (0..workspace.tabs.len()).find(|index| *index != workspace.active).unwrap_or(0);
                let tab = workspace.tabs.remove(oldest);
                if workspace.active > oldest { workspace.active -= 1; }
                let _ = workspace.chrome.cdp.call(None, "Target.closeTarget", json!({"targetId": tab.target_id}), CALL_TIMEOUT).await;
                extra.insert("closed_oldest_tab".into(), json!(true));
            }
            let tab = attach_tab(&workspace.chrome, &workspace.context_id.clone(), workspace.viewport).await?;
            workspace.tabs.push(tab);
            workspace.active = workspace.tabs.len() - 1;
        }
        let chrome = workspace.chrome.clone();
        let session = workspace.tabs[workspace.active].session_id.clone();
        if action == "resize" {
            let width = input["width"].as_u64().unwrap_or(0) as u32;
            let height = input["height"].as_u64().unwrap_or(0) as u32;
            for tab in &workspace.tabs {
                chrome.cdp.call(Some(&tab.session_id), "Emulation.setDeviceMetricsOverride",
                    json!({"width": width, "height": height, "deviceScaleFactor": 1, "mobile": false}), CALL_TIMEOUT).await?;
            }
            workspace.viewport = (width, height);
            workspace.last_used = Instant::now();
            return Ok(json!({"host": "headless", "width": width, "height": height, "tabs": workspace.tabs.len()}));
        }
        let page = Page { cdp: &chrome.cdp, session: &session, end, viewport: workspace.viewport };
        if action != "open" && !(action == "navigate" && input["url"].is_string()) {
            if let Some(url) = page.state().await?["url"].as_str() { authorize_url(url, api_port)?; }
        }
        let mut result = page.run(action, input).await;
        if result.is_err() && matches!(action, "click" | "press" | "type")
            && let Ok((closed, true)) = prune_tabs(&mut workspace).await
            && !workspace.tabs.is_empty()
        {
            let session = workspace.tabs[workspace.active].session_id.clone();
            let page = Page { cdp: &chrome.cdp, session: &session, end, viewport: workspace.viewport };
            let mut state = page.settle(page.left()).await?;
            state["active_tab_closed"] = json!(true);
            state["closed_tabs"] = json!(closed);
            state["note"] = json!(format!("{action} 之后网页关闭了自己的标签页，已切到剩余的标签页 {}", workspace.active));
            result = Ok(state);
        }
        if result.is_ok() && matches!(action, "click" | "press" | "type") {
            match adopt_opened_tab(&mut workspace).await {
                Ok(Some(opened)) => {
                    let session = workspace.tabs[workspace.active].session_id.clone();
                    let page = Page { cdp: &chrome.cdp, session: &session, end, viewport: workspace.viewport };
                    let state = page.settle(page.left()).await;
                    if let (Ok(Value::Object(map)), Ok(Value::Object(state))) = (result.as_mut(), state) {
                        map.extend(state);
                        map.insert("opened_tab".into(), opened);
                    }
                }
                Ok(None) => {}
                Err(error) => result = Err(error),
            }
        }
        let active = workspace.tabs[workspace.active].session_id.clone();
        workspace.last_used = Instant::now();
        if let Ok(Value::Object(map)) = result.as_mut() {
            map.extend(extra);
            map.insert("host".into(), json!("headless"));
            map.insert("tab".into(), json!({"index": workspace.active, "count": workspace.tabs.len()}));
            let mut dialogs = chrome.cdp.take_dialogs(&session);
            if active != session { dialogs.extend(chrome.cdp.take_dialogs(&active)); }
            if !dialogs.is_empty() { map.insert("dialogs".into(), json!(dialogs)); }
        }
        result
    }
}

/// A tab the active page opened itself (target=_blank, window.open) becomes
/// the active tab, as it would in a desktop browser.
async fn adopt_opened_tab(workspace: &mut Workspace) -> Result<Option<Value>, Value> {
    let opener = workspace.tabs[workspace.active].target_id.clone();
    let targets = workspace.chrome.cdp.call(None, "Target.getTargets", json!({}), CALL_TIMEOUT).await?;
    let Some(opened) = targets["targetInfos"].as_array().into_iter().flatten().find(|info| info["type"] == "page"
        && info["openerId"] == opener.as_str() && info["browserContextId"] == workspace.context_id.as_str()
        && !workspace.tabs.iter().any(|tab| info["targetId"] == tab.target_id.as_str())).cloned() else { return Ok(None) };
    if workspace.tabs.len() >= MAX_TABS {
        let oldest = (0..workspace.tabs.len()).find(|index| *index != workspace.active).unwrap_or(0);
        let tab = workspace.tabs.remove(oldest);
        if workspace.active > oldest { workspace.active -= 1; }
        let _ = workspace.chrome.cdp.call(None, "Target.closeTarget", json!({"targetId": tab.target_id}), CALL_TIMEOUT).await;
    }
    let tab = prepare_tab(&workspace.chrome, opened["targetId"].as_str().unwrap_or("").to_owned(), workspace.viewport).await?;
    workspace.tabs.push(tab);
    workspace.active = workspace.tabs.len() - 1;
    Ok(Some(json!({"url": opened["url"], "title": opened["title"], "index": workspace.active})))
}


async fn evaluate(cdp: &Cdp, session: &str, expression: String, timeout: Duration) -> Result<Value, Value> {
    let reply = cdp.call(Some(session), "Runtime.evaluate",
        json!({"expression": expression, "awaitPromise": true, "returnByValue": true, "userGesture": true}), timeout).await?;
    if let Some(details) = reply.get("exceptionDetails").filter(|value| !value.is_null()) {
        let text = details["exception"]["description"].as_str().or(details["text"].as_str()).unwrap_or("脚本异常");
        return Err(failure("chat.browser.script_failed", format!("页面脚本执行失败：{text}"), "runtime", false));
    }
    Ok(reply["result"]["value"].clone())
}

async fn state(cdp: &Cdp, session: &str) -> Result<Value, Value> {
    let history = cdp.call(Some(session), "Page.getNavigationHistory", json!({}), CALL_TIMEOUT).await?;
    let index = history["currentIndex"].as_i64().unwrap_or(0);
    let entries = history["entries"].as_array().map(Vec::len).unwrap_or(0) as i64;
    let entry = &history["entries"][index as usize];
    let page = evaluate(cdp, session, "({url: location.href, title: document.title, ready: document.readyState})".into(),
        Duration::from_secs(3)).await.ok();
    let url = page.as_ref().and_then(|page| page["url"].as_str()).or(entry["url"].as_str()).unwrap_or("");
    let title = page.as_ref().and_then(|page| page["title"].as_str()).or(entry["title"].as_str()).unwrap_or("");
    let loading = page.as_ref().is_none_or(|page| page["ready"] != "complete");
    Ok(json!({"url": url, "title": title, "loading": loading, "can_go_back": index > 0, "can_go_forward": index + 1 < entries}))
}

fn evaluate_script(expression: &str) -> String {
    format!(r#"(async () => {{
    const value = await ({expression}
    );
    const seen = new WeakSet();
    const json = JSON.stringify(value === undefined ? null : value, (key, item) => {{
      if (typeof item === 'bigint') return item.toString();
      if (typeof item === 'function') return '[function ' + (item.name || 'anonymous') + ']';
      if (item instanceof Element) return '<' + item.tagName.toLowerCase() + (item.id ? '#' + item.id : '') + '>';
      if (item && typeof item === 'object') {{ if (seen.has(item)) return '[circular]'; seen.add(item); }}
      return item;
    }});
    return {{ type: value === null ? 'null' : Array.isArray(value) ? 'array' : typeof value, json: json ?? 'null' }};
  }})()"#)
}

/// (key, code, virtual key code, text) for a KeyboardEvent.key-style name.
fn key_definition(key: &str) -> Option<(String, String, i64, Option<String>)> {
    let named = |name: &str, code: &str, vk: i64, text: Option<&str>| Some((name.to_owned(), code.to_owned(), vk, text.map(str::to_owned)));
    match key {
        "Enter" | "Return" => named("Enter", "Enter", 13, Some("\r")),
        "Tab" => named("Tab", "Tab", 9, None),
        "Escape" | "Esc" => named("Escape", "Escape", 27, None),
        "Backspace" => named("Backspace", "Backspace", 8, None),
        "Delete" => named("Delete", "Delete", 46, None),
        "Insert" => named("Insert", "Insert", 45, None),
        "ArrowUp" | "Up" => named("ArrowUp", "ArrowUp", 38, None),
        "ArrowDown" | "Down" => named("ArrowDown", "ArrowDown", 40, None),
        "ArrowLeft" | "Left" => named("ArrowLeft", "ArrowLeft", 37, None),
        "ArrowRight" | "Right" => named("ArrowRight", "ArrowRight", 39, None),
        "Home" => named("Home", "Home", 36, None),
        "End" => named("End", "End", 35, None),
        "PageUp" => named("PageUp", "PageUp", 33, None),
        "PageDown" => named("PageDown", "PageDown", 34, None),
        "Space" | " " => named(" ", "Space", 32, Some(" ")),
        _ => {
            if let Some(number) = key.strip_prefix('F').and_then(|rest| rest.parse::<i64>().ok()).filter(|n| (1..=12).contains(n)) {
                return named(key, key, 111 + number, None);
            }
            let mut chars = key.chars();
            let (Some(ch), None) = (chars.next(), chars.next()) else { return None };
            let (code, vk) = if ch.is_ascii_alphabetic() {
                (format!("Key{}", ch.to_ascii_uppercase()), ch.to_ascii_uppercase() as i64)
            } else if ch.is_ascii_digit() {
                (format!("Digit{ch}"), ch as i64)
            } else {
                (String::new(), 0)
            };
            Some((ch.to_string(), code, vk, Some(ch.to_string())))
        }
    }
}

struct Page<'a> {
    cdp: &'a Cdp,
    session: &'a str,
    end: Instant,
    viewport: (u32, u32),
}

impl Page<'_> {
    fn left(&self) -> Duration {
        self.end.saturating_duration_since(Instant::now()).max(Duration::from_millis(200))
    }

    async fn call(&self, method: &str, params: Value) -> Result<Value, Value> {
        self.cdp.call(Some(self.session), method, params, self.left().min(CALL_TIMEOUT)).await
    }

    async fn run_fn(&self, name: &str, argument: Value) -> Result<Value, Value> {
        evaluate(self.cdp, self.session, format!("(() => {{\n{PAGE_JS}\nconst input = {argument};\n\
            if (input && typeof input === 'object' && 'interactive' in input) input.interactive = INTERACTIVE;\n\
            return {name}(input);\n}})()"), self.left().min(CALL_TIMEOUT)).await
    }

    async fn state(&self) -> Result<Value, Value> {
        state(self.cdp, self.session).await
    }

    async fn settle(&self, limit: Duration) -> Result<Value, Value> {
        let end = Instant::now() + limit.min(self.left());
        loop {
            let state = self.state().await?;
            if state["loading"] != true || Instant::now() >= end { return Ok(state); }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    async fn navigate(&self, url: &str) -> Result<Value, Value> {
        let reply = self.call("Page.navigate", json!({"url": url})).await?;
        if let Some(error) = reply["errorText"].as_str().filter(|text| !text.is_empty()) {
            return Err(failure("chat.browser.load_failed", format!("网页加载失败：{error}"), "runtime", true));
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
        self.settle(self.left()).await
    }

    async fn target(&self, input: &Value) -> Result<Value, Value> {
        let found = self.run_fn("pageTarget", json!({"ref": input["ref"], "selector": input["selector"], "text": input["text"],
            "interactive": Value::Null})).await?;
        if found.is_null() || found["missing"] == true {
            return Err(failure("chat.browser.target_missing", "页面上找不到该元素；请重新读取 elements 获取最新 ref", "validation", false));
        }
        Ok(found)
    }

    async fn click(&self, input: &Value) -> Result<Value, Value> {
        let mut found = json!({});
        let point = if input["point"].is_object() { input["point"].clone() } else {
            found = self.target(input).await?;
            json!({"x": found["x"], "y": found["y"]})
        };
        let x = point["x"].as_f64().unwrap_or(-1.0).round();
        let y = point["y"].as_f64().unwrap_or(-1.0).round();
        if x < 0.0 || y < 0.0 || x >= self.viewport.0 as f64 || y >= self.viewport.1 as f64 {
            return Err(failure("chat.browser.point_outside", format!("点击位置 ({x}, {y}) 不在 {}x{} 视口内", self.viewport.0, self.viewport.1), "validation", false));
        }
        self.call("Input.dispatchMouseEvent", json!({"type": "mouseMoved", "x": x, "y": y})).await?;
        self.call("Input.dispatchMouseEvent", json!({"type": "mousePressed", "x": x, "y": y, "button": "left", "buttons": 1, "clickCount": 1})).await?;
        self.call("Input.dispatchMouseEvent", json!({"type": "mouseReleased", "x": x, "y": y, "button": "left", "buttons": 0, "clickCount": 1})).await?;
        tokio::time::sleep(Duration::from_millis(400)).await;
        self.settle(self.left()).await?;
        Ok(json!({"x": x, "y": y, "tag": found["tag"], "text": found["text"], "covered_by": found["covered_by"]}))
    }

    async fn press(&self, key: &str, modifiers: &[String], repeat: u64) -> Result<(), Value> {
        let Some((key, code, vk, text)) = key_definition(key) else {
            return Err(failure("chat.browser.key_invalid", format!("不支持的按键 {key:?}；使用 KeyboardEvent.key 名称或单个字符"), "validation", false));
        };
        let mask = modifiers.iter().map(|name| match name.as_str() { "alt" => 1, "control" => 2, "meta" => 4, "shift" => 8, _ => 0 }).sum::<i64>();
        let text = text.filter(|_| mask & (2 | 4) == 0).map(|text| if mask & 8 != 0 { text.to_uppercase() } else { text });
        for _ in 0..repeat {
            let mut down = json!({"type": if text.is_some() { "keyDown" } else { "rawKeyDown" }, "key": key, "code": code,
                "windowsVirtualKeyCode": vk, "nativeVirtualKeyCode": vk, "modifiers": mask});
            if let Some(text) = &text { down["text"] = json!(text); down["unmodifiedText"] = json!(text); }
            self.call("Input.dispatchKeyEvent", down).await?;
            self.call("Input.dispatchKeyEvent", json!({"type": "keyUp", "key": key, "code": code,
                "windowsVirtualKeyCode": vk, "nativeVirtualKeyCode": vk, "modifiers": mask})).await?;
        }
        Ok(())
    }

    async fn run(&self, action: &str, input: &Value) -> Result<Value, Value> {
        let merge = |mut base: Value, extra: Value| {
            if let (Some(base), Some(extra)) = (base.as_object_mut(), extra.as_object()) {
                for (key, value) in extra { if !value.is_null() { base.insert(key.clone(), value.clone()); } }
            }
            base
        };
        match action {
            "open" => self.navigate(input["url"].as_str().unwrap_or("")).await,
            "navigate" => {
                if let Some(url) = input["url"].as_str() { return self.navigate(url).await; }
                let direction = input["action"].as_str().unwrap_or("");
                if direction == "reload" {
                    self.call("Page.reload", json!({})).await?;
                } else {
                    let history = self.call("Page.getNavigationHistory", json!({})).await?;
                    let index = history["currentIndex"].as_i64().unwrap_or(0) + if direction == "back" { -1 } else { 1 };
                    let Some(entry) = history["entries"].as_array().and_then(|rows| usize::try_from(index).ok().and_then(|i| rows.get(i))) else {
                        return Err(failure("chat.browser.no_history", if direction == "back" { "没有可后退的页面" } else { "没有可前进的页面" }, "state", false));
                    };
                    self.call("Page.navigateToHistoryEntry", json!({"entryId": entry["id"]})).await?;
                }
                tokio::time::sleep(Duration::from_millis(300)).await;
                self.settle(self.left()).await
            }
            "wait" => {
                let settled = self.settle(self.left()).await?;
                let want = input["selector"].as_str().map(|selector| json!({"selector": selector, "text": Value::Null}))
                    .or_else(|| input["text"].as_str().map(|text| json!({"selector": Value::Null, "text": text})));
                let Some(want) = want else { return Ok(settled) };
                let gone = input["gone"] == true;
                let end = Instant::now() + Duration::from_millis(input["timeout_ms"].as_u64().unwrap_or(10_000)).min(self.left());
                loop {
                    let present = match self.run_fn("pagePresent", want.clone()).await {
                        Ok(value) => Some(value.as_bool().ok_or_else(|| failure("chat.browser.headless.protocol_failed", "页面存在性检查没有返回布尔值", "runtime", false))?),
                        Err(error) if error["code"] == "chat.browser.headless.protocol_failed" && error["protocol_code"] == -32000 => None,
                        Err(error) => return Err(error),
                    };
                    if present.is_some_and(|present| present != gone) {
                        return Ok(merge(self.state().await?, json!({"waited_for": input["selector"].as_str().or(input["text"].as_str()), "gone": gone})));
                    }
                    if Instant::now() >= end {
                        return Err(failure("chat.browser.wait_timeout", format!("等待{}超时：{}", if gone { "消失" } else { "出现" },
                            input["selector"].as_str().or(input["text"].as_str()).unwrap_or("")), "timeout", true));
                    }
                    tokio::time::sleep(Duration::from_millis(200)).await;
                }
            }
            "read" => {
                self.settle(self.left()).await?;
                let mode = input["mode"].as_str().unwrap_or("text");
                let read = self.run_fn("pageRead", json!({"mode": mode, "selector": input["selector"], "interactive": Value::Null})).await?;
                if read.is_null() || read["missing"] == true {
                    return Err(failure("chat.browser.target_missing", format!("找不到选择器 {} 对应的元素", input["selector"].as_str().unwrap_or("")), "validation", false));
                }
                let offset = input["offset"].as_u64().unwrap_or(0) as usize;
                let limit = input["limit"].as_u64().unwrap_or(1).max(1) as usize;
                let state = self.state().await?;
                if mode == "elements" {
                    let rows = read["elements"].as_array().cloned().unwrap_or_default();
                    let end = rows.len().min(offset + limit);
                    let page: Vec<Value> = rows.get(offset.min(rows.len())..end).unwrap_or(&[]).to_vec();
                    Ok(merge(state, json!({"mode": mode, "offset": offset, "total_elements": rows.len(),
                        "next_offset": if end < rows.len() { json!(end) } else { Value::Null }, "elements": page})))
                } else {
                    let content: Vec<u16> = read["content"].as_str().unwrap_or("").encode_utf16().collect();
                    let end = content.len().min(offset + limit);
                    let slice = String::from_utf16_lossy(content.get(offset.min(content.len())..end).unwrap_or(&[]));
                    let mut out = merge(state, json!({"mode": mode, "offset": offset, "total_chars": content.len(), "content": slice}));
                    out["next_offset"] = if end < content.len() { json!(end) } else { Value::Null };
                    Ok(out)
                }
            }
            "click" => {
                let clicked = self.click(input).await?;
                Ok(merge(self.state().await?, json!({"clicked": clicked})))
            }
            "type" => {
                let target = if input["ref"].is_null() && input["selector"].is_null() { None } else {
                    Some(self.click(&json!({"ref": input["ref"], "selector": input["selector"]})).await?)
                };
                let clear = input["clear"] == true;
                let focused = self.run_fn("pageFocused", json!({"caretToEnd": !clear})).await?;
                if focused["editable"] != true {
                    return Err(failure("chat.browser.not_editable", "当前焦点不是可输入的元素；请提供 ref 或 selector", "validation", false));
                }
                let text = input["text"].as_str().unwrap_or("");
                if Instant::now() >= self.end {
                    return Err(failure("chat.browser.input_timeout", "输入期限已到，未输入文字。", "runtime", false));
                }
                let inserted = self.run_fn("pageInsert", json!({"clear": clear, "text": text, "submit": input["submit"]})).await?;
                if inserted == false {
                    return Err(failure("chat.browser.type_failed", "页面拒绝了文字输入", "runtime", false));
                }
                if let Some(selector) = input["submit_when"].as_str() {
                    while Instant::now() < self.end {
                        if self.run_fn("pagePresent", json!({"selector": selector})).await? == true { break; }
                        let remaining = self.end.saturating_duration_since(Instant::now());
                        tokio::time::sleep(Duration::from_millis(50).min(remaining)).await;
                    }
                }
                let typed = self.run_fn("pageFocused", json!({"caretToEnd": false, "expectedRef": inserted.as_str()})).await?;
                let state = if input["submit"] == true {
                    if typed.is_null() {
                        return Err(failure("chat.browser.submit_focus_changed", "输入框焦点已改变，未发送 Enter。", "runtime", false));
                    }
                    if Instant::now() >= self.end {
                        return Err(failure("chat.browser.submit_timeout", "文字已输入，提交期限已到，未发送 Enter。", "runtime", false));
                    }
                    self.press("Enter", &[], 1).await?;
                    self.settle(self.left()).await?
                } else {
                    self.state().await?
                };
                Ok(merge(state, json!({"typed_chars": text.encode_utf16().count(), "cleared": clear, "submitted": input["submit"] == true,
                    "target": target.map(|t| json!({"tag": t["tag"], "text": t["text"]})), "focused": if typed.is_null() { focused } else { typed }})))
            }
            "press" => {
                let target = if input["ref"].is_null() && input["selector"].is_null() { None } else {
                    Some(self.click(&json!({"ref": input["ref"], "selector": input["selector"]})).await?)
                };
                let modifiers: Vec<String> = input["modifiers"].as_array().into_iter().flatten().filter_map(Value::as_str).map(str::to_owned).collect();
                let repeat = input["repeat"].as_u64().unwrap_or(1).clamp(1, 50);
                self.press(input["key"].as_str().unwrap_or(""), &modifiers, repeat).await?;
                tokio::time::sleep(Duration::from_millis(250)).await;
                let state = self.settle(self.left()).await?;
                let focused = self.run_fn("pageFocused", json!({"caretToEnd": false})).await?;
                Ok(merge(state, json!({"pressed": input["key"], "modifiers": modifiers, "repeat": repeat, "focused": focused,
                    "target": target.map(|t| json!({"tag": t["tag"], "text": t["text"]}))})))
            }
            "scroll" => {
                let target = if input["ref"].is_null() && input["selector"].is_null() { None } else { Some(self.target(input).await?) };
                tokio::time::sleep(Duration::from_millis(150)).await;
                let position = self.run_fn("pageScroll", json!({"direction": input["direction"], "amount": input["amount"], "to": input["to"]})).await?;
                let mut out = merge(self.state().await?, position);
                if let Some(target) = target { out["target"] = json!({"tag": target["tag"], "text": target["text"]}); }
                Ok(out)
            }
            "evaluate" => {
                let result = evaluate(self.cdp, self.session, evaluate_script(input["expression"].as_str().unwrap_or("")), self.left())
                    .await.map_err(|error| failure("chat.browser.evaluate_failed",
                        format!("表达式执行失败：{}", error["message"].as_str().unwrap_or("")), "runtime", false))?;
                let Some(json_text) = result["json"].as_str() else {
                    return Err(failure("chat.browser.evaluate_failed", "表达式没有返回可读取的结果", "runtime", false));
                };
                let chars = json_text.encode_utf16().count();
                if chars > EVALUATE_MAX_CHARS {
                    return Err(failure("chat.browser.evaluate_too_large",
                        format!("结果有 {chars} 个字符，超过 {EVALUATE_MAX_CHARS}；请缩小表达式返回的数据"), "validation", false));
                }
                let value: Value = serde_json::from_str(json_text).unwrap_or(Value::Null);
                Ok(merge(self.state().await?, json!({"result_type": result["type"], "result": value})))
            }
            "logs" => {
                let kind = input["kind"].as_str().unwrap_or("console");
                let logs = self.run_fn(if kind == "network" { "pageNetwork" } else { "pageConsole" }, Value::Null).await?;
                if logs.is_null() {
                    return Err(failure("chat.browser.logs_unavailable", "此页面没有记录日志，请重新打开网页", "state", true));
                }
                if let Some(message) = logs["error"]["message"].as_str() {
                    return Err(failure("chat.browser.logs_capture_limit", message, "resource", false));
                }
                let rows: Vec<Value> = logs["rows"].as_array().into_iter().flatten().filter(|row| match kind {
                    "console" => input["level"].as_str().is_none_or(|level| row["level"] == level),
                    _ => input["failed_only"] != true || row["ok"] != true,
                }).cloned().collect();
                let offset = input["offset"].as_u64().unwrap_or(0) as usize;
                let limit = input["limit"].as_u64().unwrap_or(100).max(1) as usize;
                let end = rows.len().min(offset + limit);
                let page: Vec<Value> = rows.get(offset.min(rows.len())..end).unwrap_or(&[]).to_vec();
                let mut out = merge(self.state().await?, json!({"kind": kind, "total": rows.len(), "dropped": logs["dropped"], "offset": offset, "entries": page}));
                out["next_offset"] = if end < rows.len() { json!(end) } else { Value::Null };
                Ok(out)
            }
            "screenshot" => {
                let state = self.settle(self.left()).await?;
                let shot = self.call("Page.captureScreenshot", json!({"format": "png"})).await?;
                Ok(merge(state, json!({"width": self.viewport.0, "height": self.viewport.1, "data": shot["data"]})))
            }
            _ => Err(failure("chat.browser.arguments_invalid", format!("未知浏览器操作 {action:?}"), "validation", false)),
        }
    }
}
