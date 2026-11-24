use super::{ExtError, Result, manifest};
use crate::AppState;
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
    process::Stdio,
    sync::{
        Arc, Weak,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::{Child, ChildStdin, ChildStdout},
    sync::Mutex,
    task::JoinHandle,
};

const MAX_FRAME_BYTES: usize = 4 * 1024 * 1024;

fn runtime_err(code: &str, message: impl Into<String>) -> ExtError {
    ExtError::runtime(code, message)
}

pub struct Host {
    processes: Mutex<HashMap<String, Arc<Mutex<Process>>>>,
    lifecycle: Mutex<()>,
    monitor_started: AtomicBool,
}
impl Default for Host {
    fn default() -> Self {
        Self {
            processes: Mutex::new(HashMap::new()),
            lifecycle: Mutex::new(()),
            monitor_started: AtomicBool::new(false),
        }
    }
}

impl Host {
    pub(super) fn try_begin_monitor(&self) -> bool {
        !self.monitor_started.swap(true, Ordering::SeqCst)
    }
    pub async fn lock_lifecycle(&self) -> tokio::sync::MutexGuard<'_, ()> {
        self.lifecycle.lock().await
    }
    pub(super) async fn get(&self, id: &str) -> Option<Arc<Mutex<Process>>> {
        self.processes.lock().await.get(id).cloned()
    }
    pub(super) async fn activate(
        &self, id: String, process: Process, persist: impl FnOnce() -> Result<Value>,
    ) -> Result<Value> {
        let mut processes = self.processes.lock().await;
        let row = persist()?;
        processes.insert(id, Arc::new(Mutex::new(process)));
        Ok(row)
    }
    pub(super) async fn ids(&self) -> Vec<String> {
        self.processes.lock().await.keys().cloned().collect()
    }
    pub(super) async fn remove(&self, id: &str) -> Option<Arc<Mutex<Process>>> {
        self.processes.lock().await.remove(id)
    }
    pub async fn shutdown(&self) {
        let processes: Vec<_> = self
            .processes
            .lock()
            .await
            .drain()
            .map(|(_, process)| process)
            .collect();
        for process in processes {
            let _ = process.lock().await.stop().await;
        }
    }
}

pub(super) struct Process {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: u64,
    pub capabilities: Value,
    pub isolation: Value,
    pub secret_usage: Vec<Value>,
    pub version: String,
    extension_id: String,
    permissions: Value,
    state: Weak<AppState>,
    proposal_session: String,
    stderr_task: JoinHandle<()>,
}

impl Drop for Process {
    fn drop(&mut self) {
        self.stderr_task.abort();
        let _ = self.child.start_kill();
    }
}

fn command_path(root: &Path, raw: &str) -> Result<PathBuf> {
    if raw.starts_with("./") {
        return manifest::relative_file(root, raw, "entrypoints.command[0]");
    }
    if raw.starts_with('/') {
        let entry = PathBuf::from(raw);
        entry.canonicalize().map_err(|e| runtime_err("extension.executable_unavailable", e.to_string()))?;
        return Ok(resolve_python_shim(entry));
    }
    let search = crate::shell_env::path_os().unwrap_or_default();
    for dir in std::env::split_paths(&search) {
        let candidate = dir.join(raw);
        if candidate.is_file() {
            return Ok(resolve_python_shim(if candidate.is_absolute() { candidate }
                else { std::env::current_dir().map_err(|e| runtime_err("extension.executable_unavailable", e.to_string()))?.join(candidate) }));
        }
    }
    Err(runtime_err(
        "extension.executable_unavailable",
        format!("extension executable {raw} not found"),
    ))
}

fn resolve_python_shim(path: PathBuf) -> PathBuf {
    if path != Path::new("/usr/bin/python3") {
        return path;
    }
    let output = crate::host_process::std_command(&path)
        .args(["-c", "import sys;print(sys.executable)"])
        .stdin(Stdio::null())
        .output();
    let Ok(output) = output else { return path };
    if !output.status.success() {
        return path;
    }
    let candidate = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    Path::new(&candidate).canonicalize().unwrap_or(path)
}

