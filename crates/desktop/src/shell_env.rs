//! Login-shell environment discovery, persistent cache, and thread-safe host
//! process environment storage shared by `muteki-desktop` and `muteki-server`.
//!
//! Global process environment is never mutated via `std::env::set_var` after
//! threads start (undefined behavior in Rust 2024). Instead, allowlisted login
//! shell variables live in a `RwLock` store and are injected into child
//! processes explicitly after resolving bare command names against the live
//! `PATH`.

use serde_json::{Map, Value, json};
use std::{
    collections::HashMap,
    ffi::{OsStr, OsString},
    fs::OpenOptions,
    io::{Read, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        OnceLock, RwLock,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

pub const LOGIN_SHELL_ENV_NAMES: &[&str] = &[
    "PATH",
    "__MISE_DIFF",
    "HOMEBREW_PREFIX",
    "HOMEBREW_CELLAR",
    "HOMEBREW_REPOSITORY",
    "SSH_AUTH_SOCK",
    "SSL_CERT_FILE",
    "SSL_CERT_DIR",
    "NODE_EXTRA_CA_CERTS",
    "JAVA_HOME",
    "FNM_DIR",
    "FNM_MULTISHELL_PATH",
    "NVM_DIR",
    "VOLTA_HOME",
    "BUN_INSTALL",
    "PNPM_HOME",
    "CARGO_HOME",
    "PYENV_ROOT",
    "XDG_CONFIG_HOME",
    "XDG_DATA_HOME",
    "XDG_RUNTIME_DIR",
    "XDG_CURRENT_DESKTOP",
    "XDG_SESSION_DESKTOP",
    "XDG_SESSION_TYPE",
    "DISPLAY",
    "WAYLAND_DISPLAY",
    "DBUS_SESSION_BUS_ADDRESS",
    "LANG",
    "LC_ALL",
    "LC_CTYPE",
];

const CACHE_FILE_NAME: &str = "shell-environment-cache.json";
const CACHE_VERSION: u64 = 1;
const INITIAL_PROBE_TIMEOUT: Duration = Duration::from_secs(5);
const RETRY_PROBE_TIMEOUT: Duration = Duration::from_secs(20);
#[cfg(target_os = "macos")]
const LAUNCHCTL_TIMEOUT: Duration = Duration::from_secs(2);

static TMP_SEQ: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Debug)]
struct ShellEnvState {
    vars: HashMap<String, String>,
    source: String,
    discovered: bool,
    updated_at_ms: u64,
    generation: u64,
}

#[derive(Clone, Debug)]
pub struct RefreshOutcome {
    pub changed: bool,
    pub discovered: bool,
    pub source: String,
}

