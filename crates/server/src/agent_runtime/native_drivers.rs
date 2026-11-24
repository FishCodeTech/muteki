//! Muteki-owned native transport drivers for `droid.rpc`, `kimi.local_server`,
//! `omp.rpc_v2` and Droid stdio MCP bridge shaping.

fn supervised_command(program:impl AsRef<std::ffi::OsStr>)->tokio::process::Command{tokio::process::Command::from(agent::supervised_command(program))}

pub(crate) fn apply_env_pairs(cmd: &mut tokio::process::Command, env: &[(String, String)]) {
    for (k, v) in env {
        if crate::network_proxy::PROXY_ENV_KEYS.contains(&k.as_str()) && v.is_empty() {
            cmd.env_remove(k);
        } else {
            cmd.env(k, v);
        }
    }
}

#[path="native_drivers/droid_events.rs"]
mod droid_events;

mod agy;
pub use agy::start_agy_acp;

use super::{
    RuntimeError,
    access::{self, AccessPlan},
};
use agent::{
    AgentEvent, ApprovalDecision, ApprovalKind, ApprovalOption, ApprovalOptionKind,
    ApprovalRequest, Compaction, DeltaKind, ItemContent, ItemStatus, ProviderKind, ResumeCursor,
    SessionCommand, SessionHandle, ThreadItem, TokenUsage, TurnStatus, UserInputDelivery,
    UserInputQuestion,
};
use base64::Engine;
use serde_json::{Value, json};
use smol::channel::{Receiver, Sender};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    process::Stdio,
    sync::Arc,
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    process::{ChildStdin, Command},
    sync::Mutex,
};

pub const OMP_FRAME_LIMIT_BYTES: usize = 1024 * 1024;
pub const OMP_CHUNK_PAYLOAD_BYTES: usize = 256 * 1024;
pub const OMP_MAX_REASSEMBLED_BYTES: usize = 64 * 1024 * 1024;

const DROID_STDIO_BRIDGE_PY: &str = r#"#!/usr/bin/env python3
import json, sys, urllib.request

def main():
    cfg_path = ""
    args = sys.argv[1:]
    for i, a in enumerate(args):
        if a == "--config" and i + 1 < len(args):
            cfg_path = args[i + 1]
    with open(cfg_path, "r", encoding="utf-8") as f:
        cfg = json.load(f)
    endpoint = cfg["endpoint"]
    token = cfg["bearer_token"]
    for raw in sys.stdin:
        line = raw.strip()
        if not line:
            continue
        req = urllib.request.Request(
            endpoint,
            data=line.encode("utf-8"),
            headers={
                "Content-Type": "application/json",
                "Accept": "application/json, text/event-stream",
                "Authorization": f"Bearer {token}",
            },
            method="POST",
        )
        try:
            with urllib.request.urlopen(req, timeout=90) as resp:
                body = resp.read().decode("utf-8").strip()
                if body:
                    sys.stdout.write(body + "\n")
                    sys.stdout.flush()
        except Exception as exc:
            try:
                msg = json.loads(line)
                mid = msg.get("id")
            except Exception:
                mid = None
            if mid is not None:
                err = {"jsonrpc": "2.0", "id": mid, "error": {"code": -32000, "message": str(exc)}}
                sys.stdout.write(json.dumps(err) + "\n")
                sys.stdout.flush()

if __name__ == "__main__":
    main()
"#;

#[derive(Debug, Clone)]
pub struct DroidMcpBridge {
    pub servers: Vec<Value>,
    pub dropped: Vec<String>,
    pub data_dir: Option<PathBuf>,
}

impl DroidMcpBridge {
    pub fn cleanup(&mut self) {
        if let Some(dir) = self.data_dir.take() {
            let _ = std::fs::remove_dir_all(dir);
        }
    }
}

impl Drop for DroidMcpBridge {
    fn drop(&mut self) {
        self.cleanup();
    }
}

/// Shape MCP servers for Droid (`droid.rpc` and `droid.acp`): writes the bearer
/// token to a mode-0600 connection file inside a private directory and returns
/// Droid-validated `{type: "stdio", name, command, args, env: {KEY: value}}`
/// entries.
pub fn shape_droid_mcp(
    mcp_servers: &[agent::McpRegistration],
) -> Result<DroidMcpBridge, RuntimeError> {
    let mut bridge = DroidMcpBridge {
        servers: Vec::new(),
        dropped: Vec::new(),
        data_dir: None,
    };
    let mut internal = Vec::new();
    for reg in mcp_servers {
        if reg.name.starts_with("muteki-") {
            internal.push(reg);
        } else {
            bridge.dropped.push(reg.name.clone());
        }
    }
    if internal.is_empty() {
        return Ok(bridge);
    }
    for reg in &internal {
        if reg.url.trim().is_empty() || reg.bearer_token.trim().is_empty() {
            return Err(RuntimeError::new(
                "droid.mcp.token_missing",
                "Droid MCP injection has no gateway endpoint or bearer token",
            ));
        }
    }
    let dir = std::env::temp_dir().join(format!(
        "muteki-droid-mcp-{}",
        uuid::Uuid::new_v4().simple()
    ));
    std::fs::create_dir_all(&dir).map_err(|e| {
        RuntimeError::new(
            "runtime.io_error",
            format!("could not create droid MCP dir: {e}"),
        )
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
    }
    let script_path = dir.join("stdio_server.py");
    super::credentials::private_file(&script_path, DROID_STDIO_BRIDGE_PY.as_bytes())?;
    for (idx, reg) in internal.iter().enumerate() {
        let config_name = if idx == 0 {
            "connection.json".to_owned()
        } else {
            format!("connection-{idx}.json")
        };
        let config_path = dir.join(config_name);
        let payload = json!({
            "endpoint": reg.url,
            "bearer_token": reg.bearer_token,
        });
        super::credentials::private_file(&config_path, payload.to_string().as_bytes())?;
        bridge.servers.push(json!({
            "type": "stdio",
            "name": reg.name,
            "command": "python3",
            "args": [script_path.to_string_lossy(), "--config", config_path.to_string_lossy()],
            "env": {},
        }));
    }
    bridge.data_dir = Some(dir);
    Ok(bridge)
}

/// Map Muteki's access mode to Droid's RPC autonomy level.
pub fn droid_autonomy_level(access_mode: &str) -> Result<&'static str, RuntimeError> {
    match access_mode {
        access::SUPERVISED => Ok("off"),
        access::AUTO_ACCEPT_EDITS => Ok("low"),
        access::AUTO => Ok("medium"),
        access::FULL_ACCESS => Ok("high"),
        other => Err(RuntimeError::new(
            "runtime.access_mode_invalid",
            format!("unknown access mode {other} for droid.rpc"),
        )),
    }
}

/// Map Muteki's access mode to Kimi Local Server's per-prompt `permission_mode`.
pub fn kimi_permission_mode(access_mode: &str) -> Result<&'static str, RuntimeError> {
    match access_mode {
        access::SUPERVISED | access::AUTO_ACCEPT_EDITS => Ok("manual"),
        access::AUTO => Ok("auto"),
        access::FULL_ACCESS => Ok("yolo"),
        other => Err(RuntimeError::new(
            "runtime.access_mode_invalid",
            format!("unknown access mode {other} for kimi.local_server"),
        )),
    }
}

/// Check whether `cwd` is inside a Git worktree (`.git` directory or `gitdir:` file).
pub fn kimi_git_worktree_root(cwd: &Path) -> Option<PathBuf> {
    let canonical = std::fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
    for ancestor in canonical.ancestors() {
        let marker = ancestor.join(".git");
        if marker.is_dir() {
            return Some(ancestor.to_path_buf());
        }
        if marker.is_file()
            && let Ok(text) = std::fs::read_to_string(&marker)
            && text
                .trim_start_matches('\u{feff}')
                .lines()
                .next()
                .is_some_and(|line| line.trim_start().starts_with("gitdir:"))
        {
            return Some(ancestor.to_path_buf());
        }
    }
    None
}

pub fn validate_kimi_access_mode(
    adapter_id: &str,
    access_mode: &str,
    cwd: &Path,
) -> Result<(), RuntimeError> {
    if access_mode == access::SUPERVISED
        && let Some(root) = kimi_git_worktree_root(cwd)
    {
        return Err(RuntimeError::unsupported(format!(
            "{adapter_id} cannot enforce supervised mode inside Git worktree {} (Kimi manual mode auto-approves edits in a Git worktree); choose auto-accept-edits, auto, or full-access",
            root.display()
        )));
    }
    Ok(())
}

/// Current OMP accepts one JSONL object per stdin frame; v2 chunking is stdout-only.
/// https://github.com/can1357/oh-my-pi/blob/main/docs/rpc.md#transport-and-framing
pub fn omp_encode_frames(frame: &Value, _v2_negotiated: bool) -> Result<Vec<String>, RuntimeError> {
    serde_json::to_string(frame)
        .map(|serialized| vec![serialized])
        .map_err(|e| RuntimeError::new("runtime.protocol_error", e.to_string()))
}

#[derive(Default)]
pub struct OmpChunkReassembler {
    pending: Option<(String, usize, usize, usize, Vec<u8>)>,
}

impl OmpChunkReassembler {
    pub fn feed(&mut self, frame: Value) -> Result<Option<Value>, RuntimeError> {
        let invalid = |message: &str| RuntimeError::new("runtime.protocol_error", message);
        if frame["type"] != "rpc_chunk" {
            if self.pending.is_some() {
                return Err(invalid("rpc chunk sequence interrupted"));
            }
            return Ok(Some(frame));
        }
        let id = frame["chunkId"]
            .as_str()
            .filter(|s| !s.is_empty() && s.len() <= 128)
            .ok_or_else(|| invalid("invalid rpc chunk id"))?;
        let index = frame["index"]
            .as_u64()
            .ok_or_else(|| invalid("invalid rpc chunk index"))? as usize;
        let count = frame["count"]
            .as_u64()
            .filter(|n| *n >= 2 && *n <= 256)
            .ok_or_else(|| invalid("invalid rpc chunk count"))? as usize;
        let length = frame["byteLength"]
            .as_u64()
            .filter(|n| {
                *n >= OMP_FRAME_LIMIT_BYTES as u64 && *n <= OMP_MAX_REASSEMBLED_BYTES as u64
            })
            .ok_or_else(|| invalid("invalid rpc chunk byteLength"))? as usize;
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(
                frame["data"]
                    .as_str()
                    .ok_or_else(|| invalid("invalid rpc chunk data"))?,
            )
            .map_err(|_| invalid("invalid rpc chunk base64"))?;
        if bytes.is_empty() || bytes.len() > OMP_CHUNK_PAYLOAD_BYTES || index >= count {
            return Err(invalid("invalid rpc chunk payload"));
        }
        if self.pending.is_none() {
            if index != 0 {
                return Err(invalid("rpc chunk sequence must start at index 0"));
            }
            self.pending = Some((id.to_owned(), count, length, 0, Vec::new()));
        }
        let pending = self.pending.as_mut().unwrap();
        if pending.0 != id || pending.1 != count || pending.2 != length || pending.3 != index {
            return Err(invalid("rpc chunk sequence mismatch"));
        }
        pending.4.extend(bytes);
        pending.3 += 1;
        if pending.4.len() > length {
            return Err(invalid("rpc chunk sequence exceeds declared length"));
        }
        if pending.3 < count {
            return Ok(None);
        }
        let (_, _, _, _, bytes) = self.pending.take().unwrap();
        if bytes.len() != length {
            return Err(invalid("rpc chunk sequence length mismatch"));
        }
        let value: Value =
            serde_json::from_slice(&bytes).map_err(|_| invalid("invalid reassembled rpc frame"))?;
        if !value.is_object() {
            return Err(invalid("rpc frame must be an object"));
        }
        Ok(Some(value))
    }
}