fn sandbox_plan(
    manifest: &Value,
    install_dir: &Path,
    state_dir: &Path,
    workspace_root: &Path,
    executable: &Path,
) -> Result<(Vec<String>, Value)> {
    if !cfg!(target_os = "macos") {
        return Err(runtime_err(
            "extension.isolation_unavailable",
            "no enforced extension sandbox backend on this platform",
        ));
    }
    if !Path::new("/usr/bin/sandbox-exec").is_file() {
        return Err(runtime_err(
            "extension.isolation_unavailable",
            "sandbox-exec is unavailable",
        ));
    }
    let install = install_dir
        .canonicalize()
        .map_err(|e| runtime_err("extension.isolation_unavailable", e.to_string()))?;
    let state = state_dir
        .canonicalize()
        .map_err(|e| runtime_err("extension.isolation_unavailable", e.to_string()))?;
    let venv_root = executable.parent()
        .filter(|parent| matches!(parent.file_name().and_then(|name| name.to_str()), Some("bin" | "Scripts")))
        .and_then(Path::parent)
        .filter(|root| root.join("pyvenv.cfg").is_file())
        .map(Path::canonicalize)
        .transpose()
        .map_err(|e| runtime_err("extension.isolation_unavailable", e.to_string()))?;
    let executable = executable
        .canonicalize()
        .map_err(|e| runtime_err("extension.isolation_unavailable", e.to_string()))?;
    let runtime_root = if executable.starts_with(&install) {
        install.clone()
    } else if executable.starts_with(&state) {
        state.clone()
    } else if venv_root.as_ref().is_some_and(|root| executable.starts_with(root)) {
        executable.parent().unwrap().to_path_buf()
    } else if executable.starts_with("/Applications/Xcode.app/Contents/Developer") {
        PathBuf::from("/Applications/Xcode.app/Contents/Developer")
    } else if executable.starts_with("/opt/homebrew") {
        PathBuf::from("/opt/homebrew")
    } else if executable.starts_with("/usr/local") {
        PathBuf::from("/usr/local")
    } else if executable.starts_with("/Users") {
        let parts: Vec<_> = executable.components().collect();
        if let Some(index) = parts.iter().position(|c| c.as_os_str() == "versions") {
            parts.iter().take(index + 2).collect::<PathBuf>()
        } else {
            return Err(runtime_err(
                "extension.isolation_unavailable",
                "runtime outside supported interpreter roots",
            ));
        }
    } else if executable.starts_with("/usr") || executable.starts_with("/bin") {
        PathBuf::from("/usr")
    } else {
        return Err(runtime_err(
            "extension.isolation_unavailable",
            "runtime outside supported roots",
        ));
    };
    let fs_tokens = manifest["permissions"]["filesystem"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let workspace_read = fs_tokens
        .iter()
        .any(|v| v == "workspace-read" || v == "workspace-write");
    let workspace_write = fs_tokens.iter().any(|v| v == "workspace-write");
    let network: Vec<String> = manifest["permissions"]["network"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_owned)
        .collect();
    if network.iter().any(|v| v.starts_with("*.")) {
        return Err(runtime_err(
            "extension.isolation_unavailable",
            "wildcard network target cannot be enforced",
        ));
    }
    let mut read_roots = vec![install.clone(), state.clone(), runtime_root.clone()];
    if let Some(venv) = &venv_root {
        for relative in ["pyvenv.cfg", "bin", "Scripts", "lib", "lib64", "Lib", "share"] {
            let candidate = venv.join(relative);
            match candidate.canonicalize() {
                Ok(path) if path.starts_with(venv) => read_roots.push(path),
                Ok(_) => return Err(runtime_err("extension.isolation_unavailable", "virtualenv dependency path leaves its environment")),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound && !candidate.is_symlink() => {},
                Err(error) => return Err(runtime_err("extension.isolation_unavailable", error.to_string())),
            }
        }
    }
    for path in [
        "/System",
        "/usr",
        "/bin",
        "/sbin",
        "/Library",
        "/dev",
        "/private/var/db/timezone",
        "/opt/homebrew",
        "/usr/local",
        "/etc/ssl",
        "/private/etc/ssl",
    ] {
        if Path::new(path).exists() {
            read_roots.push(PathBuf::from(path));
        }
    }
    let mut write_roots = vec![state.clone()];
    if workspace_read {
        let workspace = workspace_root
            .canonicalize()
            .map_err(|e| runtime_err("extension.isolation_unavailable", e.to_string()))?;
        read_roots.push(workspace.clone());
        if workspace_write {
            write_roots.push(workspace);
        }
    }
    read_roots.sort();
    read_roots.dedup();
    write_roots.sort();
    write_roots.dedup();
    let mut lines = vec![
        "(version 1)".to_owned(),
        "(deny default)".to_owned(),
        "(import \"system.sb\")".to_owned(),
        "(allow process-info*)".to_owned(),
        "(deny process-fork)".to_owned(),
        "(allow sysctl-read)".to_owned(),
        "(allow mach-lookup)".to_owned(),
        "(allow file-read-metadata)".to_owned(),
        format!(
            "(allow process-exec (subpath {}))",
            serde_json::to_string(&runtime_root.to_string_lossy()).unwrap()
        ),
    ];
    if let Some(venv) = &venv_root {
        for relative in ["bin", "Scripts"] {
            let directory = venv.join(relative);
            if directory.is_dir() {
                lines.push(format!("(allow process-exec (subpath {}))",
                    serde_json::to_string(&directory.to_string_lossy()).unwrap()));
            }
        }
    }
    for root in &read_roots {
        lines.push(format!(
            "(allow file-read* (subpath {}))",
            serde_json::to_string(&root.to_string_lossy()).unwrap()
        ));
    }
    for root in &write_roots {
        lines.push(format!(
            "(allow file-write* (subpath {}))",
            serde_json::to_string(&root.to_string_lossy()).unwrap()
        ));
    }
    lines.push("(allow file-write* (literal \"/dev/null\"))".to_owned());
    if !network.is_empty() {
        lines.push("(allow network-outbound (remote ip \"*:*\"))".to_owned());
    }
    let profile = state.join(".extension-sandbox.sb");
    fs::write(&profile, lines.join("\n"))
        .map_err(|e| runtime_err("extension.isolation_unavailable", e.to_string()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&profile, fs::Permissions::from_mode(0o600))
            .map_err(|e| runtime_err("extension.isolation_unavailable", e.to_string()))?;
    }
    let isolation = json!({"backend":"macos-sandbox-exec","enforcement":"enforced",
        "filesystem_roots":read_roots.iter().map(|p|p.to_string_lossy().into_owned()).collect::<Vec<_>>(),
        "network_targets":network,"network_enforcement":if network.is_empty(){"none"}else{"all_outbound"},
        "resource_limits":{"cpu_seconds":300,"open_files":128,"file_size_bytes":64*1024*1024},
        "subprocess_policy":"denied"});
    Ok((
        vec![
            "/usr/bin/sandbox-exec".to_owned(),
            "-f".to_owned(),
            profile.to_string_lossy().into_owned(),
        ],
        isolation,
    ))
}

fn secret_env(state: &AppState, manifest: &Value) -> Result<(Vec<(String, String)>, Vec<Value>)> {
    let mut env = Vec::new();
    let mut usage = Vec::new();
    for ref_value in manifest["permissions"]["secrets"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
    {
        let normalized = ref_value.strip_prefix("secret://").unwrap_or(ref_value);
        let value = if let Some(key) = normalized.strip_prefix("env/") {
            if key.is_empty() || !key.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') {
                return Err(runtime_err(
                    "extension.secret_unresolvable",
                    "invalid environment secret reference",
                ));
            }
            std::env::var(key).map_err(|_| {
                runtime_err(
                    "extension.secret_unresolvable",
                    format!("declared secret {ref_value} is unavailable"),
                )
            })?
        } else if normalized.starts_with("platform/") {
            let reference = format!("secret://{normalized}");
            crate::runs::competition_api::resolve_platform_secret(state, &reference).map_err(
                |_| {
                    runtime_err(
                        "extension.secret_unresolvable",
                        format!("declared secret {ref_value} is unavailable"),
                    )
                },
            )?
        } else {
            return Err(runtime_err(
                "extension.secret_unresolvable",
                "unsupported secret reference scope",
            ));
        };
        let env_name = format!(
            "MUTEKI_SECRET_{}",
            normalized
                .chars()
                .map(|c| if c.is_ascii_alphanumeric() {
                    c.to_ascii_uppercase()
                } else {
                    '_'
                })
                .collect::<String>()
                .trim_matches('_')
        );
        env.push((env_name.clone(), value));
        usage.push(json!({"reference":ref_value,"scope":format!("extension:{}",manifest["id"].as_str().unwrap_or("")),
            "injection":"minimal_process_environment","environment_name":env_name,"expires_at":"process_exit"}));
    }
    Ok((env, usage))
}

async fn read_frame(reader: &mut BufReader<ChildStdout>) -> Result<Vec<u8>> {
    let mut line = Vec::new();
    loop {
        let available = reader
            .fill_buf()
            .await
            .map_err(|e| runtime_err("extension.protocol_error", e.to_string()))?;
        if available.is_empty() {
            return Err(runtime_err(
                "extension.unavailable",
                "extension stdout closed",
            ));
        }
        let count = available
            .iter()
            .position(|byte| *byte == b'\n')
            .map(|n| n + 1)
            .unwrap_or(available.len());
        if line.len() + count > MAX_FRAME_BYTES {
            return Err(runtime_err(
                "extension.protocol_error",
                "response exceeds 4 MiB",
            ));
        }
        line.extend_from_slice(&available[..count]);
        let done = line.last() == Some(&b'\n');
        reader.consume(count);
        if done {
            return Ok(line);
        }
    }
}

impl Process {
    pub(super) async fn start(
        state: &Arc<AppState>,
        manifest: &Value,
        install_dir: &Path,
        state_dir: &Path,
        workspace_root: &Path,
        config: &Value,
    ) -> Result<(Self, Value)> {
        fs::create_dir_all(state_dir)
            .map_err(|e| runtime_err("extension.start_failed", e.to_string()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(state_dir, fs::Permissions::from_mode(0o700))
                .map_err(|e| runtime_err("extension.start_failed", e.to_string()))?;
        }
        let raw = manifest["entrypoints"]["command"]
            .as_array()
            .ok_or_else(|| runtime_err("extension.start_failed", "entrypoint command missing"))?;
        let command: Vec<String> = raw
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect();
        if command.is_empty() {
            return Err(runtime_err(
                "extension.start_failed",
                "entrypoint command missing",
            ));
        }
        let executable = command_path(install_dir, &command[0])?;
        let (mut prefix, isolation) = sandbox_plan(
            manifest,
            install_dir,
            state_dir,
            workspace_root,
            &executable,
        )?;
        let (secrets, secret_usage) = secret_env(state, manifest)?;
        prefix.push(executable.to_string_lossy().into_owned());
        prefix.extend(command.iter().skip(1).cloned());
        let log_dir = state_dir.join("logs");
        fs::create_dir_all(&log_dir)
            .map_err(|e| runtime_err("extension.start_failed", e.to_string()))?;
        let log_path = log_dir.join(format!(
            "{}-{}.log",
            manifest["version"].as_str().unwrap_or("unknown"),
            chrono::Utc::now().timestamp_millis()
        ));
        // Intentionally clears the environment before applying the sandboxed extension env.
        #[allow(clippy::disallowed_methods)]
        let mut cmd = tokio::process::Command::new(&prefix[0]);
        cmd.args(&prefix[1..])
            .current_dir(install_dir)
            .env_clear()
            .env(
                "PATH",
                crate::shell_env::var("PATH").unwrap_or_else(|| "/usr/bin:/bin:/usr/local/bin".to_owned()),
            )
            .env(
                "LANG",
                crate::shell_env::var("LANG").unwrap_or_else(|| "en_US.UTF-8".to_owned()),
            )
            .env("PYTHONDONTWRITEBYTECODE", "1")
            .env("MUTEKI_EXTENSION_ID", manifest["id"].as_str().unwrap_or(""))
            .env(
                "MUTEKI_EXTENSION_VERSION",
                manifest["version"].as_str().unwrap_or(""),
            )
            .env("MUTEKI_EXTENSION_STATE_DIR", state_dir)
            .env("MUTEKI_PROTOCOL_VERSION", "1")
            .env("MUTEKI_PERMISSION_ENFORCEMENT", "enforced")
            .env("TMPDIR", state_dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        for (key, value) in secrets {
            cmd.env(key, value);
        }
        #[cfg(unix)]
        unsafe {
            use std::os::unix::process::CommandExt;
            cmd.as_std_mut().pre_exec(|| {
                for (resource, limit) in [
                    (libc::RLIMIT_CPU, 300u64),
                    (libc::RLIMIT_NOFILE, 128u64),
                    (libc::RLIMIT_FSIZE, 64u64 * 1024 * 1024),
                ] {
                    let value = libc::rlimit {
                        rlim_cur: limit as libc::rlim_t,
                        rlim_max: limit as libc::rlim_t,
                    };
                    if libc::setrlimit(resource, &value) != 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                }
                Ok(())
            });
        }
        let mut child = cmd
            .spawn()
            .map_err(|e| runtime_err("extension.start_failed", e.to_string()))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| runtime_err("extension.start_failed", "extension stdin unavailable"))?;
        let stdout = BufReader::new(child.stdout.take().ok_or_else(|| {
            runtime_err("extension.start_failed", "extension stdout unavailable")
        })?);
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| runtime_err("extension.start_failed", "extension stderr unavailable"))?;
        let version = manifest["version"].as_str().unwrap_or("").to_owned();
        let id = manifest["id"].as_str().unwrap_or("").to_owned();
        let stderr_task = tokio::spawn(async move {
            let mut reader = BufReader::new(stderr);
            let mut file = tokio::fs::File::create(log_path).await.ok();
            let mut size = 0u64;
            let mut line = Vec::new();
            loop {
                line.clear();
                match reader.read_until(b'\n', &mut line).await {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {
                        size += line.len() as u64;
                        if size > 64 * 1024 * 1024 {
                            break;
                        }
                        if let Some(file) = file.as_mut() {
                            let _ = file.write_all(&line).await;
                        }
                    }
                }
            }
        });
        let mut process = Self {
            child,
            stdin,
            stdout,
            next_id: 0,
            capabilities: json!({}),
            isolation,
            secret_usage,
            version,
            extension_id: id,
            permissions: manifest["permissions"].clone(),
            state: Arc::downgrade(state),
            proposal_session: uuid::Uuid::new_v4().simple().to_string(),
            stderr_task,
        };
        let started=async {
            let init=process.rpc("initialize",json!({"extension_id":process.extension_id,"version":process.version,
                "core_version":manifest::CORE_VERSION,"config":config,"isolation":process.isolation,
                "workspace_root":if manifest["permissions"]["filesystem"].as_array().is_some_and(|items| items.iter().any(|item| item == "workspace-read")) {
                    json!(workspace_root.to_string_lossy())
                } else { Value::Null },
                "state_dir":state_dir.to_string_lossy(),"permissions":manifest["permissions"],
                "secret_injection":{"mode":"minimal_environment","scopes":manifest["permissions"]["secrets"],"lifetime":"extension_process"}})).await?;
            if init["protocol_version"]!=1 {return Err(runtime_err("extension.protocol_mismatch","extension protocol major must be 1"));}
            process.capabilities=process.rpc("capabilities/list",json!({})).await?;
            process.capabilities["muteki_isolation"]=process.isolation.clone();
            if let Some(rel)=manifest["config_schema"].as_str(){
                let schema=manifest::load_schema(install_dir,rel)?;manifest::validate_schema(config,&schema,"config")?;
                let result=process.rpc("config/validate",json!({"config":config})).await?;
                if result["valid"]!=true{return Err(runtime_err("extension.config_rejected","extension rejected configuration"));}
            }
            process.rpc("activate",json!({"config":config,"capabilities":process.capabilities})).await?;
            let health=process.rpc("health/read",json!({})).await?;
            if health["status"]!="healthy" {return Err(runtime_err("extension.health_failed",format!("health status: {}",health["status"])));}
            Ok(health)
        }.await;
        match started {
            Ok(health) => Ok((process, health)),
            Err(err) => {
                let _ = process.stop().await;
                Err(err)
            }
        }
    }

    fn event_allowed(&self, event_type: &str) -> bool {
        let prefix = format!("ext.{}.", self.extension_id);
        if !event_type.starts_with(&prefix) || event_type == prefix {
            return false;
        }
        self.permissions["events_write"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .any(|pattern| {
                if let Some(prefix) = pattern.strip_suffix('*') {
                    event_type.starts_with(prefix)
                } else {
                    event_type == pattern
                }
            })
    }

    async fn proposal(&mut self, id: &Value, params: &Value) -> Result<()> {
        let event_type = params["event_type"].as_str().unwrap_or("");
        let response = if params["protocol_version"] != 1 {
            json!({"jsonrpc":"2.0","id":id,"error":{"code":-32602,"message":"protocol version mismatch"}})
        } else if !self.event_allowed(event_type) {
            json!({"jsonrpc":"2.0","id":id,"error":{"code":-32602,"message":"event namespace denied"}})
        } else if let Some(schema) = self.capabilities["event_schemas"].get(event_type) {
            if manifest::validate_schema(&params["payload"], schema, "event").is_err() {
                json!({"jsonrpc":"2.0","id":id,"error":{"code":-32602,"message":"event schema invalid"}})
            } else {
                self.save_event(event_type, params, id)
            }
        } else {
            self.save_event(event_type, params, id)
        };
        self.stdin
            .write_all(format!("{}\n", response).as_bytes())
            .await
            .map_err(|e| runtime_err("extension.protocol_error", e.to_string()))?;
        self.stdin
            .flush()
            .await
            .map_err(|e| runtime_err("extension.protocol_error", e.to_string()))?;
        Ok(())
    }
    fn save_event(&self, event_type: &str, params: &Value, id: &Value) -> Value {
        use sha2::{Digest, Sha256};
        let Some(state) = self.state.upgrade() else {
            return json!({"jsonrpc":"2.0","id":id,"error":{"code":-32603,"message":"extension host is shutting down"}});
        };
        let command_id = format!("cmd-ext-{}-{:x}",self.proposal_session,Sha256::digest(id.to_string().as_bytes()));
        let envelope = json!({"command_id":command_id,"command_type":"extension.propose_event",
            "aggregate_type":"extension","aggregate_id":self.extension_id,
            "actor":{"kind":"extension","id":format!("ext-host:{}",self.extension_id)},
            "payload":{"extension_id":self.extension_id,"event_type":event_type,"payload":params["payload"],
                "aggregate_type":params["aggregate_type"],"aggregate_id":params["aggregate_id"],"causation_id":params["causation_id"]}});
        match super::api::commit_proposed_event(&state,&envelope) {
            Ok(result) => json!({"jsonrpc":"2.0","id":id,"result":result}),
            Err(error) => json!({"jsonrpc":"2.0","id":id,"error":{"code":-32603,"message":error.message,"data":{"code":error.code,"category":error.category}}}),
        }
    }

    pub(super) async fn rpc(&mut self, method: &str, params: Value) -> Result<Value> {
        self.next_id += 1;
        let id = self.next_id;
        let mut params = params.as_object().cloned().unwrap_or_default();
        params.insert("protocol_version".to_owned(), json!(1));
        let request = json!({"jsonrpc":"2.0","id":id,"method":method,"params":params});
        let data = serde_json::to_vec(&request)
            .map_err(|e| runtime_err("extension.protocol_error", e.to_string()))?;
        if data.len() > MAX_FRAME_BYTES {
            return Err(runtime_err(
                "extension.protocol_error",
                "request exceeds 4 MiB",
            ));
        }
        self.stdin
            .write_all(&data)
            .await
            .map_err(|e| runtime_err("extension.protocol_error", e.to_string()))?;
        self.stdin
            .write_all(b"\n")
            .await
            .map_err(|e| runtime_err("extension.protocol_error", e.to_string()))?;
        self.stdin
            .flush()
            .await
            .map_err(|e| runtime_err("extension.protocol_error", e.to_string()))?;
        let result=tokio::time::timeout(Duration::from_secs(60),async {
            loop {
                let line=read_frame(&mut self.stdout).await?;
                let frame:Value=serde_json::from_slice(&line).map_err(|e|runtime_err("extension.protocol_error",e.to_string()))?;
                if frame["jsonrpc"]!="2.0"{return Err(runtime_err("extension.protocol_error","JSON-RPC version mismatch"));}
                if frame["method"]=="event/propose" {self.proposal(&frame["id"],&frame["params"]).await?;continue;}
                if frame["method"].is_string(){
                    let reply=json!({"jsonrpc":"2.0","id":frame["id"],"error":{"code":-32601,"message":"unsupported extension request"}});
                    self.stdin.write_all(format!("{}\n",reply).as_bytes()).await.map_err(|e|runtime_err("extension.protocol_error",e.to_string()))?;
                    continue;
                }
                if frame["id"]!=id {return Err(runtime_err("extension.protocol_error","unexpected response id"));}
                if !frame["error"].is_null(){return Err(runtime_err("extension.rpc_error",frame["error"]["message"].as_str().unwrap_or("extension RPC failed")));}
                return Ok(frame["result"].clone());
            }
        }).await.map_err(|_|runtime_err("extension.timeout",format!("extension did not answer {method}")))?;
        result
    }

    pub(super) async fn stop(&mut self) -> Result<Option<i32>> {
        let _ = self.rpc("deactivate", json!({})).await;
        let _ = self.rpc("shutdown", json!({})).await;
        let status = match tokio::time::timeout(Duration::from_secs(5), self.child.wait()).await {
            Ok(Ok(status)) => Some(status),
            _ => {
                let _ = self.child.start_kill();
                self.child.wait().await.ok()
            }
        };
        self.stderr_task.abort();
        Ok(status.and_then(|status| status.code()))
    }
}