enum ProbeExecResult {
    Output(Vec<u8>),
    TimedOut,
    Failed,
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

fn is_allowlisted(name: &str) -> bool {
    LOGIN_SHELL_ENV_NAMES.contains(&name)
}

fn ensure_locale_fallback(vars: &mut HashMap<String, String>) {
    #[cfg(target_os = "macos")]
    {
        let has_locale = ["LANG", "LC_ALL", "LC_CTYPE"]
            .iter()
            .any(|key| vars.get(*key).is_some_and(|v| !v.trim().is_empty()));
        if !has_locale {
            vars.insert("LC_CTYPE".to_owned(), "en_US.UTF-8".to_owned());
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = vars;
    }
}

fn from_inherited() -> HashMap<String, String> {
    let mut vars = HashMap::new();
    for &name in LOGIN_SHELL_ENV_NAMES {
        if let Ok(value) = std::env::var(name)
            && !value.is_empty()
        {
            vars.insert(name.to_owned(), value);
        }
    }
    ensure_locale_fallback(&mut vars);
    vars
}

fn store() -> &'static RwLock<ShellEnvState> {
    static STORE: OnceLock<RwLock<ShellEnvState>> = OnceLock::new();
    STORE.get_or_init(|| {
        RwLock::new(ShellEnvState {
            vars: from_inherited(),
            source: "inherited".to_owned(),
            discovered: false,
            updated_at_ms: now_ms(),
            generation: 0,
        })
    })
}

pub fn merge_paths(preferred: Option<&str>, fallback: Option<&str>) -> Option<String> {
    let mut unique: Vec<PathBuf> = Vec::new();
    for source in [preferred, fallback].into_iter().flatten() {
        for entry in std::env::split_paths(source) {
            if entry.as_os_str().is_empty() {
                continue;
            }
            if !unique.contains(&entry) {
                unique.push(entry);
            }
        }
    }
    if unique.is_empty() {
        return None;
    }
    std::env::join_paths(unique).ok()?.into_string().ok()
}

#[cfg(unix)]
fn resolved_user_shell() -> String {
    if let Ok(shell) = std::env::var("SHELL")
        && shell.starts_with('/')
        && !shell.is_empty()
    {
        return shell;
    }
    let uid = unsafe { libc::geteuid() };
    let mut pwd = std::mem::MaybeUninit::<libc::passwd>::uninit();
    let mut buf = vec![0u8; 4096];
    let mut result: *mut libc::passwd = std::ptr::null_mut();
    let rc = unsafe {
        libc::getpwuid_r(
            uid,
            pwd.as_mut_ptr(),
            buf.as_mut_ptr().cast::<libc::c_char>(),
            buf.len(),
            &mut result,
        )
    };
    if rc == 0 && !result.is_null() {
        let pw = unsafe { pwd.assume_init() };
        if !pw.pw_shell.is_null() {
            let shell = unsafe { std::ffi::CStr::from_ptr(pw.pw_shell) }
                .to_string_lossy()
                .into_owned();
            if shell.starts_with('/') && !shell.is_empty() {
                return shell;
            }
        }
    }
    if cfg!(target_os = "macos") {
        "/bin/zsh".to_owned()
    } else {
        "/bin/sh".to_owned()
    }
}

#[cfg(not(unix))]
fn resolved_user_shell() -> String {
    std::env::var("COMSPEC").unwrap_or_else(|_| "cmd.exe".to_owned())
}

fn cache_key() -> Value {
    json!([
        std::env::consts::OS,
        std::env::var("HOME").unwrap_or_default(),
        resolved_user_shell(),
        std::env::var("ZDOTDIR").unwrap_or_default(),
    ])
}

fn write_cache(state_dir: &Path, vars: &HashMap<String, String>) {
    if let Err(error) = std::fs::create_dir_all(state_dir) {
        let _ = error;
        return;
    }
    let mut env_map = Map::new();
    for &name in LOGIN_SHELL_ENV_NAMES {
        if let Some(value) = vars.get(name)
            && !value.is_empty()
        {
            env_map.insert(name.to_owned(), Value::String(value.clone()));
        }
    }
    let payload = json!({
        "version": CACHE_VERSION,
        "key": cache_key(),
        "updated_at_ms": now_ms(),
        "env": env_map,
    });
    let Ok(bytes) = serde_json::to_vec_pretty(&payload) else {
        return;
    };
    let seq = TMP_SEQ.fetch_add(1, Ordering::Relaxed);
    let tmp = state_dir.join(format!(
        ".shell-environment-cache.{}.{}.tmp",
        std::process::id(),
        seq
    ));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let Ok(mut file) = options.open(&tmp) else {
        return;
    };
    if file.write_all(&bytes).is_err() || file.sync_all().is_err() {
        let _ = std::fs::remove_file(&tmp);
        return;
    }
    drop(file);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600));
    }
    let target = state_dir.join(CACHE_FILE_NAME);
    if std::fs::rename(&tmp, &target).is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
}

fn build_merged_vars(
    override_env: &HashMap<String, String>,
    fallback_path_env: Option<&str>,
) -> HashMap<String, String> {
    let mut merged = from_inherited();
    let inherited_path = merged.get("PATH").cloned();

    if let Some(override_path) = override_env.get("PATH").filter(|v| !v.is_empty()) {
        if let Some(joined) = merge_paths(Some(override_path), inherited_path.as_deref()) {
            merged.insert("PATH".to_owned(), joined);
        }
        if let Some(mise_diff) = override_env.get("__MISE_DIFF").filter(|v| !v.is_empty()) {
            merged.insert("__MISE_DIFF".to_owned(), mise_diff.clone());
        } else {
            merged.remove("__MISE_DIFF");
        }
    } else if let Some(fallback_path) = fallback_path_env.filter(|v| !v.is_empty()) {
        if let Some(joined) = merge_paths(Some(fallback_path), inherited_path.as_deref()) {
            merged.insert("PATH".to_owned(), joined);
        }
        merged.remove("__MISE_DIFF");
    }

    for (key, value) in override_env {
        if key == "PATH" || key == "__MISE_DIFF" {
            continue;
        }
        if is_allowlisted(key) && !value.is_empty() {
            merged.insert(key.clone(), value.clone());
        }
    }
    ensure_locale_fallback(&mut merged);
    merged
}