async fn write_omp(
    stdin: Arc<Mutex<ChildStdin>>,
    frame: Value,
    v2: bool,
) -> Result<(), RuntimeError> {
    let encoded = omp_encode_frames(&frame, v2)?;
    let mut guard = stdin.lock().await;
    for line in encoded {
        guard
            .write_all(format!("{line}\n").as_bytes())
            .await
            .map_err(|e| RuntimeError::new("runtime.io_error", e.to_string()))?;
    }
    guard
        .flush()
        .await
        .map_err(|e| RuntimeError::new("runtime.io_error", e.to_string()))
}

async fn omp_request(
    stdin: &Arc<Mutex<ChildStdin>>,
    lines: &mut tokio::io::Lines<BufReader<tokio::process::ChildStdout>>,
    kind: &str,
    mut params: Value,
    v2: bool,
) -> Result<Value, RuntimeError> {
    let id = format!("req-{}", uuid::Uuid::new_v4().simple());
    params["id"] = json!(id);
    params["type"] = json!(kind);
    write_omp(stdin.clone(), params, v2).await?;
    tokio::time::timeout(Duration::from_secs(60), async {
        let mut chunks = OmpChunkReassembler::default();
        while let Some(line) = lines
            .next_line()
            .await
            .map_err(|e| RuntimeError::new("runtime.io_error", e.to_string()))?
        {
            let raw: Value = serde_json::from_str(&line)
                .map_err(|e| RuntimeError::new("runtime.protocol_error", e.to_string()))?;
            let Some(frame) = chunks.feed(raw)? else {
                continue;
            };
            if frame["type"] == "extension_ui_request" {
                write_omp(
                    stdin.clone(),
                    json!({"type":"extension_ui_response","id":frame["id"],"cancelled":true}),
                    v2,
                )
                .await?;
                continue;
            }
            if frame["command"] == "parse" && frame["success"] == false {
                return Err(RuntimeError::new(
                    "runtime.protocol_error",
                    frame["error"].to_string(),
                ));
            }
            if frame["id"] != id {
                continue;
            }
            if frame["success"] != true {
                return Err(RuntimeError::new(
                    "runtime.protocol_error",
                    format!("OMP {kind} failed: {}", frame["error"]),
                ));
            }
            return Ok(frame["data"].clone());
        }
        Err(RuntimeError::new(
            "runtime.session_closed",
            "OMP exited before command response",
        ))
    })
    .await
    .map_err(|_| {
        RuntimeError::new(
            "runtime.protocol_error",
            format!("OMP {kind} response timed out"),
        )
    })?
}

fn spawn_in_process_group(cmd: &mut Command) {
    #[cfg(unix)]
    {
        cmd.process_group(0);
    }
    let _ = cmd;
}

fn kill_process_group(pid: Option<u32>) {
    #[cfg(unix)]
    if let Some(pid) = pid {
        unsafe {
            let _ = libc::kill(-(pid as i32), libc::SIGTERM);
        }
    }
    #[cfg(not(unix))]
    let _ = pid;
}

fn droid_envelope(mut value: Value) -> Value {
    value["jsonrpc"] = json!("2.0");
    value["factoryApiVersion"] = json!("1.0.0");
    value["factoryProtocolVersion"] = json!("1.221.0");
    value["type"] = json!(if value.get("method").is_some() {
        "request"
    } else {
        "response"
    });
    value
}

#[derive(Debug)]
enum DroidFailure { Rejected(String), Unknown(String) }
impl DroidFailure {
    fn unknown(&self) -> bool { matches!(self, Self::Unknown(_)) }
    fn delivery(self, request_id: String, operation: &str) -> AgentEvent {
        AgentEvent::ControlDelivery { request_id, operation: operation.into(),
            status: if self.unknown() { "unknown" } else { "failed" }.into(),
            error: Some(json!({"code":if self.unknown(){"runtime.delivery_unknown"}else{"runtime.delivery_failed"},"message":self.to_string()})) }
    }
}
impl std::fmt::Display for DroidFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self { Self::Rejected(message) | Self::Unknown(message) => f.write_str(message) }
    }
}

struct DroidWaiter { sender:tokio::sync::oneshot::Sender<Result<Value,DroidFailure>>, late_turn:Option<(u64,String)> }
type DroidPending=Arc<std::sync::Mutex<HashMap<String,DroidWaiter>>>;
async fn droid_live_request(stdin:&Arc<Mutex<ChildStdin>>,pending:&DroidPending,method:&str,params:Value)->Result<Value,DroidFailure>{
    droid_tracked_request(stdin,pending,uuid::Uuid::new_v4().to_string(),method,params,None).await
}
async fn droid_tracked_request(stdin:&Arc<Mutex<ChildStdin>>,pending:&DroidPending,id:String,method:&str,params:Value,late_turn:Option<(u64,String)>)->Result<Value,DroidFailure>{
    let(sender,receiver)=tokio::sync::oneshot::channel();pending.lock().unwrap().insert(id.clone(),DroidWaiter{sender,late_turn});
    let result=async{let mut guard=stdin.lock().await;guard.write_all(format!("{}\n",droid_envelope(json!({"id":id,"method":method,"params":params}))).as_bytes()).await.map_err(|e|DroidFailure::Unknown(format!("Droid request write failed: {e}")))?;guard.flush().await.map_err(|e|DroidFailure::Unknown(format!("Droid request flush failed: {e}")))?;drop(guard);
        if method=="droid.compact_session"{receiver.await.map_err(|_|DroidFailure::Unknown("Droid closed before confirming compaction".into()))?}else{tokio::time::timeout(Duration::from_secs(30),receiver).await.map_err(|_|DroidFailure::Unknown(format!("Droid {method} response timed out after dispatch")))?.map_err(|_|DroidFailure::Unknown("Droid response channel closed after dispatch".into()))?}
    }.await;if !matches!(&result,Err(error) if error.unknown()){pending.lock().unwrap().remove(&id);}result
}

fn droid_user_message(
    message_id: String,
    text: &str,
    attachments: &[agent::Attachment],
) -> Result<Value, DroidFailure> {
    let mut params = json!({"messageId": message_id, "text": text});
    let mut files = Vec::new();
    let mut images = Vec::new();
    for attachment in attachments {
        if let Some(path) = attachment.workspace_file() {
            let uri = url::Url::from_file_path(path)
                .map_err(|_| DroidFailure::Rejected("Droid requires an absolute attachment path".into()))?;
            files.push(json!({"type":"text", "mediaType":"text/plain", "mime":"text/uri-list",
                "data": uri.as_str(), "name": "Workspace attachment"}));
        } else {
            images.push(json!({"type":"base64", "mediaType": attachment.media_type, "data": attachment.data_base64}));
        }
    }
    if !files.is_empty() {
        params["files"] = json!(files);
    }
    if !images.is_empty() {
        params["images"] = json!(images);
    }
    Ok(params)
}

async fn droid_compact(stdin:&Arc<Mutex<ChildStdin>>,pending:&DroidPending,session:&Arc<std::sync::Mutex<String>>,events:&Sender<AgentEvent>,model:&Option<String>,instructions:&str)->Result<Value,DroidFailure>{
    let before=droid_live_request(stdin,pending,"droid.get_context_stats",json!({})).await?;
    let mut params=json!({});if !instructions.is_empty(){params["customInstructions"]=json!(instructions);}
    let result=droid_live_request(stdin,pending,"droid.compact_session",params).await?;
    let id=result["newSessionId"].as_str().filter(|id|!id.is_empty()).ok_or_else(||DroidFailure::Unknown("Droid compact response omitted successor session identity".into()))?;
    *session.lock().unwrap()=id.into();let _=events.send(AgentEvent::SessionStarted{provider_session_id:id.into(),resume:ResumeCursor(json!({"session_id":id})),model:model.clone()}).await;
    let after=match droid_live_request(stdin,pending,"droid.get_context_stats",json!({})).await{Ok(value)=>value,Err(error)=>{let _=events.send(AgentEvent::Warning{message:format!("Droid compaction completed; refreshed context is unavailable: {error}")}).await;json!({"accuracy":"unknown"})}};
    let mut outcome=json!({"status":"completed","removed_count":result["removedCount"],"compaction":{"tokens_before":before["used"]}});
    if after["accuracy"]=="exact"{outcome["compaction"]["tokens_after"]=after["used"].clone();outcome["context"]=json!({"used":after["used"],"limit":after["limit"]});}Ok(outcome)
}

async fn droid_request(
    stdin: &Arc<Mutex<ChildStdin>>,
    lines: &mut tokio::io::Lines<BufReader<tokio::process::ChildStdout>>,
    method: &str,
    params: Value,
) -> Result<Value, RuntimeError> {
    let id = uuid::Uuid::new_v4().to_string();
    let message = droid_envelope(json!({"id":id,"method":method,"params":params}));
    stdin
        .lock()
        .await
        .write_all(format!("{message}\n").as_bytes())
        .await
        .map_err(|e| RuntimeError::new("runtime.io_error", e.to_string()))?;
    tokio::time::timeout(Duration::from_secs(60), async {
        while let Some(line) = lines
            .next_line()
            .await
            .map_err(|e| RuntimeError::new("runtime.io_error", e.to_string()))?
        {
            let frame: Value = serde_json::from_str(&line)
                .map_err(|e| RuntimeError::new("runtime.protocol_error", e.to_string()))?;
            if frame["id"] != id {
                continue;
            }
            if let Some(error) = frame.get("error").filter(|e| !e.is_null()) {
                return Err(RuntimeError::new(
                    "runtime.protocol_error",
                    format!("Droid {method}: {}", error["message"]),
                ));
            }
            return Ok(frame["result"].clone());
        }
        Err(RuntimeError::new(
            "runtime.session_closed",
            "Droid exited before session response",
        ))
    })
    .await
    .map_err(|_| {
        RuntimeError::new(
            "runtime.protocol_error",
            format!("Droid {method} timed out"),
        )
    })?
}