fn update_store(
    next_vars: HashMap<String, String>,
    source: &str,
    discovered: bool,
) -> (bool, u64) {
    let mut guard = store().write().unwrap_or_else(|poison| poison.into_inner());
    let changed = guard.vars != next_vars;
    if changed {
        guard.vars = next_vars;
        guard.generation = guard.generation.saturating_add(1);
    }
    guard.source = source.to_owned();
    guard.discovered = guard.discovered || discovered;
    guard.updated_at_ms = now_ms();
    (changed, guard.generation)
}

pub fn init_from_cache(state_dir: &Path) -> bool {
    {
        let guard = store().read().unwrap_or_else(|poison| poison.into_inner());
        if guard.discovered {
            return false;
        }
    }
    let path = state_dir.join(CACHE_FILE_NAME);
    let Ok(raw) = std::fs::read(&path) else {
        return false;
    };
    let Ok(doc) = serde_json::from_slice::<Value>(&raw) else {
        return false;
    };
    if doc.get("version").and_then(Value::as_u64) != Some(CACHE_VERSION) {
        return false;
    }
    if doc.get("key") != Some(&cache_key()) {
        return false;
    }
    let Some(env_obj) = doc.get("env").and_then(Value::as_object) else {
        return false;
    };
    let mut cached = HashMap::new();
    for (key, value) in env_obj {
        if is_allowlisted(key)
            && let Some(text) = value.as_str()
            && !text.is_empty()
        {
            cached.insert(key.clone(), text.to_owned());
        }
    }
    if cached.is_empty() {
        return false;
    }
    let next_vars = build_merged_vars(&cached, None);
    let (changed, _) = update_store(next_vars, "cache", false);
    changed
}

pub fn apply_external_env(
    env_obj: &Map<String, Value>,
    source: &str,
    state_dir: Option<&Path>,
) -> bool {
    let mut external = HashMap::new();
    for (key, value) in env_obj {
        if is_allowlisted(key)
            && let Some(text) = value.as_str()
            && !text.is_empty()
        {
            external.insert(key.clone(), text.to_owned());
        }
    }
    let next_vars = build_merged_vars(&external, None);
    if let Some(dir) = state_dir {
        write_cache(dir, &next_vars);
    }
    let (changed, _) = update_store(next_vars, source, true);
    changed
}

#[cfg(unix)]
fn build_capture_command(shell_path: &str, names: &[&str]) -> String {
    let base = Path::new(shell_path)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("sh");
    let mut script = String::new();
    match base {
        "nu" => {
            for &name in names {
                let value_expr = if name == "PATH" {
                    r#"if ($env.PATH | describe | str starts-with "list") { ($env.PATH | str join (char esep)) } else { ($env.PATH | to text) }"#
                } else {
                    "($env.<NAME> | to text)"
                }
                .replace("<NAME>", name);
                script.push_str(&format!(
                    "if (\"{name}\" in $env) {{ print \"__MUTEKI_ENV_{name}_START__\"; print ({value_expr}); print \"__MUTEKI_ENV_{name}_END__\" }}; "
                ));
            }
        }
        "fish" => {
            for &name in names {
                script.push_str(&format!(
                    "if set -q {name}; printf '__MUTEKI_ENV_{name}_START__\\n'; printenv {name}; printf '\\n__MUTEKI_ENV_{name}_END__\\n'; end; "
                ));
            }
        }
        _ => {
            script.push_str("(trap - DEBUG) 2>/dev/null && trap - DEBUG; ");
            for &name in names {
                script.push_str(&format!(
                    "if printenv {name} >/dev/null 2>&1; then printf '__MUTEKI_ENV_{name}_START__\\n'; printenv {name}; printf '\\n__MUTEKI_ENV_{name}_END__\\n'; fi; "
                ));
            }
        }
    }
    script
}

#[cfg(unix)]
fn extract_captured_values(raw: &[u8], names: &[&str]) -> HashMap<String, String> {
    let text = String::from_utf8_lossy(raw);
    let mut out = HashMap::new();
    for &name in names {
        let start_marker = format!("__MUTEKI_ENV_{name}_START__");
        let end_marker = format!("__MUTEKI_ENV_{name}_END__");
        let Some(start_idx) = text.rfind(&start_marker) else {
            continue;
        };
        let after_start = &text[start_idx + start_marker.len()..];
        let Some(end_rel) = after_start.find(&end_marker) else {
            continue;
        };
        let value = after_start[..end_rel].trim_matches(&['\r', '\n'][..]);
        if !value.is_empty() {
            out.insert(name.to_owned(), value.to_owned());
        }
    }
    out
}