/// Start a `droid.rpc` session (`droid exec --input-format stream-jsonrpc --output-format stream-jsonrpc`).
pub async fn start_droid_rpc(
    binary: &Path,
    cwd: &Path,
    model: Option<String>,
    access: &AccessPlan,
    env: &[(String, String)],
    extra_args: &[String],
    resume: Option<Value>,
    instructions: Option<String>,
    mcp_servers: &[agent::McpRegistration],
) -> Result<SessionHandle, RuntimeError> {
    let autonomy = droid_autonomy_level(&access.mode)?;
    let mut mcp_bridge = shape_droid_mcp(mcp_servers)?;
    let mut cmd = supervised_command(binary);
    cmd.args([
        "exec",
        "--input-format",
        "stream-jsonrpc",
        "--output-format",
        "stream-jsonrpc",
    ]);
    if let Some(instructions) = &instructions {
        cmd.args(["--append-system-prompt", instructions]);
    }
    cmd.args(extra_args)
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    apply_env_pairs(&mut cmd, env);
    spawn_in_process_group(&mut cmd);
    let mut child = cmd.spawn().map_err(|e| {
        RuntimeError::new(
            "runtime.spawn_failed",
            format!("droid.rpc could not start: {e}"),
        )
    })?;
    let pid = child.id();
    let mut stderr = child.stderr.take().ok_or_else(|| {
        RuntimeError::new("runtime.spawn_failed", "droid.rpc stderr unavailable")
    })?;
    let (startup_done, startup_finished) = tokio::sync::oneshot::channel::<()>();
    let stderr_task = tokio::spawn(async move {
        let mut captured = Vec::new();
        tokio::select! {
            result = stderr.read_to_end(&mut captured) => result.map(|_| captured),
            _ = startup_finished => {
                tokio::io::copy(&mut stderr, &mut tokio::io::sink()).await.map(|_| Vec::new())
            }
        }
    });
    let stdin = Arc::new(Mutex::new(child.stdin.take().ok_or_else(|| {
        RuntimeError::new("runtime.spawn_failed", "droid.rpc stdin unavailable")
    })?));
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| RuntimeError::new("runtime.spawn_failed", "droid.rpc stdout unavailable"))?;

    let write_rpc = |stdin: Arc<Mutex<ChildStdin>>, value: Value| async move {
        let mut line = serde_json::to_vec(&droid_envelope(value)).unwrap_or_default();
        line.push(b'\n');
        let mut guard = stdin.lock().await;
        guard.write_all(&line).await?;
        guard.flush().await
    };

    let resumed_id = resume.as_ref().and_then(|r| {
        r["session_id"]
            .as_str()
            .or_else(|| r["sessionId"].as_str())
            .or_else(|| r.as_str())
            .map(str::to_owned)
    });
    let mut lines = BufReader::new(stdout).lines();
    let mut params = if let Some(existing) = &resumed_id {
        json!({"sessionId":existing,"mcpServers":mcp_bridge.servers})
    } else {
        json!({"machineId":format!("muteki-{}",uuid::Uuid::new_v4().simple()),"cwd":cwd,
            "modelId":model,"interactionMode":"auto","autonomyLevel":autonomy,
            "mcpServers":mcp_bridge.servers})
    };
    params
        .as_object_mut()
        .unwrap()
        .retain(|_, value| !value.is_null());
    let startup = async {
        let mut opened = droid_request(
            &stdin,
            &mut lines,
            if resumed_id.is_some() {
                "droid.load_session"
            } else {
                "droid.initialize_session"
            },
            params,
        )
        .await?;
        let session_id = opened["sessionId"]
            .as_str()
            .map(str::to_owned)
            .or(resumed_id)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| RuntimeError::new("runtime.protocol_error", "Droid omitted sessionId"))?;
        if opened["settings"]["autonomyLevel"] != autonomy
            || opened["settings"]["interactionMode"] == "spec"
            || model
                .as_ref()
                .is_some_and(|m| opened["settings"]["modelId"] != *m)
        {
            let mut update = json!({"autonomyLevel":autonomy,"interactionMode":"auto","modelId":model});
            update.as_object_mut().unwrap().retain(|_, v| !v.is_null());
            droid_request(&stdin, &mut lines, "droid.update_session_settings", update).await?;
            opened = droid_request(
                &stdin,
                &mut lines,
                "droid.load_session",
                json!({"sessionId":session_id,"mcpServers":mcp_bridge.servers}),
            )
            .await?;
        }
        if opened["settings"]["autonomyLevel"] != autonomy {
            return Err(RuntimeError::new(
                "droid.autonomy_unconfirmed",
                "Droid did not confirm the selected access mode",
            ));
        }

        Ok::<_, RuntimeError>((opened, session_id))
    }.await;
    let (opened, session_id) = match startup {
        Ok(value) => { let _ = startup_done.send(()); value }
        Err(mut error) => {
            kill_process_group(pid);
            if let Err(stop_error) = child.kill().await {
                error.message.push_str(&format!("\nDroid stop: {stop_error}"));
            }
            match child.wait().await {
                Ok(status) => error.message.push_str(&format!("\nDroid process: {status}")),
                Err(wait_error) => error.message.push_str(&format!("\nDroid wait: {wait_error}")),
            }
            match stderr_task.await {
                Ok(Ok(bytes)) if !bytes.is_empty() => {
                    error.message.push_str("\nstderr:\n");
                    error.message.push_str(&String::from_utf8_lossy(&bytes));
                }
                Ok(Ok(_)) => {},
                Ok(Err(read_error)) => error.message.push_str(&format!("\nDroid stderr read: {read_error}")),
                Err(join_error) => error.message.push_str(&format!("\nDroid stderr capture: {join_error}")),
            }
            return Err(error);
        }
    };

    let model = opened["settings"]["modelId"]
        .as_str()
        .map(str::to_owned)
        .or(model);

    let (cmd_tx, cmd_rx): (Sender<SessionCommand>, Receiver<SessionCommand>) =
        smol::channel::unbounded();
    let (evt_tx, evt_rx): (Sender<AgentEvent>, Receiver<AgentEvent>) = smol::channel::unbounded();
    if let Some(pid)=pid {let _=evt_tx.try_send(AgentEvent::ProcessStarted{pid});}

    let _ = evt_tx.try_send(AgentEvent::SessionStarted {
        provider_session_id: session_id.clone(),
        resume: ResumeCursor(json!({"session_id": session_id})),
        model: model.clone(),
    });

    let pending:DroidPending=Arc::new(std::sync::Mutex::new(HashMap::new()));
    let reader_pending=pending.clone();
    let reader_stdin=stdin.clone();
    let reverse_ids=Arc::new(std::sync::Mutex::new(HashMap::<String,Value>::new()));let reader_reverse_ids=reverse_ids.clone();
    let offers=Arc::new(std::sync::Mutex::new(HashMap::<String,Vec<String>>::new()));let reader_offers=offers.clone();
    let questions = Arc::new(std::sync::Mutex::new(HashMap::<String, Vec<Value>>::new()));
    let reader_questions = questions.clone();
    let active_turn = Arc::new(std::sync::Mutex::new(String::new()));
    let reader_turn = active_turn.clone();
    let reader_evt_tx = evt_tx.clone();
    let reader_session_id = Arc::new(std::sync::Mutex::new(session_id));
    let reader_sid_clone = reader_session_id.clone();
    let reader_model = model.clone();
    tokio::spawn(async move {
        let mut assistant_buf = String::new();
        let mut projection=droid_events::Projection::default();
        projection.baseline=resume.as_ref().and_then(|value|value.get("usage_baseline")).filter(|value|value.is_object()).cloned().or_else(||opened.get("inclusiveTokenUsage").or_else(||opened.pointer("/settings/tokenUsage")).or_else(||opened.get("tokenUsage")).filter(|value|value.is_object()).cloned().or_else(||if resume.is_none(){Some(json!({"inputTokens":0,"outputTokens":0,"cacheReadTokens":0,"cacheCreationTokens":0,"thinkingTokens":0}))}else{None}));
        while let Ok(Some(line)) = lines.next_line().await {
            let Ok(frame) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            if frame.get("result").is_some()||frame.get("error").is_some(){
                let waiter=reader_pending.lock().unwrap().remove(frame["id"].as_str().unwrap_or(""));
                if let Some(waiter)=waiter{
                    let result=if let Some(error)=frame.get("error").filter(|e|!e.is_null()){Err(DroidFailure::Rejected(error["message"].as_str().unwrap_or("Droid RPC failed").to_owned()))}else{Ok(frame["result"].clone())};
                    if let Err(result)=waiter.sender.send(result) && let Some((delivery_id,turn_id))=waiter.late_turn {
                        match result {Ok(_)=>{let _=reader_evt_tx.send(AgentEvent::TurnAccepted{delivery_id}).await;},Err(error)=>{
                            let _=reader_evt_tx.send(AgentEvent::Origin{turn_id:turn_id.clone(),agent_id:None,event:Box::new(AgentEvent::TurnFailed{code:"droid.prompt_delivery_failed".into(),message:error.to_string()})}).await;
                            let _=reader_evt_tx.send(AgentEvent::TurnCompleted{turn_id:turn_id.clone(),status:TurnStatus::Failed,usage:None}).await;
                            let mut active=reader_turn.lock().unwrap();if *active==turn_id{active.clear();}
                        }}
                    }continue;
                }
            }
            if let Some(err) = frame.get("error").filter(|e| !e.is_null()) {
                let msg = err["message"]
                    .as_str()
                    .unwrap_or("droid RPC error")
                    .to_owned();
                let _ = reader_evt_tx
                    .send(AgentEvent::Error {
                        message: msg,
                        fatal: frame["id"] == json!(1) || frame["id"] == json!(2),
                    })
                    .await;
                continue;
            }
            let payload = if frame["params"]["notification"].is_object() {
                &frame["params"]["notification"]
            } else {
                frame.get("params").unwrap_or(&frame)
            };
            let kind = payload["type"]
                .as_str()
                .or_else(|| frame["method"].as_str())
                .or_else(|| frame["type"].as_str())
                .unwrap_or("");
            let turn_id = reader_turn.lock().unwrap().clone();
            for event in projection.apply(payload,&turn_id){let _=reader_evt_tx.send(event).await;}
            match kind {
                "assistant_text_delta" | "text_delta" | "TextDelta" | "message.delta" => {
                    let delta = payload["textDelta"]
                        .as_str()
                        .or_else(|| payload["text"].as_str())
                        .or_else(|| payload["delta"].as_str())
                        .unwrap_or("");
                    if !delta.is_empty() {
                        assistant_buf.push_str(delta);
                        let _ = reader_evt_tx
                            .send(AgentEvent::Delta {
                                item_id: format!("{turn_id}:msg-1"),
                                kind: DeltaKind::AssistantText,
                                text: delta.to_owned(),
                            })
                            .await;
                    }
                }
                "thinking_text_delta" | "thinking_delta" | "ThinkingDelta" => {
                    let delta = payload["textDelta"]
                        .as_str()
                        .or_else(|| payload["text"].as_str())
                        .or_else(|| payload["delta"].as_str())
                        .unwrap_or("");
                    if !delta.is_empty() {
                        let _ = reader_evt_tx
                            .send(AgentEvent::Delta {
                                item_id: format!("{turn_id}:reason-1"),
                                kind: DeltaKind::ReasoningText,
                                text: delta.to_owned(),
                            })
                            .await;
                    }
                }
                "droid.request_permission" | "permission_request" | "PermissionRequest" => {
                    let req_id = frame["id"]
                        .as_str()
                        .map(str::to_owned)
                        .or_else(|| frame["id"].as_u64().map(|n| n.to_string()))
                        .or_else(|| payload["requestId"].as_str().map(str::to_owned))
                        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
                    let tool = &payload["toolUses"][0]["toolUse"];
                    let options=payload["options"].as_array().into_iter().flatten().filter_map(|option|{
                        let id=option["value"].as_str()?;let kind=match id{"proceed_once"=>ApprovalOptionKind::AllowOnce,"cancel"=>ApprovalOptionKind::RejectOnce,_=>return None};
                        Some(ApprovalOption{id:id.into(),label:option["label"].as_str().filter(|s|!s.is_empty()).unwrap_or(id).into(),kind})
                    }).collect::<Vec<_>>();
                    if options.is_empty(){
                        let reply=droid_envelope(json!({"jsonrpc":"2.0","id":frame["id"],"error":{"code":-32602,"message":"Muteki received no supported one-time permission or cancellation option"}}));
                        let mut writer=reader_stdin.lock().await;
                        let delivered=writer.write_all(format!("{reply}\n").as_bytes()).await.is_ok()&&writer.flush().await.is_ok();drop(writer);
                        let _=reader_evt_tx.send(AgentEvent::Warning{message:if delivered{"Droid permission request rejected: no supported native choices"}else{"Droid permission rejection delivery is unknown"}.into()}).await;continue;
                    }
                    reader_reverse_ids.lock().unwrap().insert(req_id.clone(),frame["id"].clone());reader_offers.lock().unwrap().insert(req_id.clone(),options.iter().map(|option|option.id.clone()).collect());
                    let(approval_kind,fields)=droid_events::permission(payload);
                    let _ = reader_evt_tx
                        .send(AgentEvent::ApprovalRequested(ApprovalRequest {native_execution: Some(json!({"tool_name":fields["title"],"action":fields["title"],"arguments":fields,"resource_kind":match fields["approval_kind"].as_str(){Some("command_execution")=>"process",Some("file_change")=>"file",Some("plan_exit")=>"control",_=>"mcp"}})), tool_call_id: tool["id"].as_str().map(str::to_owned),
                            id: req_id,
                            turn_id: Some(turn_id.clone()),
                            parent_item_id: None,
                            kind: approval_kind,
                            options,
                        }))
                        .await;
                }
                "droid.ask_user" | "question_request" | "QuestionRequest" => {
                    let req_id = frame["id"]
                        .as_str()
                        .map(str::to_owned)
                        .or_else(||frame["id"].as_u64().map(|id|id.to_string()))
                        .or_else(|| payload["requestId"].as_str().map(str::to_owned))
                        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
                    reader_reverse_ids.lock().unwrap().insert(req_id.clone(),frame["id"].clone());
                    let requested = payload["questions"].as_array().cloned().unwrap_or_default();
                    reader_questions
                        .lock()
                        .unwrap()
                        .insert(req_id.clone(), requested.clone());
                    let questions = requested
                        .iter()
                        .map(|q| UserInputQuestion { contract: agent::native_question_contract(q),
                            id: q["index"].to_string(),
                            header: q["topic"].as_str().unwrap_or("").to_owned(),
                            question: q["question"].as_str().unwrap_or("").to_owned(),
                            options: q["options"]
                                .as_array()
                                .into_iter()
                                .flatten()
                                .filter_map(|o| o.as_str())
                                .map(|text| agent::UserInputOption {
                                    label: text.to_owned(),
                                    description: String::new(),
                                })
                                .collect(),
                            multi_select: q["multiSelect"].as_bool().unwrap_or(false),
                            prefill: None,
                        })
                        .collect();
                    let _ = reader_evt_tx
                        .send(AgentEvent::UserInputRequested {
                            request_id: req_id,
                            delivery: UserInputDelivery::Blocking,
                            questions,
                        })
                        .await;
                }
                "compact_complete" | "session_compacted" => {
                    if let Some(new_sid) = payload["sessionId"]
                        .as_str()
                        .or_else(|| payload["newSessionId"].as_str())
                        .filter(|s| !s.is_empty())
                    {
                        *reader_sid_clone.lock().unwrap() = new_sid.to_owned();
                        let _ = reader_evt_tx
                            .send(AgentEvent::SessionStarted {
                                provider_session_id: new_sid.to_owned(),
                                resume: ResumeCursor(json!({"session_id": new_sid})),
                                model: reader_model.clone(),
                            })
                            .await;
                    }
                    let _ = reader_evt_tx
                        .send(AgentEvent::ContextCompacted(Compaction {
                            pre_tokens: payload["tokensBefore"].as_u64(),
                            post_tokens: payload["tokensAfter"].as_u64(),
                            ..Default::default()
                        }))
                        .await;
                }
                "agent_turn_completed" | "run_success" | "RunSuccess" | "turn_completed" => {
                    reader_turn.lock().unwrap().clear();
                    let text = std::mem::take(&mut assistant_buf);
                    if !text.is_empty() {
                        let _ = reader_evt_tx
                            .send(AgentEvent::ItemCompleted(ThreadItem {
                                id: format!("{turn_id}:msg-1"),
                                parent_item_id: None,
                                content: ItemContent::AssistantMessage { text },
                            }))
                            .await;
                    }
                    let usage=None;
                    if let Some(baseline)=&projection.baseline{let sid=reader_sid_clone.lock().unwrap().clone();let _=reader_evt_tx.send(AgentEvent::SessionStarted{provider_session_id:sid.clone(),resume:ResumeCursor(json!({"session_id":sid,"usage_baseline":baseline})),model:reader_model.clone()}).await;}
                    let _ = reader_evt_tx
                        .send(AgentEvent::TurnCompleted {
                            turn_id: turn_id.clone(),
                            status: match payload["reason"].as_str() {
                                Some("cancelled") => TurnStatus::Interrupted,
                                Some(reason) if !matches!(reason,"completed"|"spec_handoff") => TurnStatus::Failed,
                                _ => TurnStatus::Completed,
                            },
                            usage,
                        })
                        .await;
                }
                "run_failure" | "RunFailure" => {
                    reader_turn.lock().unwrap().clear();assistant_buf.clear();
                    let _=reader_evt_tx.send(projection.settle_incomplete(&turn_id)).await;
                    let sid=reader_sid_clone.lock().unwrap().clone();
                    let _=reader_evt_tx.send(AgentEvent::SessionStarted{provider_session_id:sid.clone(),resume:ResumeCursor(json!({"session_id":sid,"usage_baseline":projection.baseline})),model:reader_model.clone()}).await;
                    let msg = payload["message"]
                        .as_str()
                        .or_else(|| payload["error"].as_str())
                        .unwrap_or("droid turn failed")
                        .to_owned();
                    let _ = reader_evt_tx
                        .send(AgentEvent::TurnFailed {
                            code: "runtime.turn_failed".into(),
                            message: msg,
                        })
                        .await;
                    let _ = reader_evt_tx
                        .send(AgentEvent::TurnCompleted {
                            turn_id: turn_id.clone(),
                            status: TurnStatus::Failed,
                            usage: None,
                        })
                        .await;
                }
                "run_interrupted" | "RunInterrupted" => {
                    reader_turn.lock().unwrap().clear();assistant_buf.clear();
                    let _=reader_evt_tx.send(projection.settle_incomplete(&turn_id)).await;
                    let sid=reader_sid_clone.lock().unwrap().clone();
                    let _=reader_evt_tx.send(AgentEvent::SessionStarted{provider_session_id:sid.clone(),resume:ResumeCursor(json!({"session_id":sid,"usage_baseline":projection.baseline})),model:reader_model.clone()}).await;
                    let _ = reader_evt_tx
                        .send(AgentEvent::TurnCompleted {
                            turn_id: turn_id.clone(),
                            status: TurnStatus::Interrupted,
                            usage: None,
                        })
                        .await;
                }
                _ => {}
            }
        }
        for (_,waiter) in reader_pending.lock().unwrap().drain(){let _=waiter.sender.send(Err(DroidFailure::Unknown("Droid session closed before replying".into())));}
        let _ = reader_evt_tx
            .send(AgentEvent::SessionClosed {
                reason: Some("droid.rpc process exited".into()),
            })
            .await;
    });

    tokio::spawn(async move {
        let mut next_rpc_id = 10u64;
        let mut queued=std::collections::VecDeque::new();
        loop {
            let cmd=if active_turn.lock().unwrap().is_empty()&&!queued.is_empty(){queued.pop_front().unwrap()}else{
                tokio::select!{command=cmd_rx.recv()=>{let Ok(command)=command else{break};command},_=tokio::time::sleep(Duration::from_millis(25)),if !queued.is_empty()=>continue}
            };
            if matches!(&cmd,SessionCommand::SendTurn{..})&&!active_turn.lock().unwrap().is_empty(){queued.push_back(cmd);continue;}
            if matches!(&cmd,SessionCommand::Interrupt|SessionCommand::Shutdown){
                for command in queued.drain(..){if let SessionCommand::SendTurn{delivery_id,..}=command{let _=evt_tx.send(AgentEvent::ControlDelivery{request_id:delivery_id.to_string(),operation:"turn".into(),status:"failed".into(),error:Some(json!({"code":"runtime.cancelled_before_delivery","message":"Queued turn was cancelled before native dispatch"}))}).await;}}
            }
            next_rpc_id += 1;
            if let SessionCommand::SetOption{id,value}=&cmd && id=="muteki:operation"{
                let request_id=value["request_id"].as_str().unwrap_or("").to_owned();
                if value["name"]!="compact"||!active_turn.lock().unwrap().is_empty(){let _=evt_tx.send(DroidFailure::Rejected("Droid native operation is unavailable while active".into()).delivery(request_id,"runtime_operation")).await;continue;}
                let arguments=value["arguments"].as_str().unwrap_or("").to_owned();
                let compact_id=format!("compact-operation:{request_id}");*active_turn.lock().unwrap()=compact_id.clone();
                let(stdin,pending,sid,events,model,active)=(stdin.clone(),pending.clone(),reader_session_id.clone(),evt_tx.clone(),model.clone(),active_turn.clone());
                tokio::spawn(async move{let result=droid_compact(&stdin,&pending,&sid,&events,&model,&arguments).await;if !matches!(&result,Err(error) if error.unknown()){let mut current=active.lock().unwrap();if *current==compact_id{current.clear();}}let _=events.send(match result{Ok(result)=>AgentEvent::RuntimeOperationResult{request_id,result:Some(result),error:None},Err(error)=>error.delivery(request_id,"runtime_operation")}).await;});continue;
            }
            match cmd {
                SessionCommand::SendTurn {
                    delivery_id,
                    text,
                    options,
                    attachments,
                } => {
                    let turn_id = format!("turn-{}", uuid::Uuid::new_v4().simple());
                    *active_turn.lock().unwrap() = turn_id.clone();
                    let _ = evt_tx.send(AgentEvent::TurnStarted { turn_id:turn_id.clone() }).await;
                    let result:Result<Value,DroidFailure>=async{
                        if let Some(effort)=options.as_ref().and_then(|options|options.effort.as_deref()).filter(|effort|*effort!="default"){droid_live_request(&stdin,&pending,"droid.update_session_settings",json!({"reasoningEffort":effort})).await?;}
                        let message_id = format!("muteki-{}", uuid::Uuid::new_v4().simple());
                        let message = droid_user_message(message_id, &text, &attachments)?;
                        let rpc_id = uuid::Uuid::new_v4().to_string();
                        droid_tracked_request(&stdin,&pending,rpc_id,"droid.add_user_message",message,Some((delivery_id,turn_id))).await
                    }.await;
                    if let Err(error)=result{if error.unknown(){let _=evt_tx.send(error.delivery(delivery_id.to_string(),"turn")).await;continue;}let id=std::mem::take(&mut *active_turn.lock().unwrap());let _=evt_tx.send(AgentEvent::TurnFailed{code:"droid.prompt_delivery_failed".into(),message:error.to_string()}).await;let _=evt_tx.send(AgentEvent::TurnCompleted{turn_id:id,status:TurnStatus::Failed,usage:None}).await;continue;}
                    let _=evt_tx.send(AgentEvent::TurnAccepted{delivery_id}).await;
                }
                SessionCommand::Steer { request_id, text, attachments } => {
                    let result = async {
                        let message_id = format!("{}{request_id}", droid_events::STEER_MESSAGE_PREFIX);
                        let mut message = droid_user_message(message_id.clone(), &text, &attachments)?;
                        message["queuePlacement"] = json!("end_of_turn");
                        let _ = evt_tx.send(AgentEvent::ControlDelivery {
                            request_id: request_id.clone(),
                            operation: "steer".into(),
                            status: "in_flight".into(),
                            error: None,
                        }).await;
                        if active_turn.lock().unwrap().is_empty() {
                            return Err(DroidFailure::Rejected("Droid turn ended before steering".into()));
                        }
                        droid_tracked_request(&stdin, &pending, message_id, "droid.add_user_message", message, None).await
                    }.await;
                    if let Err(error) = result {
                        let _ = evt_tx.send(error.delivery(request_id, "steer")).await;
                    }
                }
                SessionCommand::RespondApproval { request_id,decision } => {
                    let selected=match &decision{ApprovalDecision::Option(id)=>id.as_str(),ApprovalDecision::Cancel=>"cancel"};
                    let offered=offers.lock().unwrap().get(&request_id).is_some_and(|options|options.iter().any(|option|option==selected));
                    let wire_id=reverse_ids.lock().unwrap().get(&request_id).cloned();
                    if !offered||wire_id.as_ref().is_none_or(Value::is_null){let _=evt_tx.send(AgentEvent::ControlDelivery{request_id,operation:"approval".into(),status:"failed".into(),error:Some(json!({"code":"droid.permission_stale","message":"Native request or selected permission option is unavailable"}))}).await;continue;}
                    let result=write_rpc(stdin.clone(),json!({"jsonrpc":"2.0","id":wire_id,"result":{"selectedOption":selected}})).await;
                    let event=match result{Ok(())=>{reverse_ids.lock().unwrap().remove(&request_id);offers.lock().unwrap().remove(&request_id);AgentEvent::ApprovalResolved{request_id,decision}},Err(err)=>AgentEvent::ControlDelivery{request_id,operation:"approval".into(),status:"unknown".into(),error:Some(json!({"code":"droid.reply_unknown","message":err.to_string()}))}};let _=evt_tx.send(event).await;
                }
                SessionCommand::RespondUserInput {request_id,answers}=>{
                    let requested=questions.lock().unwrap().get(&request_id).cloned().unwrap_or_default();
                    let collected=requested.iter().filter_map(|q|{let value=answers.get(&q["index"].to_string())?;Some(json!({"index":q["index"],"question":q["question"],"answer":value.as_str().map(str::to_owned).unwrap_or_else(||value.as_array().map(|values|values.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(", ")).unwrap_or_default())}))}).collect::<Vec<_>>();
                    let wire_id=reverse_ids.lock().unwrap().get(&request_id).cloned();
                    if wire_id.as_ref().is_none_or(Value::is_null){let _=evt_tx.send(AgentEvent::ControlDelivery{request_id,operation:"user_input".into(),status:"failed".into(),error:Some(json!({"code":"droid.question_stale","message":"Native question request is unavailable"}))}).await;continue;}
                    let result=write_rpc(stdin.clone(),json!({"id":wire_id,"result":{"cancelled":answers.is_empty(),"answers":collected}})).await;
                    match result{Ok(())=>{reverse_ids.lock().unwrap().remove(&request_id);questions.lock().unwrap().remove(&request_id);let _=evt_tx.send(AgentEvent::UserInputResolved{request_id,answers}).await;},Err(err)=>{let _=evt_tx.send(AgentEvent::ControlDelivery{request_id,operation:"user_input".into(),status:"unknown".into(),error:Some(json!({"code":"droid.reply_unknown","message":err.to_string()}))}).await;}}
                }
                SessionCommand::Compact => {
                    if !active_turn.lock().unwrap().is_empty(){let _=evt_tx.send(AgentEvent::Warning{message:"Droid compaction rejected while a native turn is active".into()}).await;continue;}
                    let turn_id=format!("compact-{}",uuid::Uuid::new_v4().simple());*active_turn.lock().unwrap()=turn_id.clone();let _=evt_tx.send(AgentEvent::TurnStarted{turn_id:turn_id.clone()}).await;
                    let(stdin,pending,sid,events,model,active)=(stdin.clone(),pending.clone(),reader_session_id.clone(),evt_tx.clone(),model.clone(),active_turn.clone());
                    tokio::spawn(async move{
                        let result=droid_compact(&stdin,&pending,&sid,&events,&model,"").await;
                        let succeeded=result.is_ok();match result{Ok(result)=>{let _=events.send(AgentEvent::ContextCompacted(Compaction{pre_tokens:result["compaction"]["tokens_before"].as_u64(),post_tokens:result["compaction"]["tokens_after"].as_u64(),status:Some("completed".into()),..Default::default()})).await;},Err(error)=>{let _=events.send(AgentEvent::Origin{turn_id:turn_id.clone(),agent_id:None,event:Box::new(AgentEvent::TurnFailed{code:if error.unknown(){"runtime.delivery_unknown"}else{"droid.compact_failed"}.into(),message:error.to_string()})}).await;}}
                        {let mut current=active.lock().unwrap();if *current==turn_id{current.clear();}}
                        let _=events.send(AgentEvent::TurnCompleted{turn_id,status:if succeeded{TurnStatus::Completed}else{TurnStatus::Failed},usage:None}).await;
                    });
                }
                SessionCommand::Interrupt => {
                    let req = json!({
                        "jsonrpc": "2.0",
                        "id": next_rpc_id.to_string(),
                        "method": "droid.interrupt_session",
                        "params": {}
                    });
                    let _ = write_rpc(stdin.clone(), req).await;
                }
                SessionCommand::Shutdown => {
                    kill_process_group(pid);
                    let _ = child.kill().await;
                    mcp_bridge.cleanup();
                    break;
                }
                _ => {}
            }
        }
        kill_process_group(pid);
        let _ = child.kill().await;
        mcp_bridge.cleanup();
    });

    Ok(SessionHandle {
        provider: ProviderKind::Acp,
        commands: cmd_tx,
        events: evt_rx,
    })
}

/// Resolve Kimi Local Server token from explicit env or `<KIMI_CODE_HOME>/server.token`.
pub fn resolve_kimi_server_token(env: &[(String, String)], home: Option<&Path>) -> Option<String> {
    if let Some((_, val)) = env
        .iter()
        .rev()
        .find(|(k, v)| k == "KIMI_CODE_SERVER_TOKEN" && !v.trim().is_empty())
    {
        return Some(val.trim().to_owned());
    }
    if let Ok(val) = std::env::var("KIMI_CODE_SERVER_TOKEN")
        && !val.trim().is_empty()
    {
        return Some(val.trim().to_owned());
    }
    let kimi_home = env
        .iter()
        .rev()
        .find(|(k, v)| k == "KIMI_CODE_HOME" && !v.trim().is_empty())
        .map(|(_, v)| PathBuf::from(v))
        .or_else(|| home.map(Path::to_path_buf))
        .or_else(|| std::env::var_os("KIMI_CODE_HOME").map(PathBuf::from))
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".kimi-code")))?;
    std::fs::read_to_string(kimi_home.join("server.token"))
        .ok()
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
}

mod kimi;
mod opencode;
#[cfg(test)]
mod live_controls;
mod omp_events;
pub use kimi::start_kimi_local_server;
pub use opencode::{start_opencode_v2, list_opencode_v2};