#[cfg(unix)]
fn kill_process_group(pid: u32) {
    if let Ok(pgid) = i32::try_from(pid)
        && pgid > 0
    {
        unsafe {
            libc::kill(-pgid, libc::SIGKILL);
        }
    }
}

#[cfg(unix)]
fn run_bounded_command(program: &OsStr, args: &[&str], timeout: Duration) -> ProbeExecResult {
    use std::os::unix::process::CommandExt;

    #[allow(clippy::disallowed_methods)]
    let mut cmd = Command::new(program);
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .process_group(0);

    let Ok(mut child) = cmd.spawn() else {
        return ProbeExecResult::Failed;
    };
    let pid = child.id();
    let Some(mut stdout) = child.stdout.take() else {
        kill_process_group(pid);
        let _ = child.kill();
        let _ = child.wait();
        return ProbeExecResult::Failed;
    };

    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stdout.read_to_end(&mut buf);
        let _ = tx.send(buf);
    });

    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let bytes = match rx.recv_timeout(Duration::from_millis(200)) {
                    Ok(bytes) => bytes,
                    Err(_) => {
                        kill_process_group(pid);
                        rx.recv_timeout(Duration::from_millis(100)).unwrap_or_default()
                    }
                };
                if status.success() || !bytes.is_empty() {
                    return ProbeExecResult::Output(bytes);
                }
                return ProbeExecResult::Failed;
            }
            Ok(None) => {
                if Instant::now() >= deadline {
                    kill_process_group(pid);
                    let _ = child.kill();
                    let _ = child.wait();
                    return ProbeExecResult::TimedOut;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(_) => {
                kill_process_group(pid);
                let _ = child.kill();
                let _ = child.wait();
                return ProbeExecResult::Failed;
            }
        }
    }
}

#[cfg(target_os = "macos")]
fn launchctl_path_fallback() -> Option<String> {
    let args = ["getenv", "PATH"];
    let ProbeExecResult::Output(bytes) =
        run_bounded_command(OsStr::new("/bin/launchctl"), &args, LAUNCHCTL_TIMEOUT)
    else {
        return None;
    };
    let text = String::from_utf8_lossy(&bytes).trim().to_owned();
    (!text.is_empty()).then_some(text)
}

#[cfg(all(unix, not(target_os = "macos")))]
fn launchctl_path_fallback() -> Option<String> {
    None
}

pub fn refresh_blocking(state_dir: Option<&Path>) -> RefreshOutcome {
    #[cfg(not(unix))]
    {
        let _ = state_dir;
        let guard = store().read().unwrap_or_else(|poison| poison.into_inner());
        return RefreshOutcome {
            changed: false,
            discovered: guard.discovered,
            source: guard.source.clone(),
        };
    }
    #[cfg(unix)]
    {
        let shell = resolved_user_shell();
        let script = build_capture_command(&shell, LOGIN_SHELL_ENV_NAMES);
        let args = ["-ilc", script.as_str()];

        let raw = match run_bounded_command(OsStr::new(&shell), &args, INITIAL_PROBE_TIMEOUT) {
            ProbeExecResult::Output(bytes) => Some(bytes),
            ProbeExecResult::TimedOut => {
                match run_bounded_command(OsStr::new(&shell), &args, RETRY_PROBE_TIMEOUT) {
                    ProbeExecResult::Output(bytes) => Some(bytes),
                    ProbeExecResult::TimedOut | ProbeExecResult::Failed => None,
                }
            }
            ProbeExecResult::Failed => None,
        };

        let captured = raw
            .map(|bytes| extract_captured_values(&bytes, LOGIN_SHELL_ENV_NAMES))
            .unwrap_or_default();

        let fallback_path = if !captured.contains_key("PATH") {
            launchctl_path_fallback()
        } else {
            None
        };

        if captured.is_empty() && fallback_path.is_none() {
            let guard = store().read().unwrap_or_else(|poison| poison.into_inner());
            return RefreshOutcome {
                changed: false,
                discovered: guard.discovered,
                source: guard.source.clone(),
            };
        }

        let source = if captured.contains_key("PATH") || !captured.is_empty() {
            "login_shell"
        } else {
            "launchctl"
        };
        let next_vars = build_merged_vars(&captured, fallback_path.as_deref());
        if let Some(dir) = state_dir {
            write_cache(dir, &next_vars);
        }
        let (changed, _) = update_store(next_vars, source, true);
        RefreshOutcome {
            changed,
            discovered: true,
            source: source.to_owned(),
        }
    }
}