fn omp_images(frame: &mut Value, attachments: Vec<agent::Attachment>) {
    if !attachments.is_empty() {
        frame["images"] = json!(attachments.into_iter().map(|image|
            json!({"type":"image","data":image.data_base64,"mimeType":image.media_type})).collect::<Vec<_>>());
    }
}

/// Start an `omp.rpc_v2` session (`omp --mode rpc` with JSONL `{id, type, ...params}` framing,
/// 1 MiB chunk threshold / 256 KiB chunk payload / 64 MiB reassembly, startup handshake, and
/// reverse `host_tool_call` bridge).
pub async fn start_omp_rpc_v2(
    binary: &Path,
    cwd: &Path,
    model: Option<String>,
    env: &[(String, String)],
    extra_args: &[String],
    resume: Option<Value>,
    instructions: Option<String>,
    mcp_servers: Vec<agent::McpRegistration>,
) -> Result<SessionHandle, RuntimeError> {
    let selected_model = model.clone().or_else(|| {
        env.iter()
            .rev()
            .find(|(key, _)| key == "MUTEKI_OMP_MODEL")
            .map(|(_, value)| value.clone())
    });
    let selected_provider = env
        .iter()
        .rev()
        .find(|(key, _)| key == "MUTEKI_OMP_PROVIDER")
        .map(|(_, value)| value.as_str())
        .unwrap_or("");
    let selection = selected_model
        .as_deref()
        .filter(|m| !m.trim().is_empty())
        .map(|selected| {
            if selected_provider.is_empty() {
                selected.split_once('/').unwrap_or(("", selected))
            } else {
                (
                    selected_provider,
                    selected
                        .strip_prefix(&format!("{selected_provider}/"))
                        .unwrap_or(selected),
                )
            }
        });
    let mut cmd = supervised_command(binary);
    cmd.args(["--mode", "rpc", "--cwd", &cwd.to_string_lossy()]);
    if let Some((provider, selected)) = selection {
        if !provider.is_empty() {
            cmd.args(["--provider", provider]);
        }
        cmd.args(["--model", selected]);
    }
    if let Some(instructions) = &instructions
        && !instructions.trim().is_empty()
    {
        cmd.args(["--append-system-prompt", instructions]);
    }
    cmd.args(extra_args)
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    apply_env_pairs(&mut cmd, env);
    spawn_in_process_group(&mut cmd);
    let mut child = cmd.spawn().map_err(|e| {
        RuntimeError::new(
            "runtime.spawn_failed",
            format!("omp.rpc_v2 could not start: {e}"),
        )
    })?;
    let pid = child.id();
    if let Some(mut stderr) = child.stderr.take() {
        tokio::spawn(async move {
            let _ = tokio::io::copy(&mut stderr, &mut tokio::io::sink()).await;
        });
    }
    let stdin = Arc::new(Mutex::new(child.stdin.take().ok_or_else(|| {
        RuntimeError::new("runtime.spawn_failed", "omp.rpc_v2 stdin unavailable")
    })?));
    let stdout = child.stdout.take().ok_or_else(|| {
        RuntimeError::new("runtime.spawn_failed", "omp.rpc_v2 stdout unavailable")
    })?;

    let mut lines = BufReader::new(stdout).lines();
    let ready_line = tokio::time::timeout(Duration::from_secs(20), lines.next_line())
        .await
        .map_err(|_| {
            RuntimeError::new(
                "runtime.protocol_error",
                "omp.rpc_v2 timed out waiting for ready frame",
            )
        })?
        .map_err(|e| RuntimeError::new("runtime.io_error", e.to_string()))?
        .ok_or_else(|| {
            RuntimeError::new(
                "runtime.protocol_error",
                "omp.rpc_v2 exited before ready frame",
            )
        })?;
    let ready: Value = serde_json::from_str(&ready_line).map_err(|e| {
        RuntimeError::new(
            "runtime.protocol_error",
            format!("omp.rpc_v2 invalid ready JSON: {e}"),
        )
    })?;
    if ready["type"] != "ready" {
        return Err(RuntimeError::new(
            "runtime.protocol_error",
            "OMP did not send its ready frame",
        ));
    }
    let supports_v2 = if ready["supportedProtocolVersions"]
        .as_array()
        .is_some_and(|v| v.iter().any(|v| v == 2))
    {
        omp_request(
            &stdin,
            &mut lines,
            "negotiate_protocol",
            json!({"protocolVersion":2}),
            false,
        )
        .await
        .is_ok_and(|v| v["protocolVersion"] == 2)
    } else {
        false
    };
    omp_request(&stdin, &mut lines, "get_state", json!({}), supports_v2).await?;
    if let Some(session) = resume
        .as_ref()
        .and_then(|r| {
            r["session_file"]
                .as_str()
                .or_else(|| r["sessionFile"].as_str())
                .or_else(|| r.as_str())
        })
        .filter(|s| !s.is_empty())
    {
        omp_request(
            &stdin,
            &mut lines,
            "switch_session",
            json!({"sessionPath":session}),
            supports_v2,
        )
        .await?;
    }
    if let Some((provider, selected)) = selection
        && !provider.is_empty()
    {
        omp_request(
            &stdin,
            &mut lines,
            "set_model",
            json!({"provider":provider,"modelId":selected}),
            supports_v2,
        )
        .await?;
    }
    omp_request(
        &stdin,
        &mut lines,
        "set_subagent_subscription",
        json!({"level":"events"}),
        supports_v2,
    )
    .await?;
    let gateways: Vec<agent::McpRegistration> = mcp_servers
        .into_iter()
        .filter(|s| s.name.starts_with("muteki-"))
        .collect();
    let gateway = gateways.first().cloned();
    let http_client = crate::network_proxy::http_client_builder()
        .timeout(Duration::from_secs(60))
        .build()
        .ok();
    let mut tool_routes_map: HashMap<String, agent::McpRegistration> = HashMap::new();
    if let Some(client) = &http_client {
        let mut combined_tools = Vec::new();
        for gw in &gateways {
            if let Ok(resp) = client
                .post(&gw.url)
                .bearer_auth(&gw.bearer_token)
                .json(&json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list", "params": {}}))
                .send()
                .await
                && let Ok(body) = resp.json::<Value>().await
                && let Some(tools) = body["result"]["tools"].as_array()
            {
                for tool in tools {
                    if let Some(name) = tool["name"].as_str()
                        && !tool_routes_map.contains_key(name)
                    {
                        tool_routes_map.insert(name.to_owned(), gw.clone());
                        combined_tools.push(json!({
                            "name": tool["name"],
                            "description": tool["description"],
                            "parameters": tool["inputSchema"],
                        }));
                    }
                }
            }
        }
        if !combined_tools.is_empty() {
            omp_request(
                &stdin,
                &mut lines,
                "set_host_tools",
                json!({"tools": combined_tools}),
                supports_v2,
            )
            .await?;
        }
    }
    let tool_routes = Arc::new(tool_routes_map);

    let (cmd_tx, cmd_rx): (Sender<SessionCommand>, Receiver<SessionCommand>) =
        smol::channel::unbounded();
    let (evt_tx, evt_rx): (Sender<AgentEvent>, Receiver<AgentEvent>) = smol::channel::unbounded();
    if let Some(pid)=pid {let _=evt_tx.try_send(AgentEvent::ProcessStarted{pid});}
    let catalog = omp_request(
        &stdin,
        &mut lines,
        "get_available_commands",
        json!({}),
        supports_v2,
    )
    .await?;
    let state = omp_request(&stdin, &mut lines, "get_state", json!({}), supports_v2).await?;
    let session_id = state["sessionId"]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| RuntimeError::new("runtime.protocol_error", "OMP state omitted sessionId"))?
        .to_owned();
    let _ = evt_tx.try_send(AgentEvent::SessionStarted {
        provider_session_id: session_id,
        resume: ResumeCursor(json!({"session_file":state["sessionFile"]})),
        model: state["model"]["id"]
            .as_str()
            .map(str::to_owned)
            .or(selected_model),
    });

    let _ = evt_tx.try_send(omp_events::commands(&catalog));

    let active_turn = Arc::new(std::sync::Mutex::new(String::new()));
    let reader_turn = active_turn.clone();
    let active_prompt = Arc::new(std::sync::Mutex::new(String::new()));
    let reader_prompt = active_prompt.clone();
    let steering = Arc::new(std::sync::Mutex::new(HashMap::<String, String>::new()));
    let reader_steering = steering.clone();
    let interrupted = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let reader_interrupted = interrupted.clone();
    let option_acks = Arc::new(std::sync::Mutex::new(HashMap::<String,tokio::sync::oneshot::Sender<Result<Value,String>>>::new()));
    let reader_option_acks = option_acks.clone();
    let reader_evt_tx = evt_tx.clone();
    let reader_stdin = stdin.clone();
    tokio::spawn(async move {
        let mut reassembler = OmpChunkReassembler::default();
        let mut delegation = omp_events::Delegation::default();
        let mut assistant_buf = String::new();
        let mut message_index = 0_u64;
        let mut failed = false;
        let mut tools: HashMap<String, (String, Value)> = HashMap::new();
        let mut host_calls = tokio::task::JoinSet::new();
        let mut host_cancellations: HashMap<String, tokio::task::AbortHandle> = HashMap::new();
        while let Ok(Some(line)) = lines.next_line().await {
            let Ok(raw_frame) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            let frame = match reassembler.feed(raw_frame) {
                Ok(Some(f)) => f,
                Ok(None) => continue,
                Err(err) => {
                    let _ = reader_evt_tx
                        .send(AgentEvent::Error {
                            message: err.message,
                            fatal: true,
                        })
                        .await;
                    break;
                }
            };
            while host_calls.try_join_next().is_some() {}
            host_cancellations.retain(|_, handle| !handle.is_finished());
            if frame["type"] == "response" {
                let waiter = reader_option_acks.lock().unwrap().remove(frame["id"].as_str().unwrap_or(""));
                if let Some(waiter) = waiter {
                    let result = if frame["success"] == true {Ok(frame["data"].clone())}
                        else {Err(frame["error"].as_str().unwrap_or("OMP rejected the thinking level").to_owned())};
                    let _ = waiter.send(result);
                    continue;
                }
            }
            if frame["type"] == "response" && frame["command"] == "steer" {
                let request = reader_steering
                    .lock()
                    .unwrap()
                    .remove(frame["id"].as_str().unwrap_or(""));
                if let Some(request_id) = request {
                    let event = if frame["success"] == true {
                        AgentEvent::SteerAccepted { request_id }
                    } else {
                        AgentEvent::SteerRejected { request_id }
                    };
                    let _ = reader_evt_tx.send(event).await;
                }
                if frame["success"] == false {
                    let _ = reader_evt_tx
                        .send(AgentEvent::Error {
                            message: frame["error"]
                                .as_str()
                                .unwrap_or("OMP rejected steering")
                                .to_owned(),
                            fatal: false,
                        })
                        .await;
                }
                continue;
            }
            if frame["type"] == "response"
                && frame["success"] == false
                && frame["command"] == "compact"
            {
                let events = delegation.project(&frame).unwrap_or_default();
                if events.is_empty() {
                    continue;
                }
                let _ = reader_evt_tx
                    .send(AgentEvent::Error {
                        message: frame["error"].as_str().unwrap_or("OMP compaction failed").to_owned(),
                        fatal: false,
                    })
                    .await;
                for event in events {
                    let _ = reader_evt_tx.send(event).await;
                }
                if reader_turn.lock().unwrap().is_empty() {
                    let _ = reader_evt_tx
                        .send(AgentEvent::TurnCompleted {
                            turn_id: format!("compact:{}", frame["id"].as_str().unwrap_or("")),
                            status: TurnStatus::Failed,
                            usage: None,
                        })
                        .await;
                }
                continue;
            }
            if frame["type"] == "response"
                && frame["success"] == false
                && frame["command"] != "prompt"
            {
                if frame["command"] == "abort" {
                    reader_interrupted.store(false, std::sync::atomic::Ordering::Relaxed);
                }
                let _ = reader_evt_tx
                    .send(AgentEvent::Error {
                        message: frame["error"]
                            .as_str()
                            .unwrap_or("OMP control command failed")
                            .to_owned(),
                        fatal: false,
                    })
                    .await;
                continue;
            }
            if let Some(events) = delegation.project(&frame) {
                for event in events {
                    let _ = reader_evt_tx.send(event).await;
                }
                continue;
            }
            let turn_id = reader_turn.lock().unwrap().clone();
            match frame["type"].as_str().unwrap_or("") {
                "host_tool_call" => {
                    let call_id = frame["id"]
                        .as_str()
                        .or_else(|| frame["callId"].as_str())
                        .unwrap_or("")
                        .to_owned();
                    let tool_name = frame["name"]
                        .as_str()
                        .or_else(|| frame["toolName"].as_str())
                        .unwrap_or("")
                        .to_owned();
                    let args = frame
                        .get("arguments")
                        .or_else(|| frame.get("args"))
                        .cloned()
                        .unwrap_or_else(|| json!({}));
                    let client = http_client.clone();
                    let gateway = tool_routes
                        .get(&tool_name)
                        .cloned()
                        .or_else(|| gateway.clone());
                    let output = reader_stdin.clone();
                    let key = call_id.clone();
                    let task = host_calls.spawn(async move {
                        let result = async {
                            let gw = gateway.as_ref().ok_or("gateway unavailable".to_owned())?;
                            let client = client.as_ref().ok_or("HTTP client unavailable".to_owned())?;
                            let response = client.post(&gw.url).bearer_auth(&gw.bearer_token)
                                .json(&json!({"jsonrpc":"2.0","id":call_id,"method":"tools/call",
                                    "params":{"name":tool_name,"arguments":args}}))
                                .send().await.map_err(|error|error.to_string())?
                                .error_for_status().map_err(|error|error.to_string())?;
                            response.json::<Value>().await.map_err(|error|error.to_string())
                        }.await;
                        let body = result.unwrap_or_else(|error|json!({"error":{"message":error}}));
                        let tool_result = body.get("result").cloned().unwrap_or(body);
                        let is_error = tool_result["isError"] == true || tool_result.get("error").is_some();
                        let tool_result = if tool_result["content"].is_array() { tool_result }
                            else { json!({"content":[{"type":"text","text":tool_result.to_string()}],"details":tool_result}) };
                        let _ = write_omp(output,json!({"id":call_id,"type":"host_tool_result",
                            "result":tool_result,"isError":is_error}),supports_v2).await;
                    });
                    host_cancellations.insert(key, task);
                }
                "host_tool_cancel" => {
                    if let Some(task) =
                        host_cancellations.remove(frame["targetId"].as_str().unwrap_or(""))
                    {
                        task.abort();
                    }
                }
                "extension_ui_request" => {
                    let req_id = frame["id"].as_str().unwrap_or("").to_owned();
                    let _ = write_omp(
                        reader_stdin.clone(),
                        json!({"id": req_id, "type": "extension_ui_response", "cancelled": true}),
                        supports_v2,
                    )
                    .await;
                }
                "message_update" => {
                    let delta_event = &frame["assistantMessageEvent"];
                    let kind = if delta_event["type"] == "thinking_delta" {
                        DeltaKind::ReasoningText
                    } else {
                        DeltaKind::AssistantText
                    };
                    if let Some(delta) = delta_event["delta"]
                        .as_str()
                        .or_else(|| frame["delta"].as_str())
                        .filter(|v| !v.is_empty())
                    {
                        if kind == DeltaKind::AssistantText {
                            assistant_buf.push_str(delta);
                        }
                        let _ = reader_evt_tx
                            .send(AgentEvent::Delta {
                                item_id: format!("{turn_id}:msg-{message_index}"),
                                kind,
                                text: delta.to_owned(),
                            })
                            .await;
                    }
                }
                "message_end" if frame["message"]["role"] == "assistant" => {
                    let message = &frame["message"];
                    let text = message["content"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter(|b| b["type"] == "text")
                        .filter_map(|b| b["text"].as_str())
                        .collect::<String>();
                    if message["stopReason"] == "aborted" {
                        reader_interrupted.store(true, std::sync::atomic::Ordering::Relaxed);
                    }
                    if !reader_interrupted.load(std::sync::atomic::Ordering::Relaxed)
                        && (message["stopReason"] == "error"
                            || message["errorMessage"]
                                .as_str()
                                .is_some_and(|e| !e.is_empty()))
                    {
                        failed = true;
                        let _ = reader_evt_tx
                            .send(AgentEvent::TurnFailed {
                                code: "runtime.turn_failed".into(),
                                message: message["errorMessage"]
                                    .as_str()
                                    .unwrap_or("OMP assistant failed")
                                    .to_owned(),
                            })
                            .await;
                    }
                    if !text.is_empty() || !assistant_buf.is_empty() {
                        let _ = reader_evt_tx
                            .send(AgentEvent::ItemCompleted(ThreadItem {
                                id: format!("{turn_id}:msg-{message_index}"),
                                parent_item_id: None,
                                content: ItemContent::AssistantMessage {
                                    text: if text.is_empty() {
                                        std::mem::take(&mut assistant_buf)
                                    } else {
                                        text
                                    },
                                },
                            }))
                            .await;
                    }
                    assistant_buf.clear();
                    message_index += 1;
                    let usage = &message["usage"];
                    if usage.is_object() {
                        let identity=message["id"].as_str().map(str::to_owned).or_else(||message.get("timestamp").filter(|value|!value.is_null()).map(Value::to_string)).unwrap_or_else(||format!("{turn_id}:message-{message_index}"));
                        let _=reader_evt_tx.send(AgentEvent::UsageDetail{scope:"message".into(),usage_id:identity,usage:json!({"input_tokens":usage["input"],"output_tokens":usage["output"],"cache_read_tokens":usage["cacheRead"],"cache_write_tokens":usage["cacheWrite"],"reasoning_tokens":usage["reasoning"],"reported_cost":usage["cost"]["total"],"cost_kind":"estimated","input_includes_cache":false})}).await;
                        let _ = reader_evt_tx
                            .send(AgentEvent::TokenUsage(TokenUsage {
                                input_tokens: usage["input"].as_u64(),
                                output_tokens: usage["output"].as_u64(),
                                cached_input_tokens: usage["cacheRead"].as_u64(),
                                cache_write_tokens: usage["cacheWrite"].as_u64(),
                                reasoning_tokens: usage["reasoning"].as_u64(),
                                cost_usd: usage["cost"]["total"].as_f64(),
                                ..Default::default()
                            }))
                            .await;
                    }
                }
                "tool_execution_update" => {
                    let id = frame["toolCallId"].as_str().unwrap_or("").to_owned();
                    if let Some((name, input)) = tools.get(&id) {
                        let _ = reader_evt_tx
                            .send(AgentEvent::ItemUpdated(ThreadItem {
                                id,
                                parent_item_id: None,
                                content: ItemContent::ToolCall {
                                    presentation: None,
                                    image_reads: Vec::new(),
                                    name: name.clone(),
                                    input: input.clone(),
                                    output: Some(frame["partialResult"].to_string()),
                                    status: ItemStatus::InProgress,
                                },
                            }))
                            .await;
                    }
                }
                "tool_execution_start" | "tool_execution_end" => {
                    let id = frame["toolCallId"].as_str().unwrap_or("").to_owned();
                    let ending = frame["type"] == "tool_execution_end";
                    let (name, input) = if ending {
                        tools.remove(&id).unwrap_or_else(|| {
                            (
                                frame["toolName"].as_str().unwrap_or("").to_owned(),
                                json!({}),
                            )
                        })
                    } else {
                        let value = (
                            frame["toolName"].as_str().unwrap_or("").to_owned(),
                            frame["args"].clone(),
                        );
                        tools.insert(id.clone(), value.clone());
                        value
                    };
                    let item = ThreadItem {
                        id,
                        parent_item_id: None,
                        content: ItemContent::ToolCall {
                            presentation: None,
                            image_reads: Vec::new(),
                            name,
                            input,
                            output: ending.then(|| frame["result"].to_string()),
                            status: if !ending {
                                ItemStatus::InProgress
                            } else if frame["isError"] == true {
                                ItemStatus::Failed
                            } else {
                                ItemStatus::Completed
                            },
                        },
                    };
                    let _ = reader_evt_tx
                        .send(if ending {
                            AgentEvent::ItemCompleted(item)
                        } else {
                            AgentEvent::ItemStarted(item)
                        })
                        .await;
                }
                "agent_end" | "agent_settled" | "prompt_result" | "response" => {
                    let kind = frame["type"].as_str().unwrap_or("");
                    let prompt_response = kind == "response" && frame["command"] == "prompt";
                    let active_request = frame["id"]
                        .as_str()
                        .is_none_or(|id| id == reader_prompt.lock().unwrap().as_str());
                    let rejected = active_request
                        && ((prompt_response && frame["success"] == false)
                            || (kind == "prompt_result" && frame["status"] == "error"));
                    if active_request && kind == "prompt_result" && frame["status"] == "aborted" {
                        reader_interrupted.store(true, std::sync::atomic::Ordering::Relaxed);
                    }
                    let local = active_request
                        && (kind == "prompt_result" || prompt_response)
                        && (frame["agentInvoked"] == false
                            || frame["data"]["agentInvoked"] == false);
                    let terminal = (matches!(kind, "agent_end" | "agent_settled")
                        && frame["isTerminal"] != false)
                        || (kind == "prompt_result" && active_request);
                    if (terminal || local || rejected) && !turn_id.is_empty() {
                        if rejected {
                            failed = true;
                            let _ = reader_evt_tx
                                .send(AgentEvent::TurnFailed {
                                    code: "runtime.turn_failed".into(),
                                    message: frame["error"].to_string(),
                                })
                                .await;
                        }
                        if !assistant_buf.is_empty() {
                            let _ = reader_evt_tx
                                .send(AgentEvent::ItemCompleted(ThreadItem {
                                    id: format!("{turn_id}:msg-{message_index}"),
                                    parent_item_id: None,
                                    content: ItemContent::AssistantMessage {
                                        text: std::mem::take(&mut assistant_buf),
                                    },
                                }))
                                .await;
                        }
                        reader_turn.lock().unwrap().clear();
                        let _ = reader_evt_tx
                            .send(AgentEvent::TurnCompleted {
                                turn_id,
                                status: if reader_interrupted
                                    .swap(false, std::sync::atomic::Ordering::Relaxed)
                                {
                                    TurnStatus::Interrupted
                                } else if failed {
                                    TurnStatus::Failed
                                } else {
                                    TurnStatus::Completed
                                },
                                usage: None,
                            })
                            .await;
                        failed = false;
                        message_index = 0;
                    }
                }
                _ => {}
            }
        }
        let _ = reader_evt_tx
            .send(AgentEvent::SessionClosed {
                reason: Some("omp.rpc_v2 process exited".into()),
            })
            .await;
    });

    tokio::spawn(async move {
        let mut seq = 1u64;
        while let Ok(cmd) = cmd_rx.recv().await {
            seq += 1;
            let id = format!("req-{seq}");
            if let SessionCommand::SetOption{id:option,value}=&cmd && option=="muteki:operation" {
                let request_id=value["request_id"].as_str().unwrap_or("").to_owned();
                let result:Result<Value,String>=async{
                    if !active_turn.lock().unwrap().is_empty(){return Err("OMP native turn is active".into());}
                    let(method,mut params)=agent::native_rpc_operation("omp",value["name"].as_str().unwrap_or(""),value["arguments"].as_str().unwrap_or(""))?;
                    params["id"]=json!(id);params["type"]=json!(method);let(sender,receiver)=tokio::sync::oneshot::channel();option_acks.lock().unwrap().insert(id.clone(),sender);
                    write_omp(stdin.clone(),params,supports_v2).await.map_err(|e|e.message)?;
                    let result=tokio::time::timeout(Duration::from_secs(if method=="compact"{180}else{30}),receiver).await.map_err(|_|"OMP operation response timed out".to_owned())?.map_err(|_|"OMP response channel closed".to_owned())??;
                    Ok(agent::native_rpc_result(method,result))
                }.await;
                option_acks.lock().unwrap().remove(&id);
                let event=match result{Ok(result)=>AgentEvent::RuntimeOperationResult{request_id,result:Some(result),error:None},Err(error)=>AgentEvent::RuntimeOperationResult{request_id,result:None,error:Some(error)}};
                let _=evt_tx.send(event).await;continue;
            }
            match cmd {
                SessionCommand::SendTurn {
                    delivery_id,
                    text,
                    attachments,
                    options,
                } => {
                    let turn_id = format!("turn-{}", uuid::Uuid::new_v4().simple());
                    *active_turn.lock().unwrap() = turn_id.clone();
                    *active_prompt.lock().unwrap() = id.clone();
                    interrupted.store(false, std::sync::atomic::Ordering::Relaxed);
                    let _ = evt_tx
                        .send(AgentEvent::TurnStarted {
                            turn_id: turn_id.clone(),
                        })
                        .await;
                    if let Some(level) = options.as_ref().map(|options|options.effort.as_deref().filter(|level|!level.is_empty()&&*level!="default").unwrap_or("off")) {
                        let request_id = format!("{id}-thinking");
                        let (sender,receiver) = tokio::sync::oneshot::channel();
                        option_acks.lock().unwrap().insert(request_id.clone(),sender);
                        let applied: Result<Value,String> = async {
                            write_omp(stdin.clone(),json!({"id":request_id,"type":"set_thinking_level","level":level}),supports_v2).await.map_err(|error|error.message)?;
                            tokio::time::timeout(Duration::from_secs(25),receiver).await.map_err(|_|"OMP thinking level confirmation timed out".to_owned())?
                                .map_err(|_|"OMP closed before confirming the thinking level".to_owned())?
                        }.await;
                        option_acks.lock().unwrap().remove(&request_id);
                        if let Err(message) = applied {
                            active_turn.lock().unwrap().clear();
                            let _ = evt_tx.send(AgentEvent::TurnFailed {code:"runtime.thinking_level_failed".into(),message}).await;
                            let _ = evt_tx.send(AgentEvent::TurnCompleted {turn_id,status:TurnStatus::Failed,usage:None}).await;
                            continue;
                        }
                    }
                    let mut frame = json!({"id":id,"type":"prompt","message":text});
                    omp_images(&mut frame, attachments);
                    if let Err(error) = write_omp(stdin.clone(), frame, supports_v2).await {
                        active_turn.lock().unwrap().clear();
                        let _ = evt_tx
                            .send(AgentEvent::TurnFailed {
                                code: error.code.into(),
                                message: error.message,
                            })
                            .await;
                        let _ = evt_tx
                            .send(AgentEvent::TurnCompleted {
                                turn_id,
                                status: TurnStatus::Failed,
                                usage: None,
                            })
                            .await;
                        break;
                    }
                    let _ = evt_tx.send(AgentEvent::TurnAccepted { delivery_id }).await;
                }
                SessionCommand::Steer {
                    request_id,
                    text,
                    attachments,
                } => {
                    let mut frame = json!({"id":id,"type":"steer","message":text});
                    omp_images(&mut frame, attachments);
                    steering
                        .lock()
                        .unwrap()
                        .insert(id.clone(), request_id.clone());
                    if let Err(error) = write_omp(stdin.clone(), frame, supports_v2).await {
                        steering.lock().unwrap().remove(&id);
                        let _ = evt_tx.send(AgentEvent::SteerRejected { request_id }).await;
                        let _ = evt_tx
                            .send(AgentEvent::Error {
                                message: error.message,
                                fatal: false,
                            })
                            .await;
                    }
                }
                SessionCommand::Interrupt => {
                    interrupted.store(true, std::sync::atomic::Ordering::Relaxed);
                    if let Err(error) =
                        write_omp(stdin.clone(), json!({"id":id,"type":"abort"}), supports_v2).await
                    {
                        interrupted.store(false, std::sync::atomic::Ordering::Relaxed);
                        let _ = evt_tx
                            .send(AgentEvent::Error {
                                message: error.message,
                                fatal: false,
                            })
                            .await;
                    }
                }
                SessionCommand::Compact => {
                    let _ = write_omp(
                        stdin.clone(),
                        json!({"id": id, "type": "compact"}),
                        supports_v2,
                    )
                    .await;
                }
                SessionCommand::Shutdown => {
                    kill_process_group(pid);
                    let _ = child.kill().await;
                    break;
                }
                _ => {}
            }
        }
        kill_process_group(pid);
        let _ = child.kill().await;
    });

    Ok(SessionHandle {
        provider: ProviderKind::Pi,
        commands: cmd_tx,
        events: evt_rx,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    #[ignore = "requires an installed OMP CLI; exchanges protocol frames without model calls"]
    async fn live_omp_protocol_and_session_handshake() {
        let binary = PathBuf::from(std::env::var("MUTEKI_LIVE_OMP_BIN").expect("OMP binary path"));
        let root = tempfile::tempdir().unwrap();
        let profile = root.path().join("profile");
        std::fs::create_dir_all(&profile).unwrap();
        let source_profile = PathBuf::from(
            std::env::var("MUTEKI_LIVE_OMP_PROFILE").expect("source account profile"),
        );
        std::fs::copy(
            source_profile.join("models.yml"),
            profile.join("models.yml"),
        )
        .unwrap();
        let model = std::env::var("MUTEKI_LIVE_OMP_MODEL").expect("source account model");
        let env = vec![
            ("MUTEKI_OMP_PROVIDER".to_owned(), "muteki".to_owned()),
            ("MUTEKI_OMP_MODEL".to_owned(), model.clone()),
            (
                "PI_CODING_AGENT_DIR".to_owned(),
                profile.to_string_lossy().into_owned(),
            ),
        ];
        let mut cmd = supervised_command(&binary);
        cmd.args(["--mode", "rpc", "--provider", "muteki", "--model", &model])
            .current_dir(root.path());
        apply_env_pairs(&mut cmd, &env);
        let mut child = cmd
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let stdin = Arc::new(Mutex::new(child.stdin.take().unwrap()));
        let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
        let ready: Value = serde_json::from_str(
            &tokio::time::timeout(Duration::from_secs(30), lines.next_line())
                .await
                .unwrap()
                .unwrap()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(ready["type"], "ready");
        let negotiated = omp_request(
            &stdin,
            &mut lines,
            "negotiate_protocol",
            json!({"protocolVersion":2}),
            false,
        )
        .await
        .unwrap();
        assert_eq!(negotiated["protocolVersion"], 2);
        let state = omp_request(
            &stdin,
            &mut lines,
            "get_state",
            json!({"wire_probe":"界".repeat(400_000)}),
            true,
        )
        .await
        .unwrap();
        assert!(state["sessionId"].is_string(), "{state}");
        child.kill().await.unwrap();
        let handle = start_omp_rpc_v2(
            &binary,
            root.path(),
            Some(model),
            &env,
            &[],
            None,
            None,
            Vec::new(),
        )
        .await
        .unwrap();
        assert!(matches!(
            handle.events.recv().await.unwrap(),
            AgentEvent::SessionStarted { .. }
        ));
        handle
            .commands
            .send(SessionCommand::Shutdown)
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(10), async {
            while !handle.commands.is_closed() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
    }

    #[test]
    fn omp_v2_sends_jsonl_and_decodes_stdout_chunks() {
        let frame = json!({"id":"1","type":"response","data":"界".repeat(400_000)});
        let outbound = omp_encode_frames(&frame, true).unwrap();
        assert_eq!(outbound.len(), 1);
        assert_eq!(serde_json::from_str::<Value>(&outbound[0]).unwrap(), frame);
        let bytes = outbound[0].as_bytes();
        let parts: Vec<_> = bytes.chunks(OMP_CHUNK_PAYLOAD_BYTES).collect();
        let mut decoder = OmpChunkReassembler::default();
        let mut decoded = None;
        for (index, part) in parts.iter().enumerate() {
            decoded = decoder
                .feed(json!({"type":"rpc_chunk","chunkId":"upstream-frame",
                "index":index,"count":parts.len(),"byteLength":bytes.len(),
                "data":base64::engine::general_purpose::STANDARD.encode(part)}))
                .unwrap();
        }
        assert_eq!(decoded.unwrap(), frame);
    }

    #[test]
    fn droid_mcp_bridge_writes_0600_connection_file_and_cleans_up() {
        let reg = vec![agent::McpRegistration {
            name: "muteki-control".into(),
            url: "http://127.0.0.1:8100/api/capability".into(),
            bearer_token: "secret-token".into(),
        }];
        let mut bridge = shape_droid_mcp(&reg).unwrap();
        assert_eq!(bridge.servers.len(), 1);
        assert_eq!(bridge.servers[0]["type"], "stdio");
        assert_eq!(bridge.servers[0]["name"], "muteki-control");
        assert!(bridge.servers[0]["env"].is_object());
        let dir = bridge.data_dir.clone().unwrap();
        let cfg = dir.join("connection.json");
        assert!(cfg.is_file());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&cfg).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        bridge.cleanup();
        assert!(!dir.exists());
    }

    #[test]
    fn kimi_permission_mode_and_git_worktree_check() {
        assert_eq!(kimi_permission_mode("supervised").unwrap(), "manual");
        assert_eq!(kimi_permission_mode("auto-accept-edits").unwrap(), "manual");
        assert_eq!(kimi_permission_mode("auto").unwrap(), "auto");
        assert_eq!(kimi_permission_mode("full-access").unwrap(), "yolo");
        let dir = tempfile::tempdir().unwrap();
        assert!(validate_kimi_access_mode("kimi.local_server", "supervised", dir.path()).is_ok());
        std::fs::create_dir_all(dir.path().join(".git")).unwrap();
        assert_eq!(
            validate_kimi_access_mode("kimi.local_server", "supervised", dir.path())
                .unwrap_err()
                .code,
            "capability.unsupported"
        );
        assert!(
            validate_kimi_access_mode("kimi.local_server", "auto-accept-edits", dir.path()).is_ok()
        );
    }
}