pub fn is_discovered() -> bool {
    store()
        .read()
        .unwrap_or_else(|poison| poison.into_inner())
        .discovered
}

pub fn base_vars() -> Vec<(String, String)> {
    let guard = store().read().unwrap_or_else(|poison| poison.into_inner());
    let mut pairs: Vec<(String, String)> = guard
        .vars
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    pairs.sort_by(|a, b| a.0.cmp(&b.0));
    pairs
}

pub fn path_os() -> Option<OsString> {
    let guard = store().read().unwrap_or_else(|poison| poison.into_inner());
    guard
        .vars
        .get("PATH")
        .filter(|v| !v.is_empty())
        .map(OsString::from)
        .or_else(|| std::env::var_os("PATH"))
}

pub fn path_string() -> String {
    path_os()
        .and_then(|v| v.into_string().ok())
        .unwrap_or_default()
}

pub fn var(name: &str) -> Option<String> {
    if is_allowlisted(name) {
        let guard = store().read().unwrap_or_else(|poison| poison.into_inner());
        if let Some(value) = guard.vars.get(name)
            && !value.is_empty()
        {
            return Some(value.clone());
        }
    }
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

pub fn export_payload() -> Value {
    let guard = store().read().unwrap_or_else(|poison| poison.into_inner());
    let mut env_map = Map::new();
    for &name in LOGIN_SHELL_ENV_NAMES {
        if let Some(value) = guard.vars.get(name)
            && !value.is_empty()
        {
            env_map.insert(name.to_owned(), Value::String(value.clone()));
        }
    }
    json!({
        "source": guard.source,
        "discovered": guard.discovered,
        "generation": guard.generation,
        "env": env_map,
    })
}

pub fn summary() -> Value {
    let guard = store().read().unwrap_or_else(|poison| poison.into_inner());
    let path_entries = guard
        .vars
        .get("PATH")
        .map(|p| {
            std::env::split_paths(p)
                .filter(|d| !d.as_os_str().is_empty())
                .count()
        })
        .unwrap_or(0);
    json!({
        "source": guard.source,
        "discovered": guard.discovered,
        "generation": guard.generation,
        "updated_at_ms": guard.updated_at_ms,
        "path_entries": path_entries,
    })
}

pub fn is_executable_file(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path)
            .map(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    }
    #[cfg(not(unix))]
    {
        path.is_file()
    }
}

pub fn resolve_program_in_path(program: &OsStr, path_env: Option<&OsStr>) -> PathBuf {
    let raw = program.to_string_lossy();
    if raw.is_empty() || raw.contains('/') || raw.contains('\\') {
        return PathBuf::from(program);
    }
    let Some(path_var) = path_env else {
        return PathBuf::from(program);
    };
    #[cfg(windows)]
    let candidate_names: Vec<String> = {
        let mut names = Vec::new();
        if Path::new(raw.as_ref()).extension().is_some() {
            names.push(raw.into_owned());
        } else {
            let pathext =
                std::env::var("PATHEXT").unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".to_owned());
            for ext in pathext.split(';').map(str::trim).filter(|e| !e.is_empty()) {
                names.push(format!("{raw}{ext}"));
            }
            names.push(raw.into_owned());
        }
        names
    };
    for directory in std::env::split_paths(path_var) {
        if !directory.is_absolute() {
            continue;
        }
        #[cfg(windows)]
        {
            for name in &candidate_names {
                let candidate = directory.join(name);
                if is_executable_file(&candidate) {
                    return candidate;
                }
            }
        }
        #[cfg(not(windows))]
        {
            let candidate = directory.join(program);
            if is_executable_file(&candidate) {
                return candidate;
            }
        }
    }
    PathBuf::from(program)
}

pub fn resolve_program(program: &OsStr) -> PathBuf {
    let path = path_os();
    resolve_program_in_path(program, path.as_deref())
}

#[allow(clippy::disallowed_methods)]
pub fn command(program: impl AsRef<OsStr>) -> Command {
    let resolved = resolve_program(program.as_ref());
    let mut cmd = Command::new(resolved);
    for (key, value) in base_vars() {
        cmd.env(key, value);
    }
    cmd
}
