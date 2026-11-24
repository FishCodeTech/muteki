//! Docker container worker execution, Reverse Control Plane (RCP) receiver,
//! worker image inspection, network mode resolution, and resource limits.

use crate::AppState;
use crate::agent_runtime::{RuntimeError, credentials, descriptors};
use crate::native_chat;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Mutex as AsyncMutex, oneshot};

pub const DEFAULT_WORKER_IMAGE: &str = "muteki-worker:latest";
pub const CONTAINER_WORKSPACE: &str = "/home/kali/workspace";
pub const CONTAINER_CONTROL_DIR: &str = "/run/muteki/control";
pub const CONTAINER_ACCOUNTS_ROOT: &str = "/run/muteki/accounts";
pub const SHARED_CONTAINER_RUN_ID: &str = "__shared_pool__";
pub const SHARED_CONTAINER_NAME: &str = "muteki-worker-shared";
pub const SHARED_CONTAINER_WORKSPACE: &str = "/home/kali/workspace/runs";

const MIB: u64 = 1024 * 1024;
const GIB: u64 = 1024 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkerResourceLimits {
    pub memory: String,
    pub memory_bytes: u64,
    pub cpus: f64,
    pub pids_limit: i64,
    pub output_limit: String,
    pub output_limit_bytes: u64,
    pub disk_limit: String,
    pub disk_limit_bytes: u64,
}

impl WorkerResourceLimits {
    pub fn full_defaults() -> Self {
        Self {
            memory: "2g".to_owned(),
            memory_bytes: 2 * GIB,
            cpus: 2.0,
            pids_limit: 512,
            output_limit: "64m".to_owned(),
            output_limit_bytes: 64 * MIB,
            disk_limit: "4g".to_owned(),
            disk_limit_bytes: 4 * GIB,
        }
    }

    pub fn docker_cpus(&self) -> String {
        self.cpus.to_string()
    }

    pub fn to_settings_fields(&self) -> Value {
        json!({
            "worker_memory": self.memory,
            "worker_cpus": self.cpus,
            "worker_pids_limit": self.pids_limit,
            "worker_output_limit": self.output_limit,
            "worker_disk_limit": self.disk_limit,
        })
    }
}

pub fn parse_byte_size(raw: &Value, field_name: &str) -> Result<(String, u64), String> {
    if raw.is_boolean() {
        return Err(format!(
            "{field_name} must be a byte size such as '8g' or '512m'"
        ));
    }
    let text = raw
        .as_str()
        .map(str::to_owned)
        .unwrap_or_else(|| raw.to_string())
        .trim()
        .to_ascii_lowercase();
    if text.is_empty() {
        return Err(format!("{field_name} cannot be empty"));
    }
    let numeric_len = text
        .bytes()
        .take_while(|byte| byte.is_ascii_digit() || *byte == b'.')
        .count();
    let (amount, unit) = text.split_at(numeric_len);
    let scale = match unit.trim() {
        "" | "b" => 1_u128,
        "k" | "kb" | "ki" | "kib" => 1024_u128,
        "m" | "mb" | "mi" | "mib" => 1024_u128.pow(2),
        "g" | "gb" | "gi" | "gib" => 1024_u128.pow(3),
        "t" | "tb" | "ti" | "tib" => 1024_u128.pow(4),
        "p" | "pb" | "pi" | "pib" => 1024_u128.pow(5),
        "e" | "eb" | "ei" | "eib" => 1024_u128.pow(6),
        _ => return Err(format!("{field_name} has invalid byte size {text:?}")),
    };
    let (whole, fraction) = amount.split_once('.').unwrap_or((amount, ""));
    let invalid = || format!("{field_name} has invalid byte size {text:?}");
    if whole.is_empty()
        || !whole.bytes().all(|byte| byte.is_ascii_digit())
        || !fraction.bytes().all(|byte| byte.is_ascii_digit())
        || amount.ends_with('.')
    {
        return Err(invalid());
    }
    let whole = whole.parse::<u128>().map_err(|_| invalid())?;
    let fraction = fraction.trim_end_matches('0');
    let divisor = 10_u128
        .checked_pow(fraction.len() as u32)
        .ok_or_else(invalid)?;
    let fractional = if fraction.is_empty() {
        0
    } else {
        fraction
            .parse::<u128>()
            .map_err(|_| invalid())?
            .checked_mul(scale)
            .ok_or_else(invalid)?
    };
    if fractional % divisor != 0 {
        return Err(format!(
            "{field_name} must be a positive whole byte count within int64"
        ));
    }
    let bytes = whole
        .checked_mul(scale)
        .and_then(|value| value.checked_add(fractional / divisor))
        .filter(|value| (1..=i64::MAX as u128).contains(value))
        .ok_or_else(|| format!("{field_name} must be a positive whole byte count within int64"))?
        as u64;
    Ok((text.replace(' ', ""), bytes))
}

pub fn resolve_worker_resource_limits(
    settings: Option<&Value>,
) -> Result<WorkerResourceLimits, String> {
    let base = WorkerResourceLimits::full_defaults();

    let get_val = |cfg_key: &str, env_key: &str| -> Option<Value> {
        if let Some(cfg) = settings
            && let Some(v) = cfg.get(cfg_key)
            && !v.is_null()
        {
            return Some(v.clone());
        }
        std::env::var(env_key)
            .ok()
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty())
            .map(Value::String)
    };

    let (memory, memory_bytes) = match get_val("worker_memory", "MUTEKI_WORKER_MEMORY") {
        Some(v) => parse_byte_size(&v, "worker_memory")?,
        None => (base.memory, base.memory_bytes),
    };
    let cpus = match get_val("worker_cpus", "MUTEKI_WORKER_CPUS") {
        Some(v) => {
            if v.is_boolean() {
                return Err("worker_cpus must be a number".to_owned());
            }
            let num = v
                .as_f64()
                .or_else(|| v.as_str().and_then(|s| s.trim().parse::<f64>().ok()))
                .ok_or_else(|| "worker_cpus must be a number".to_owned())?;
            if !num.is_finite() || num <= 0.0 {
                return Err("worker_cpus must be finite and > 0".to_owned());
            }
            num
        }
        None => base.cpus,
    };
    let pids_limit = match get_val("worker_pids_limit", "MUTEKI_WORKER_PIDS")
        .or_else(|| get_val("worker_pids_limit", "MUTEKI_WORKER_PIDS_LIMIT"))
    {
        Some(v) => {
            if v.is_boolean() {
                return Err("worker_pids_limit must be an integer".to_owned());
            }
            let num = v
                .as_i64()
                .or_else(|| v.as_str().and_then(|s| s.trim().parse::<i64>().ok()))
                .ok_or_else(|| "worker_pids_limit must be an integer".to_owned())?;
            if num <= 0
                || v.as_str()
                    .is_some_and(|value| value.trim() != num.to_string())
            {
                return Err("worker_pids_limit must be a positive integer".to_owned());
            }
            num
        }
        None => base.pids_limit,
    };
    let (output_limit, output_limit_bytes) =
        match get_val("worker_output_limit", "MUTEKI_WORKER_OUTPUT_LIMIT") {
            Some(v) => parse_byte_size(&v, "worker_output_limit")?,
            None => (base.output_limit, base.output_limit_bytes),
        };
    let (disk_limit, disk_limit_bytes) =
        match get_val("worker_disk_limit", "MUTEKI_WORKER_DISK_LIMIT") {
            Some(v) => parse_byte_size(&v, "worker_disk_limit")?,
            None => (base.disk_limit, base.disk_limit_bytes),
        };

    Ok(WorkerResourceLimits {
        memory,
        memory_bytes,
        cpus,
        pids_limit,
        output_limit,
        output_limit_bytes,
        disk_limit,
        disk_limit_bytes,
    })
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerNetworkResolution {
    pub requested: String,
    pub effective: Option<String>,
    pub reason: Option<String>,
    pub error: Option<String>,
}

pub fn is_running_in_container() -> bool {
    Path::new("/.dockerenv").exists()
        || std::fs::read_to_string("/proc/1/cgroup")
            .map(|t| t.contains("docker") || t.contains("containerd") || t.contains("kubepods"))
            .unwrap_or(false)
}

pub fn project_worker_network(requested: &str) -> WorkerNetworkResolution {
    let raw = requested.trim().to_ascii_lowercase();
    let mode = if raw.is_empty() {
        "bridge"
    } else {
        raw.as_str()
    };
    let managed_gateway = managed_gateway_name(mode);
    if !["bridge", "host", "none"].contains(&mode) && managed_gateway.is_none() {
        return WorkerNetworkResolution {
            requested: mode.to_owned(),
            effective: None,
            reason: None,
            error: Some(format!("不支持的 Worker 网络模式: {requested:?}")),
        };
    }
    let override_network = std::env::var("MUTEKI_WORKER_NETWORK").unwrap_or_default();
    let override_network = override_network.trim();
    if managed_gateway.is_some() {
        return WorkerNetworkResolution {
            requested: mode.to_owned(),
            effective: Some(if mode.starts_with("src-record:") {
                "bridge".to_owned()
            } else {
                mode.to_owned()
            }),
            reason: Some(if mode.starts_with("src-record:") {
                "SRC declared traffic recording".to_owned()
            } else {
                "SRC enforced gateway".to_owned()
            }),
            error: None,
        };
    }
    if mode == "host" {
        return WorkerNetworkResolution {
            requested: "host".to_owned(),
            effective: Some("host".to_owned()),
            reason: None,
            error: None,
        };
    }
    if mode == "none" {
        if !override_network.is_empty() {
            return WorkerNetworkResolution {
                requested: "none".to_owned(),
                effective: None,
                reason: None,
                error: Some("worker_network=none conflicts with MUTEKI_WORKER_NETWORK".to_owned()),
            };
        }
        return WorkerNetworkResolution {
            requested: "none".to_owned(),
            effective: Some("none".to_owned()),
            reason: Some("docker --network none".to_owned()),
            error: None,
        };
    }
    if matches!(override_network, "host" | "none") || override_network.starts_with("container:") {
        return WorkerNetworkResolution {
            requested: "bridge".to_owned(),
            effective: None,
            reason: None,
            error: Some("MUTEKI_WORKER_NETWORK must name a bridge network".to_owned()),
        };
    }
    WorkerNetworkResolution {
        requested: "bridge".to_owned(),
        effective: Some(if override_network.is_empty() {
            "bridge".to_owned()
        } else {
            override_network.to_owned()
        }),
        reason: (!override_network.is_empty())
            .then(|| "MUTEKI_WORKER_NETWORK compose shared network".to_owned()),
        error: None,
    }
}

fn managed_gateway_name(mode: &str) -> Option<&str> {
    let (kind, name) = mode.split_once(':')?;
    let hex = name.strip_prefix("muteki-src-egress-")?;
    (matches!(kind, "container" | "src-record")
        && hex.len() == 24
        && hex.bytes().all(|byte| byte.is_ascii_hexdigit()))
    .then_some(name)
}

struct GatewayMount {
    root: PathBuf,
    proxy: Option<String>,
}

async fn managed_gateway_mount(requested: &str) -> Result<Option<GatewayMount>, String> {
    let Some(name) = managed_gateway_name(requested) else {
        return Ok(None);
    };
    let output = crate::host_process::command("docker")
        .args(["inspect", name])
        .output()
        .await
        .map_err(|error| format!("SRC gateway inspection failed: {error}"))?;
    if !output.status.success() {
        return Err(format!("SRC gateway {name} is unavailable"));
    }
    let rows: Vec<Value> = serde_json::from_slice(&output.stdout)
        .map_err(|error| format!("SRC gateway inspection is invalid: {error}"))?;
    let info = rows.first().ok_or("SRC gateway inspection is empty")?;
    if info["Config"]["Labels"]["muteki.src.gateway_schema"] != "rust1"
        || info["State"]["Running"] != true
    {
        return Err("SRC gateway is not an active managed gateway".to_owned());
    }
    let root = info["Mounts"]
        .as_array()
        .and_then(|mounts| {
            mounts
                .iter()
                .find(|mount| mount["Destination"] == "/run/src" && mount["Type"] == "bind")
        })
        .and_then(|mount| mount["Source"].as_str())
        .map(PathBuf::from)
        .ok_or("SRC gateway state mount is missing")?;
    if !root.join("policy.json").is_file() {
        return Err("SRC gateway policy is unavailable".to_owned());
    }
    let proxy = if requested.starts_with("src-record:") {
        let address = info["NetworkSettings"]["IPAddress"].as_str().unwrap_or("");
        if address.is_empty() {
            return Err("SRC gateway proxy address is unavailable".to_owned());
        }
        Some(format!("http://{address}:18080"))
    } else {
        None
    };
    Ok(Some(GatewayMount { root, proxy }))
}

pub fn resolve_worker_image() -> String {
    if let Ok(img) = std::env::var("MUTEKI_WORKER_IMAGE") {
        let trimmed = img.trim();
        if !trimmed.is_empty() {
            return trimmed.to_owned();
        }
    }
    DEFAULT_WORKER_IMAGE.to_owned()
}

pub async fn inspect_worker_image_status() -> Value {
    if let Some(socket) = crate::runner::configured_socket() {
        return match crate::runner::request(&socket, "/v1/worker-image").await {
            Ok(value) => value,
            Err(error) => {
                json!({"daemon":{"ok":false,"detail":error},"pulled":{"ok":false},"overall":"red"})
            }
        };
    }
    inspect_worker_image_status_local().await
}

pub(crate) async fn inspect_worker_image_status_local() -> Value {
    let preferred = resolve_worker_image();
    let expected_version = std::env::var("MUTEKI_WORKER_IMAGE_VERSION")
        .ok()
        .filter(|version| !version.trim().is_empty());
    let daemon_out = crate::host_process::command("docker")
        .args(["info", "--format", "{{.ServerVersion}}"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()
        .await;
    let (daemon_ok, daemon_detail) = match daemon_out {
        Ok(o) if o.status.success() => (
            true,
            format!("Docker {}", String::from_utf8_lossy(&o.stdout).trim()),
        ),
        Ok(o) => (false, String::from_utf8_lossy(&o.stderr).trim().to_owned()),
        Err(e) => (false, e.to_string()),
    };
    if !daemon_ok {
        return json!({
            "image": preferred,
            "daemon": {"ok": false, "detail": daemon_detail},
            "pulled": {"ok": false, "detail": "Docker daemon 未就绪"},
            "version": {"status": "unknown", "expected": expected_version, "actual": null, "detail": ""},
            "overall": "red",
        });
    }

    let inspect_json = match crate::host_process::command("docker")
        .args(["image", "inspect", &preferred])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()
        .await
    {
        Ok(output) if output.status.success() => {
            serde_json::from_slice::<Vec<Value>>(&output.stdout)
                .ok()
                .and_then(|rows| rows.into_iter().next())
        }
        _ => None,
    };

    let Some(info) = inspect_json else {
        return json!({
            "image": preferred,
            "daemon": {"ok": true, "detail": daemon_detail},
            "pulled": {"ok": false, "detail": format!("本地未找到镜像 {preferred}")},
            "version": {
                "status": "unknown",
                "expected": expected_version,
                "actual": null,
                "detail": "请构建或拉取 Worker 镜像"
            },
            "overall": "red",
        });
    };

    let size_mb = info["Size"]
        .as_u64()
        .map(|b| format!("{:.0} MB", b as f64 / (1024.0 * 1024.0)))
        .unwrap_or_default();
    let pulled_detail = if size_mb.is_empty() {
        preferred.clone()
    } else {
        format!("{preferred} ({size_mb})")
    };

    let actual_version = info["Config"]["Labels"]["org.opencontainers.image.version"]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty() && *s != "dev")
        .map(str::to_owned);

    let (ver_status, ver_detail, overall) = match (&expected_version, &actual_version) {
        (None, _) => (
            "unknown",
            "未配置预期版本；当前镜像按配置的标签使用".to_owned(),
            "green",
        ),
        (Some(expected), Some(actual)) if actual == expected => {
            ("match", format!("镜像版本 {actual} 与预期一致"), "green")
        }
        (Some(expected), _) => (
            "mismatch",
            format!(
                "镜像版本 {}，预期 {expected}",
                actual_version.as_deref().unwrap_or("未标记")
            ),
            "yellow",
        ),
    };

    let capabilities = match info["Id"].as_str() {
        Some(id) => match super::image_capabilities::cached(id).await {
            Ok(Some(manifest)) => json!({
                "declared": true,
                "architecture": manifest["architecture"],
                "engines": manifest["engines"].as_object().map(|rows| rows.iter().map(|(name, row)| (name.clone(), json!({"available": row["available"], "version": row["version"]}))).collect::<serde_json::Map<_, _>>()),
                "browser": manifest["browser"].as_object().map(|rows| rows.iter().map(|(name, row)| (name.clone(), row["available"].clone())).collect::<serde_json::Map<_, _>>()),
                "tools": manifest["tools"],
            }),
            Ok(None) => json!({"declared": false}),
            Err(error) => json!({"declared": true, "error": error}),
        },
        None => Value::Null,
    };
    json!({
        "image": preferred,
        "daemon": {"ok": true, "detail": daemon_detail},
        "pulled": {"ok": true, "detail": pulled_detail},
        "image_id": info["Id"],
        "architecture": info["Architecture"],
        "created": info["Created"],
        "size_bytes": info["Size"],
        "capabilities": capabilities,
        "version": {
            "status": ver_status,
            "expected": expected_version,
            "actual": actual_version,
            "detail": ver_detail,
        },
        "overall": overall,
    })
}

pub async fn pull_or_verify_worker_image() -> (bool, Value) {
    if let Some(socket) = crate::runner::configured_socket() {
        return match crate::runner::exchange(&socket, "/v1/worker-image/pull", Some(json!({})))
            .await
        {
            Ok(value) => (value["ok"] == true, value["result"].clone()),
            Err(error) => (false, json!({"ok":false,"detail":error})),
        };
    }
    let status = inspect_worker_image_status().await;
    if status["pulled"]["ok"].as_bool() == Some(true) {
        let image = status["image"]
            .as_str()
            .unwrap_or(DEFAULT_WORKER_IMAGE)
            .to_owned();
        let version = status["version"]["actual"]
            .as_str()
            .unwrap_or("latest")
            .to_owned();
        let detail = status["pulled"]["detail"]
            .as_str()
            .unwrap_or("已就绪")
            .to_owned();
        return (
            true,
            json!({
                "ok": true,
                "image": image,
                "version": version,
                "detail": format!("本地镜像已就绪: {detail}"),
            }),
        );
    }
    let image = resolve_worker_image();
    let out = crate::host_process::command("docker")
        .args(["pull", &image])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()
        .await;
    match out {
        Ok(o) if o.status.success() => {
            let refreshed = inspect_worker_image_status().await;
            (
                true,
                json!({
                    "ok": true,
                    "image": image,
                    "version": refreshed["version"]["actual"],
                    "detail": String::from_utf8_lossy(&o.stdout).lines().last().unwrap_or("拉取成功").trim(),
                }),
            )
        }
        Ok(o) => {
            let err_text = String::from_utf8_lossy(&o.stderr).trim().to_owned();
            (
                false,
                json!({
                    "ok": false,
                    "image": image,
                    "detail": if err_text.is_empty() { "docker pull 失败".to_owned() } else { err_text },
                }),
            )
        }
        Err(e) => (
            false,
            json!({
                "ok": false,
                "image": image,
                "detail": e.to_string(),
            }),
        ),
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerSpec {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reservation_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub owner_run_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub owner_token: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub owner_workspace: Option<String>,
    pub argv: Vec<String>,
    pub cwd: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stdin: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stdin_mode: Option<String>,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub env: HashMap<String, String>,
    pub timeout_sec: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tag: Option<String>,
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub output_limit_bytes: u64,
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub disk_limit_bytes: u64,
}

fn is_zero_u64(v: &u64) -> bool {
    *v == 0
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SupervisorFrame {
    #[serde(default)]
    pub t: String,
    #[serde(default)]
    pub req_id: i64,
    #[serde(default)]
    pub worker_id: String,
    #[serde(default)]
    pub tag: String,
    #[serde(default)]
    pub line: String,
    #[serde(default)]
    pub error: String,
    #[serde(default)]
    pub rc: i32,
    #[serde(default)]
    pub oom: bool,
    #[serde(default)]
    pub timed_out: bool,
    #[serde(default)]
    pub output_limit: bool,
    #[serde(default)]
    pub disk_limit: bool,
    #[serde(default)]
    pub signalled: i32,
    #[serde(default)]
    pub ok: bool,
    #[serde(default)]
    pub state: String,
    #[serde(default)]
    pub paused: bool,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub scoped_teardown: bool,
    #[serde(default)]
    pub scoped_processes: bool,
    #[serde(default)]
    pub execution_lease: bool,
    #[serde(default)]
    pub workers: i64,
    #[serde(default)]
    pub uptime_sec: i64,
    #[serde(default)]
    pub memory_sampled: bool,
    #[serde(default)]
    pub host_memory_total_bytes: u64,
    #[serde(default)]
    pub host_memory_available_bytes: u64,
}

pub struct SupervisorLink {
    pub run_id: String,
    pub peer_version: String,
    writer: AsyncMutex<Option<tokio::net::tcp::OwnedWriteHalf>>,
    runner_socket: Option<PathBuf>,
    pub(crate) remote: Option<Arc<crate::runner_remote::controller::Remote>>,
    runner_generation: u64,
    next_req_id: AtomicI64,
    pending_reqs: Mutex<HashMap<i64, oneshot::Sender<SupervisorFrame>>>,
    worker_streams: Mutex<HashMap<String, super::supervisor_stream::Sender>>,
    early_frames: Mutex<HashMap<String, Vec<SupervisorFrame>>>,
    worker_owners: Mutex<HashMap<String, (String, String)>>,
    closed: Arc<AtomicBool>,
}

impl SupervisorLink {
    fn remote(context: Arc<crate::runner_remote::controller::Remote>) -> Arc<Self> {
        Arc::new(Self {
            run_id: context.scope.run_id.clone(),
            peer_version: "remote-runner-v1".into(),
            writer: AsyncMutex::new(None),
            runner_socket: None,
            runner_generation: context.scope.generation,
            remote: Some(context),
            next_req_id: AtomicI64::new(1),
            pending_reqs: Mutex::new(HashMap::new()),
            worker_streams: Mutex::new(HashMap::new()),
            early_frames: Mutex::new(HashMap::new()),
            worker_owners: Mutex::new(HashMap::new()),
            closed: Arc::new(AtomicBool::new(false)),
        })
    }
    fn runner(run_id: String, socket: PathBuf, generation: u64) -> Arc<Self> {
        Arc::new(Self {
            run_id,
            peer_version: "runner-v1".into(),
            writer: AsyncMutex::new(None),
            runner_socket: Some(socket),
            remote: None,
            runner_generation: generation,
            next_req_id: AtomicI64::new(1),
            pending_reqs: Mutex::new(HashMap::new()),
            worker_streams: Mutex::new(HashMap::new()),
            early_frames: Mutex::new(HashMap::new()),
            worker_owners: Mutex::new(HashMap::new()),
            closed: Arc::new(AtomicBool::new(false)),
        })
    }
    async fn start_via_runner(
        &self,
        socket: &Path,
        spec: &WorkerSpec,
    ) -> Result<(String, super::supervisor_stream::Stream), String> {
        let command_id = uuid::Uuid::new_v4().to_string();
        let response=crate::runner::exchange(socket,"/v1/workers/start",Some(json!({"command_id":command_id,"run_id":self.run_id,"spec":spec,"generation":self.runner_generation}))).await;
        let value = match response {
            Ok(value) => value,
            Err(cause) => {
                let digest = crate::runner::canonical_digest(
                    json!({"run_id":self.run_id,"generation":self.runner_generation,"spec":spec}),
                )?;
                match crate::runner::request(socket, &format!("/v1/commands/{command_id}")).await {
                    Ok(receipt)
                        if receipt["state"] == "started"
                            && receipt["digest"] == digest
                            && receipt["run_id"] == self.run_id
                            && receipt["generation"] == self.runner_generation =>
                    {
                        receipt["result"].clone()
                    }
                    _ => {
                        return Err(format!(
                            "Runner start {command_id} was not confirmed; reconcile its receipt before retrying: {cause}"
                        ));
                    }
                }
            }
        };
        let worker = value["worker_id"]
            .as_str()
            .ok_or("Runner omitted worker ID")?
            .to_owned();
        let socket = socket.to_owned();
        let id = worker.clone();
        let closed = self.closed.clone();
        let (tx, rx) = super::supervisor_stream::channel();
        tokio::spawn(async move {
            let mut after = 0_u64;
            let mut failed_since: Option<tokio::time::Instant> = None;
            loop {
                let response = tokio::time::timeout(
                    Duration::from_secs(5),
                    crate::runner::request(
                        &socket,
                        &format!("/v1/workers/{id}/events?after={after}"),
                    ),
                )
                .await;
                let page = match response {
                    Ok(Ok(page)) => {
                        failed_since = None;
                        page
                    }
                    error => {
                        let since = failed_since.get_or_insert_with(tokio::time::Instant::now);
                        if since.elapsed() >= Duration::from_secs(30) {
                            tracing::error!(
                                ?error,
                                "Runner stream reconnect grace expired; outcome unknown"
                            );
                            closed.store(true, Ordering::SeqCst);
                            break;
                        }
                        tokio::time::sleep(Duration::from_millis(500)).await;
                        continue;
                    }
                };
                let mut terminal = false;
                for value in page["frames"].as_array().into_iter().flatten() {
                    let frame = match serde_json::from_value::<SupervisorFrame>(value.clone()) {
                        Ok(frame) => frame,
                        Err(_) => {
                            closed.store(true, Ordering::SeqCst);
                            return;
                        }
                    };
                    terminal |= matches!(frame.t.as_str(), "exit" | "transport_error");
                    if frame.t == "transport_error" {
                        closed.store(true, Ordering::SeqCst);
                    }
                    if tx.send_async(frame).await.is_err() {
                        return;
                    }
                }
                after = page["after"].as_u64().unwrap_or(after);
                if terminal {
                    break;
                }
                if page["execution_stopped"] == true {
                    closed.store(true, Ordering::SeqCst);
                    break;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        });
        Ok((worker, rx))
    }
    fn route_frame(&self, frame: SupervisorFrame) -> bool {
        match frame.t.as_str() {
            "started" | "resp" => {
                if let Ok(mut pending) = self.pending_reqs.lock()
                    && let Some(tx) = pending.remove(&frame.req_id)
                {
                    let _ = tx.send(frame);
                }
            }
            "out" | "err" | "exit" => {
                let wid = frame.worker_id.clone();
                if wid.is_empty() {
                    return true;
                }
                let Ok(mut streams) = self.worker_streams.lock() else {
                    return false;
                };
                let sender = if frame.t == "exit" {
                    streams.remove(&wid)
                } else {
                    streams.get(&wid).cloned()
                };
                if let Some(tx) = sender {
                    drop(streams);
                    let _ = tx.send(frame);
                } else {
                    let Ok(mut early) = self.early_frames.lock() else {
                        return false;
                    };
                    if !early.contains_key(&wid) && early.len() >= 64 {
                        return false;
                    }
                    let full = early.values().map(Vec::len).sum::<usize>()
                        >= super::supervisor_stream::EARLY_FRAME_CAPACITY;
                    let frames = early.entry(wid.clone()).or_default();
                    if frames.iter().any(|f| f.t == "transport_error") {
                        return true;
                    }
                    if full {
                        frames.clear();
                        frames.push(SupervisorFrame { t: "transport_error".into(), worker_id: wid,
                            error: "Worker output arrived faster than consumer registration; bounded early buffer exhausted".into(),
                            ..Default::default() });
                    } else {
                        frames.push(frame);
                    }
                }
            }
            _ => {}
        }
        true
    }

    pub fn is_alive(&self) -> bool {
        !self.closed.load(Ordering::SeqCst)
    }

    async fn call(
        &self,
        mut payload: Value,
        timeout_dur: Duration,
    ) -> Result<SupervisorFrame, String> {
        if !self.is_alive() {
            return Err(format!(
                "supervisor connection closed (run={})",
                self.run_id
            ));
        }
        if let Some(remote) = &self.remote {
            return remote.control(payload).await;
        }
        if let Some(socket) = &self.runner_socket {
            payload["generation"] = json!(self.runner_generation);
            let value = crate::runner::exchange(
                socket,
                &format!("/v1/containers/{}/control", self.run_id),
                Some(payload),
            )
            .await?;
            return serde_json::from_value(value).map_err(|e| e.to_string());
        }
        let req_id = self.next_req_id.fetch_add(1, Ordering::SeqCst);
        payload["req_id"] = json!(req_id);
        let (tx, rx) = oneshot::channel();
        if let Ok(mut pending) = self.pending_reqs.lock() {
            pending.insert(req_id, tx);
        }
        let mut raw = serde_json::to_vec(&payload).map_err(|e| e.to_string())?;
        raw.push(b'\n');
        {
            let mut w = self.writer.lock().await;
            let w = w.as_mut().ok_or("Supervisor writer unavailable")?;
            if let Err(e) = w.write_all(&raw).await {
                self.closed.store(true, Ordering::SeqCst);
                if let Ok(mut pending) = self.pending_reqs.lock() {
                    pending.remove(&req_id);
                }
                return Err(format!("supervisor write failed: {e}"));
            }
            let _ = w.flush().await;
        }
        match tokio::time::timeout(timeout_dur, rx).await {
            Ok(Ok(frame)) => Ok(frame),
            Ok(Err(_)) => Err("supervisor response channel dropped".to_owned()),
            Err(_) => {
                if let Ok(mut pending) = self.pending_reqs.lock() {
                    pending.remove(&req_id);
                }
                Err(format!(
                    "supervisor request timed out after {:.1}s",
                    timeout_dur.as_secs_f64()
                ))
            }
        }
    }

    pub async fn health(&self) -> Result<SupervisorFrame, String> {
        self.call(json!({"op": "Health"}), Duration::from_secs(5))
            .await
    }

    pub async fn renew_lease(
        &self,
        millis: u64,
        expires_at: i64,
    ) -> Result<SupervisorFrame, String> {
        self.call(
            json!({"op":"RenewLease","lease_millis":millis,"lease_expires_at":expires_at}),
            Duration::from_secs(2),
        )
        .await
    }

    pub async fn register_run(
        &self,
        owner_run_id: &str,
        owner_token: &str,
        owner_workspace: &str,
    ) -> Result<SupervisorFrame, String> {
        self.call(
            json!({
                "op": "RegisterRun",
                "owner_run_id": owner_run_id,
                "owner_token": owner_token,
                "owner_workspace": owner_workspace,
            }),
            Duration::from_secs(10),
        )
        .await
    }

    pub async fn start_worker(
        &self,
        spec: &WorkerSpec,
    ) -> Result<(String, super::supervisor_stream::Stream), String> {
        if let Some(remote) = &self.remote {
            return remote.start(spec).await;
        }
        if let Some(socket) = &self.runner_socket {
            return self.start_via_runner(socket, spec).await;
        }
        let resp = self
            .call(
                json!({
                    "op": "StartWorker",
                    "spec": spec,
                }),
                Duration::from_secs(30),
            )
            .await?;
        if !resp.error.is_empty() {
            return Err(format!("StartWorker failed: {}", resp.error));
        }
        let wid = resp.worker_id.clone();
        if wid.is_empty() {
            return Err("StartWorker returned empty worker_id".to_owned());
        }
        if let (Some(owner), Some(token)) = (&spec.owner_run_id, &spec.owner_token) {
            self.worker_owners
                .lock()
                .unwrap()
                .insert(wid.clone(), (owner.clone(), token.clone()));
        }
        let (tx, rx) = super::supervisor_stream::channel();
        let mut saw_exit = false;
        let mut streams = self
            .worker_streams
            .lock()
            .map_err(|_| "Worker stream registry unavailable")?;
        if let Some(frames) = self
            .early_frames
            .lock()
            .map_err(|_| "Early Worker stream registry unavailable")?
            .remove(&wid)
        {
            for f in frames {
                if matches!(f.t.as_str(), "exit" | "transport_error") {
                    saw_exit = true;
                }
                let _ = tx.send(f);
            }
        }
        if !saw_exit {
            streams.insert(wid.clone(), tx);
        }
        Ok((wid, rx))
    }

    pub async fn signal(
        &self,
        worker_id: &str,
        sig: &str,
        owner_run_id: Option<&str>,
        owner_token: Option<&str>,
    ) -> Result<SupervisorFrame, String> {
        let saved = self.worker_owners.lock().unwrap().get(worker_id).cloned();
        let owner_run_id = owner_run_id.or_else(|| saved.as_ref().map(|v| v.0.as_str()));
        let owner_token = owner_token.or_else(|| saved.as_ref().map(|v| v.1.as_str()));
        let mut req = json!({
            "op": "Signal",
            "worker_id": worker_id,
            "signal": sig,
        });
        if let Some(r) = owner_run_id {
            req["owner_run_id"] = json!(r);
        }
        if let Some(t) = owner_token {
            req["owner_token"] = json!(t);
        }
        self.call(req, Duration::from_secs(10)).await
    }

    pub async fn write_stdin(&self, worker_id: &str, data: &str) -> Result<bool, String> {
        let response = self
            .call(
                json!({"op":"WriteStdin","worker_id":worker_id,
            "data":data}),
                Duration::from_secs(10),
            )
            .await?;
        if !response.error.is_empty() {
            return Err(response.error);
        }
        Ok(response.ok)
    }

    pub async fn teardown(
        &self,
        owner_run_id: Option<&str>,
        owner_token: Option<&str>,
        owner_workspace: Option<&str>,
    ) -> Result<SupervisorFrame, String> {
        let mut req = json!({"op": "TeardownRun"});
        if let Some(r) = owner_run_id {
            req["owner_run_id"] = json!(r);
        }
        if let Some(t) = owner_token {
            req["owner_token"] = json!(t);
        }
        if let Some(w) = owner_workspace {
            req["owner_workspace"] = json!(w);
        }
        self.call(req, Duration::from_secs(15)).await
    }
}

struct ReceiverSlot {
    token: Option<String>,
    link: Option<Arc<SupervisorLink>>,
    waiters: Vec<oneshot::Sender<Arc<SupervisorLink>>>,
}

pub struct ControlReceiver {
    port: u16,
    bind_ip: std::net::IpAddr,
    automatic_bridge: bool,
    bridge_bind: AsyncMutex<Option<std::net::Ipv4Addr>>,
    slots: Arc<Mutex<HashMap<String, ReceiverSlot>>>,
}

impl ControlReceiver {
    pub async fn bind() -> Result<Arc<Self>, String> {
        let port_pref = std::env::var("MUTEKI_CONTROL_PORT")
            .ok()
            .and_then(|s| s.trim().parse::<u16>().ok())
            .unwrap_or(0);
        let explicit_bind = std::env::var("MUTEKI_CONTROL_BIND")
            .ok()
            .filter(|s| !s.trim().is_empty());
        let automatic_bridge = explicit_bind.is_none() && cfg!(target_os = "linux");
        let bind_host = explicit_bind.unwrap_or_else(|| "127.0.0.1".to_owned());
        let listener = match TcpListener::bind((bind_host.as_str(), port_pref)).await {
            Ok(l) => l,
            Err(_) if port_pref != 0 => TcpListener::bind((bind_host.as_str(), 0))
                .await
                .map_err(|e| format!("Failed to bind control receiver: {e}"))?,
            Err(e) => return Err(format!("Failed to bind control receiver: {e}")),
        };
        let port = listener
            .local_addr()
            .map_err(|e| format!("Failed to read receiver local_addr: {e}"))?
            .port();
        let bind_ip = listener
            .local_addr()
            .map_err(|error| error.to_string())?
            .ip();
        let slots: Arc<Mutex<HashMap<String, ReceiverSlot>>> = Arc::new(Mutex::new(HashMap::new()));
        Self::accept(listener, Arc::clone(&slots));
        Ok(Arc::new(Self {
            port,
            bind_ip,
            automatic_bridge,
            bridge_bind: AsyncMutex::new(None),
            slots,
        }))
    }

    fn accept(listener: TcpListener, slots_clone: Arc<Mutex<HashMap<String, ReceiverSlot>>>) {
        tokio::spawn(async move {
            loop {
                let Ok((stream, _peer)) = listener.accept().await else {
                    break;
                };
                let slots_conn = Arc::clone(&slots_clone);
                tokio::spawn(async move {
                    Self::handle_incoming(stream, slots_conn).await;
                });
            }
        });
    }

    async fn handle_incoming(stream: TcpStream, slots: Arc<Mutex<HashMap<String, ReceiverSlot>>>) {
        let (read_half, mut write_half) = stream.into_split();
        let mut reader = BufReader::new(read_half);
        let mut hello_line = String::new();
        let read_res =
            tokio::time::timeout(Duration::from_secs(10), reader.read_line(&mut hello_line)).await;
        if !matches!(read_res, Ok(Ok(n)) if n > 0) {
            return;
        }
        let Ok(hello) = serde_json::from_str::<Value>(hello_line.trim()) else {
            let _ = write_half
                .write_all(b"{\"ok\":false,\"error\":\"bad hello json\"}\n")
                .await;
            return;
        };
        if hello["hello"].as_i64() != Some(1) {
            let _ = write_half
                .write_all(b"{\"ok\":false,\"error\":\"unsupported hello version\"}\n")
                .await;
            return;
        }
        let run_id = hello["run_id"].as_str().unwrap_or("").trim().to_owned();
        let token = hello["token"].as_str().unwrap_or("").trim().to_owned();
        let version = hello["version"].as_str().unwrap_or("").trim().to_owned();

        let auth_err = {
            let mut guard = slots.lock().unwrap_or_else(|e| e.into_inner());
            match guard.get_mut(&run_id) {
                None => Some("unknown run_id"),
                Some(slot) => match &slot.token {
                    None => Some("token already consumed or superseded"),
                    Some(expected) if expected != &token || token.is_empty() => {
                        Some("token mismatch")
                    }
                    Some(_) => {
                        slot.token = None;
                        None
                    }
                },
            }
        };
        if let Some(err_msg) = auth_err {
            let reply = format!("{{\"ok\":false,\"error\":{err_msg:?}}}\n");
            let _ = write_half.write_all(reply.as_bytes()).await;
            return;
        }
        if write_half.write_all(b"{\"ok\":true}\n").await.is_err() {
            return;
        }
        let _ = write_half.flush().await;

        let link = SupervisorLink::new_from_halves(run_id.clone(), version, reader, write_half);
        let mut guard = slots.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(slot) = guard.get_mut(&run_id) {
            slot.link = Some(Arc::clone(&link));
            for tx in slot.waiters.drain(..) {
                let _ = tx.send(Arc::clone(&link));
            }
        }
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    pub async fn connect_addr(&self, network_mode: &str) -> Result<String, String> {
        let explicit = std::env::var("MUTEKI_CONTROL_CONNECT_HOST")
            .ok()
            .filter(|host| !host.trim().is_empty())
            .map(|host| format!("{}:{}", host.trim(), self.port));
        if network_mode == "host" && !cfg!(target_os = "macos") {
            let host = if self.bind_ip.is_unspecified() {
                std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)
            } else {
                self.bind_ip
            };
            return Ok(
                explicit.unwrap_or_else(|| std::net::SocketAddr::new(host, self.port).to_string())
            );
        }
        if self.automatic_bridge {
            let mut bridge = self.bridge_bind.lock().await;
            if bridge.is_none() {
                let output = tokio::time::timeout(
                    Duration::from_secs(10),
                    crate::host_process::command("docker")
                        .args([
                            "network",
                            "inspect",
                            "bridge",
                            "--format",
                            "{{range .IPAM.Config}}{{println .Gateway}}{{end}}",
                        ])
                        .kill_on_drop(true)
                        .output(),
                )
                .await
                .map_err(|_| {
                    "Docker bridge inspection timed out while selecting the control interface"
                        .to_owned()
                })?
                .map_err(|error| {
                    format!("Cannot inspect Docker bridge for control binding: {error}")
                })?;
                if !output.status.success() {
                    return Err(format!(
                        "Cannot inspect Docker bridge for control binding: {}",
                        String::from_utf8_lossy(&output.stderr)
                    ));
                }
                let gateway = String::from_utf8(output.stdout).map_err(|error| error.to_string())?.lines()
                    .find_map(|line| line.trim().parse::<std::net::Ipv4Addr>().ok().filter(|ip| !ip.is_unspecified()))
                    .ok_or("Docker bridge has no IPv4 gateway; configure MUTEKI_CONTROL_BIND and MUTEKI_CONTROL_CONNECT_HOST explicitly")?;
                let listener = TcpListener::bind((gateway, self.port)).await
                    .map_err(|error| format!("Cannot bind Docker bridge control interface {gateway}: {error}; configure MUTEKI_CONTROL_BIND and MUTEKI_CONTROL_CONNECT_HOST for this Docker deployment"))?;
                Self::accept(listener, Arc::clone(&self.slots));
                *bridge = Some(gateway);
            }
            return Ok(explicit.unwrap_or_else(|| {
                std::net::SocketAddr::new((*bridge).unwrap().into(), self.port).to_string()
            }));
        }
        Ok(explicit.unwrap_or_else(|| format!("host.docker.internal:{}", self.port)))
    }

    pub fn expect(&self, run_id: &str) -> String {
        let token = format!(
            "{}{}",
            uuid::Uuid::new_v4().simple(),
            uuid::Uuid::new_v4().simple()
        );
        let mut guard = self.slots.lock().unwrap_or_else(|e| e.into_inner());
        guard.insert(
            run_id.to_owned(),
            ReceiverSlot {
                token: Some(token.clone()),
                link: None,
                waiters: Vec::new(),
            },
        );
        token
    }

    pub fn get(&self, run_id: &str) -> Option<Arc<SupervisorLink>> {
        let guard = self.slots.lock().unwrap_or_else(|e| e.into_inner());
        guard
            .get(run_id)
            .and_then(|s| s.link.clone())
            .filter(|l| l.is_alive())
    }

    pub async fn wait_for(
        &self,
        run_id: &str,
        timeout: Duration,
    ) -> Result<Arc<SupervisorLink>, String> {
        let rx = {
            let mut guard = self.slots.lock().unwrap_or_else(|e| e.into_inner());
            let slot = guard
                .get_mut(run_id)
                .ok_or_else(|| format!("receiver slot not registered for run {run_id}"))?;
            if let Some(link) = &slot.link
                && link.is_alive()
            {
                return Ok(Arc::clone(link));
            }
            let (tx, rx) = oneshot::channel();
            slot.waiters.push(tx);
            rx
        };
        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(link)) => Ok(link),
            Ok(Err(_)) => Err(format!(
                "receiver slot dropped while waiting for run {run_id}"
            )),
            Err(_) => Err(format!(
                "timed out ({:.0}s) waiting for container supervisor to connect back (run={run_id})",
                timeout.as_secs_f64()
            )),
        }
    }

    pub fn forget(&self, run_id: &str) {
        let mut guard = self.slots.lock().unwrap_or_else(|e| e.into_inner());
        guard.remove(run_id);
    }
}

impl SupervisorLink {
    fn new_from_halves(
        run_id: String,
        peer_version: String,
        mut reader: BufReader<tokio::net::tcp::OwnedReadHalf>,
        writer_half: tokio::net::tcp::OwnedWriteHalf,
    ) -> Arc<Self> {
        let link = Arc::new(Self {
            run_id,
            peer_version,
            writer: AsyncMutex::new(Some(writer_half)),
            runner_socket: None,
            remote: None,
            runner_generation: 1,
            next_req_id: AtomicI64::new(1),
            pending_reqs: Mutex::new(HashMap::new()),
            worker_streams: Mutex::new(HashMap::new()),
            early_frames: Mutex::new(HashMap::new()),
            worker_owners: Mutex::new(HashMap::new()),
            closed: Arc::new(AtomicBool::new(false)),
        });
        let link_clone = Arc::clone(&link);
        tokio::spawn(async move {
            let mut line = String::new();
            loop {
                line.clear();
                match (&mut reader)
                    .take((super::supervisor_stream::MAX_FRAME_BYTES + 1) as u64)
                    .read_line(&mut line)
                    .await
                {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {
                        if line.len() > super::supervisor_stream::MAX_FRAME_BYTES
                            || !line.ends_with('\n')
                        {
                            tracing::error!(
                                "Supervisor frame exceeded the bounded wire size or was truncated"
                            );
                            break;
                        }
                        let trimmed = line.trim();
                        if trimmed.is_empty() {
                            continue;
                        }
                        let Ok(frame) = serde_json::from_str::<SupervisorFrame>(trimmed) else {
                            continue;
                        };
                        if !link_clone.route_frame(frame) {
                            break;
                        }
                    }
                }
            }
            link_clone.closed.store(true, Ordering::SeqCst);
            if let Ok(mut pending) = link_clone.pending_reqs.lock() {
                for (_, tx) in pending.drain() {
                    let _ = tx.send(SupervisorFrame {
                        t: "resp".to_owned(),
                        error: format!("supervisor connection closed (run={})", link_clone.run_id),
                        ..Default::default()
                    });
                }
            }
            if let Ok(mut streams) = link_clone.worker_streams.lock() {
                for (wid, tx) in streams.drain() {
                    let _ = tx.send(SupervisorFrame {
                        t: "transport_error".to_owned(),
                        worker_id: wid,
                        error: "supervisor connection closed; Worker outcome is unknown".to_owned(),
                        ..Default::default()
                    });
                }
            }
        });
        link
    }
}

#[derive(Clone, Serialize)]
pub struct ContainerHandle {
    pub name: String,
    pub container_id: String,
    pub run_id: String,
    pub image: String,
    pub host_workspace: PathBuf,
    pub container_workspace: String,
    pub host_control_dir: PathBuf,
    pub host_accounts_dir: Option<PathBuf>,
    #[serde(skip)]
    pub link: Arc<SupervisorLink>,
    pub scope: String,
    pub owner_token: String,
    pub policy_identity: String,
    pub network_mode: String,
    #[serde(default)]
    pub has_gateway_proxy: bool,
    pub limits: WorkerResourceLimits,
    pub worker_uid: u32,
    pub worker_gid: u32,
    pub workspace_uid: u32,
    pub workspace_gid: u32,
}

impl ContainerHandle {
    pub(crate) fn from_remote(
        mut value: Value,
        remote: Arc<crate::runner_remote::controller::Remote>,
    ) -> Result<Self, String> {
        value["generation"] = json!(remote.scope.generation);
        let mut handle = Self::from_runner(value, PathBuf::new())?;
        handle.host_workspace = remote.workspace.clone();
        handle.host_control_dir = remote
            .root
            .join("runner/remote-projections")
            .join(&remote.scope.run_id)
            .join("control");
        handle.host_accounts_dir = Some(
            remote
                .root
                .join("runner/remote-projections")
                .join(&remote.scope.run_id)
                .join("accounts"),
        );
        handle.link = SupervisorLink::remote(remote);
        Ok(handle)
    }
    fn from_runner(value: Value, socket: PathBuf) -> Result<Self, String> {
        fn field<T: serde::de::DeserializeOwned>(value: &Value, key: &str) -> Result<T, String> {
            serde_json::from_value(value[key].clone())
                .map_err(|e| format!("Invalid Runner handle {key}: {e}"))
        }
        let run_id: String = field(&value, "run_id")?;
        Ok(Self {
            link: SupervisorLink::runner(run_id.clone(), socket, field(&value, "generation")?),
            run_id,
            name: field(&value, "name")?,
            container_id: field(&value, "container_id")?,
            image: field(&value, "image")?,
            host_workspace: field(&value, "host_workspace")?,
            container_workspace: field(&value, "container_workspace")?,
            host_control_dir: field(&value, "host_control_dir")?,
            host_accounts_dir: field(&value, "host_accounts_dir")?,
            scope: field(&value, "scope")?,
            owner_token: field(&value, "owner_token")?,
            policy_identity: field(&value, "policy_identity")?,
            network_mode: field(&value, "network_mode")?,
            has_gateway_proxy: value
                .get("has_gateway_proxy")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            limits: field(&value, "limits")?,
            worker_uid: field(&value, "worker_uid")?,
            worker_gid: field(&value, "worker_gid")?,
            workspace_uid: field(&value, "workspace_uid")?,
            workspace_gid: field(&value, "workspace_gid")?,
        })
    }
    pub fn to_container_path(&self, host_path: &Path) -> Result<String, String> {
        let host_canon = std::fs::canonicalize(&self.host_workspace)
            .unwrap_or_else(|_| self.host_workspace.clone());
        let target_canon =
            std::fs::canonicalize(host_path).unwrap_or_else(|_| host_path.to_path_buf());
        let rel = target_canon
            .strip_prefix(&host_canon)
            .or_else(|_| host_path.strip_prefix(&self.host_workspace))
            .map_err(|_| {
                format!(
                    "{} is not under host_workspace {}",
                    host_path.display(),
                    self.host_workspace.display()
                )
            })?;
        let rel_str = rel.to_string_lossy().replace('\\', "/");
        if rel_str.is_empty() || rel_str == "." {
            Ok(self.container_workspace.clone())
        } else {
            Ok(format!(
                "{}/{}",
                self.container_workspace.trim_end_matches('/'),
                rel_str.trim_start_matches('/')
            ))
        }
    }
}

struct ContainerRegistry {
    receiver: AsyncMutex<Option<Arc<ControlReceiver>>>,
    handles: AsyncMutex<HashMap<String, ContainerHandle>>,
    owners: AsyncMutex<HashMap<String, Vec<String>>>,
    lifecycle_locks: Mutex<HashMap<String, std::sync::Weak<AsyncMutex<()>>>>,
}

static REGISTRY: OnceLock<ContainerRegistry> = OnceLock::new();

fn registry() -> &'static ContainerRegistry {
    REGISTRY.get_or_init(|| ContainerRegistry {
        receiver: AsyncMutex::new(None),
        handles: AsyncMutex::new(HashMap::new()),
        owners: AsyncMutex::new(HashMap::new()),
        lifecycle_locks: Mutex::new(HashMap::new()),
    })
}

fn container_lifecycle_lock(run_id: &str) -> Arc<AsyncMutex<()>> {
    let mut locks = registry().lifecycle_locks.lock().unwrap();
    locks.retain(|_, lock| lock.strong_count() > 0);
    if let Some(lock) = locks.get(run_id).and_then(std::sync::Weak::upgrade) {
        return lock;
    }
    let lock = Arc::new(AsyncMutex::new(()));
    locks.insert(run_id.to_owned(), Arc::downgrade(&lock));
    lock
}

fn worker_id_override(name: &str) -> Option<u32> {
    std::env::var(name).ok()?.trim().parse().ok()
}

fn workspace_owner_ids() -> (u32, u32) {
    #[cfg(unix)]
    let owner = (unsafe { libc::geteuid() }, unsafe { libc::getegid() });
    #[cfg(not(unix))]
    let owner = (0, 0);
    (
        worker_id_override("MUTEKI_WORKSPACE_UID").unwrap_or(owner.0),
        worker_id_override("MUTEKI_WORKSPACE_GID").unwrap_or(owner.1),
    )
}

/// `path` must lie at or beneath `base`; Worker-writable links under `base` are never followed.
#[cfg(unix)]
pub(crate) fn chown_tree_to_worker(base: &Path, path: &Path, uid: u32, gid: u32) {
    if unsafe { libc::geteuid() } != 0 {
        return;
    }
    if let Err(error) = crate::private_files::chown_tree(base, path, uid, gid)
        && error.kind() != std::io::ErrorKind::NotFound
    {
        tracing::warn!(%error, path = %path.display(), "Worker ownership handoff incomplete");
    }
}

#[cfg(unix)]
pub(crate) fn chown_ancestors_to_worker(workspace: &Path, home: &Path, uid: u32, gid: u32) {
    if unsafe { libc::geteuid() } != 0 {
        return;
    }
    if let Err(error) = crate::private_files::chown_ancestors(workspace, home, uid, gid) {
        tracing::warn!(%error, path = %home.display(), "Worker home traversal handoff incomplete");
    }
}

#[cfg(not(unix))]
pub(crate) fn chown_ancestors_to_worker(_workspace: &Path, _home: &Path, _uid: u32, _gid: u32) {}

#[cfg(not(unix))]
pub(crate) fn chown_tree_to_worker(_base: &Path, _path: &Path, _uid: u32, _gid: u32) {}

async fn get_receiver() -> Result<Arc<ControlReceiver>, String> {
    let mut guard = registry().receiver.lock().await;
    if let Some(r) = &*guard {
        return Ok(Arc::clone(r));
    }
    let r = ControlReceiver::bind().await?;
    *guard = Some(Arc::clone(&r));
    Ok(r)
}

fn sanitize_container_suffix(run_id: &str) -> String {
    let clean: String = run_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-') {
                c
            } else {
                '-'
            }
        })
        .collect();
    let trimmed = clean.trim_matches(|c: char| !c.is_ascii_alphanumeric());
    if trimmed.is_empty() {
        "default".to_owned()
    } else {
        trimmed.chars().take(40).collect()
    }
}

fn write_private_file(root: &Path, path: &Path, content: &[u8]) -> Result<(), String> {
    crate::private_files::write(root, path, content)
}

fn copy_dir_recursive(root: &Path, src: &Path, dst: &Path) -> Result<(), String> {
    if !src.is_dir() {
        return Ok(());
    }
    crate::private_files::ensure_dir(root, dst)?;
    for entry in std::fs::read_dir(src).map_err(|e| e.to_string())? {
        let entry = entry.map_err(|e| e.to_string())?;
        let ty = entry.file_type().map_err(|e| e.to_string())?;
        let s = entry.path();
        let d = dst.join(entry.file_name());
        if ty.is_dir() {
            copy_dir_recursive(root, &s, &d)?;
        } else if ty.is_file() {
            let bytes = std::fs::read(&s).map_err(|e| e.to_string())?;
            write_private_file(root, &d, &bytes)?;
        }
    }
    Ok(())
}

fn rewrite_provider_urls(value: &mut Value, rewrite: &impl Fn(&str) -> String) -> bool {
    match value {
        Value::Object(fields) => {
            let mut changed = false;
            for (key, value) in fields {
                if matches!(
                    key.as_str(),
                    "base_url"
                        | "baseUrl"
                        | "baseURL"
                        | "ANTHROPIC_BASE_URL"
                        | "OPENAI_BASE_URL"
                        | "GROK_MODELS_BASE_URL"
                        | "KIMI_MODEL_BASE_URL"
                        | "CURSOR_ENDPOINT"
                ) {
                    if let Some(url) = value.as_str() {
                        let mapped = rewrite(url);
                        if mapped != url {
                            *value = json!(mapped);
                            changed = true;
                        }
                    }
                } else {
                    changed |= rewrite_provider_urls(value, rewrite);
                }
            }
            changed
        }
        Value::Array(values) => values.iter_mut().fold(false, |changed, value| {
            rewrite_provider_urls(value, rewrite) | changed
        }),
        _ => false,
    }
}

fn project_provider_urls(
    root: &Path,
    home: &Path,
    env: &mut HashMap<String, String>,
    rewrite: &impl Fn(&str) -> String,
) -> Result<(), RuntimeError> {
    let invalid = |error: String| RuntimeError::new("credential.provider_config_invalid", error);
    if let Some(raw) = env
        .get_mut("OPENCODE_CONFIG_CONTENT")
        .filter(|value| !value.is_empty())
    {
        let mut config: Value = serde_json::from_str(raw)
            .map_err(|_| invalid("invalid OPENCODE_CONFIG_CONTENT".into()))?;
        if rewrite_provider_urls(&mut config, rewrite) {
            *raw = config.to_string();
        }
    }
    for (relative, format) in [
        (".codex/config.toml", "toml"),
        (".kimi-code/config.toml", "toml"),
        (".grok/config.toml", "toml"),
        (".omp/agent/models.yml", "yaml"),
        (".pi/agent/models.json", "json"),
        (".claude/settings.json", "json"),
        (".config/opencode/opencode.json", "jsonc"),
        (".config/opencode/opencode.jsonc", "jsonc"),
        (".config/opencode/selected.jsonc", "jsonc"),
    ] {
        let path = home.join(relative);
        let raw = match std::fs::read_to_string(&path) {
            Ok(raw) => raw,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(invalid(format!("cannot read {relative}: {error}"))),
        };
        let mut value: Value = match format {
            "toml" => toml::from_str::<toml::Value>(&raw)
                .ok()
                .and_then(|value| serde_json::to_value(value).ok()),
            "yaml" => serde_yaml::from_str(&raw).ok(),
            "jsonc" => jsonc_parser::parse_to_serde_value(
                &raw,
                &jsonc_parser::ParseOptions {
                    allow_comments: true,
                    allow_trailing_commas: true,
                    allow_loose_object_property_names: false,
                    allow_missing_commas: false,
                    allow_single_quoted_strings: false,
                    allow_hexadecimal_numbers: false,
                    allow_unary_plus_numbers: false,
                    allow_bare_decimal_point_numbers: false,
                    allow_non_finite_numbers: false,
                    allow_extended_string_escapes: false,
                },
            )
            .ok(),
            _ => serde_json::from_str(&raw).ok(),
        }
        .ok_or_else(|| invalid(format!("invalid projected {relative}")))?;
        if rewrite_provider_urls(&mut value, rewrite) {
            let bytes = match format {
                "toml" => serde_json::from_value::<toml::Value>(value)
                    .ok()
                    .and_then(|value| toml::to_string(&value).ok()),
                "yaml" => serde_yaml::to_string(&value).ok(),
                _ => Some(value.to_string()),
            }
            .ok_or_else(|| invalid(format!("cannot serialize projected {relative}")))?;
            write_private_file(root, &path, bytes.as_bytes())
                .map_err(|error| RuntimeError::new("runtime.io_error", error))?;
        }
    }
    Ok(())
}

/// Detect macOS system HTTP proxy (`127.0.0.1:<port>`) if present so bridge containers
/// can reach external LLM APIs via `host.docker.internal:<port>`.
pub fn detect_host_proxy_for_container() -> Option<String> {
    for var_name in ["HTTPS_PROXY", "https_proxy", "HTTP_PROXY", "http_proxy"] {
        if let Ok(val) = std::env::var(var_name) {
            let trimmed = val.trim();
            if !trimmed.is_empty() {
                return Some(
                    trimmed
                        .replace("127.0.0.1", "host.docker.internal")
                        .replace("localhost", "host.docker.internal"),
                );
            }
        }
    }
    #[cfg(target_os = "macos")]
    {
        let out = crate::host_process::std_command("/usr/sbin/scutil")
            .arg("--proxy")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        let text = String::from_utf8_lossy(&out.stdout);
        let mut enabled = false;
        let mut host = String::new();
        let mut port = String::new();
        for line in text.lines() {
            let line = line.trim();
            if let Some(v) = line.strip_prefix("HTTPEnable :") {
                enabled = v.trim() == "1";
            } else if let Some(v) = line.strip_prefix("HTTPProxy :") {
                host = v.trim().to_owned();
            } else if let Some(v) = line.strip_prefix("HTTPPort :") {
                port = v.trim().to_owned();
            }
        }
        if enabled && !host.is_empty() && !port.is_empty() {
            let mapped_host = if host == "127.0.0.1" || host == "localhost" {
                "host.docker.internal"
            } else {
                host.as_str()
            };
            return Some(format!("http://{mapped_host}:{port}"));
        }
    }
    None
}

/// Docker reports a missing object with different wording and case across CLI versions
/// ("No such object" up to 27, "no such object" from 28), so absence is matched loosely.
pub(crate) fn docker_reports_absent(stderr: &[u8]) -> bool {
    let text = String::from_utf8_lossy(stderr).to_ascii_lowercase();
    ["no such object", "no such container", "no such image"]
        .iter()
        .any(|phrase| text.contains(phrase))
}

pub async fn ensure_run_container(
    state: &AppState,
    run_id: &str,
    host_workspace: &Path,
    settings: &Value,
) -> Result<ContainerHandle, String> {
    ensure_run_container_owned(state, run_id, host_workspace, settings, None).await
}

pub(super) fn runtime_policy(settings: &Value) -> String {
    let mut policy = serde_json::Map::new();
    for key in [
        "worker_network",
        "worker_privilege",
        "worker_memory",
        "worker_cpus",
        "worker_pids_limit",
        "worker_output_limit",
        "worker_disk_limit",
        "_runner_trust_domain",
        "_runner_lease_required",
    ] {
        policy.insert(key.into(), settings[key].clone());
    }
    policy.insert("worker_privilege".into(), json!("root"));
    if let Some(runtime) = settings["worker_oci_runtime"]
        .as_str()
        .filter(|v| !v.trim().is_empty())
    {
        policy.insert("worker_oci_runtime".into(), json!(runtime.trim()));
    }
    policy.insert(
        "image".into(),
        settings
            .get("_runner_image_id")
            .cloned()
            .unwrap_or_else(|| {
                json!(
                    std::env::var("MUTEKI_WORKER_IMAGE")
                        .unwrap_or_else(|_| DEFAULT_WORKER_IMAGE.into())
                )
            }),
    );
    serde_json::Value::Object(policy).to_string()
}
pub fn workspace_path(state: &AppState, run_id: &str) -> String {
    let shared = state
        .store
        .get("runs", run_id)
        .ok()
        .flatten()
        .is_some_and(|run| run["worker_runtime"]["worker_container_scope"] == "shared");
    if shared {
        format!(
            "{SHARED_CONTAINER_WORKSPACE}/{}",
            super::encode_run_id(run_id)
        )
    } else {
        CONTAINER_WORKSPACE.into()
    }
}
pub(crate) fn prepare_shared_workspace(state: &AppState, run_id: &str) -> Result<(), String> {
    if crate::runner::configured_socket().is_some() {
        return Ok(());
    }
    let settings = super::launch_config::settings(state, run_id).map_err(|e| e.to_string())?;
    if crate::runner_remote::controller::selected(&settings).is_some() {
        return Ok(());
    }
    super::shared_pools::prepare(state, run_id, &settings)
}
pub async fn ensure_run_container_owned(
    state: &AppState,
    run_id: &str,
    host_workspace: &Path,
    settings: &Value,
    owner_run_id: Option<&str>,
) -> Result<ContainerHandle, String> {
    if crate::runner_remote::controller::selected(settings).is_some() {
        let handle = crate::runner_remote::controller::ensure(
            state,
            run_id,
            host_workspace,
            settings,
            owner_run_id,
        )
        .await?;
        registry()
            .handles
            .lock()
            .await
            .insert(run_id.into(), handle.clone());
        return Ok(handle);
    }
    if let Some(socket) = crate::runner::configured_socket() {
        let root = native_chat::node_state_dir(state);
        let relative = host_workspace
            .strip_prefix(&root)
            .map_err(|_| "Workspace is outside node state root")?;
        let value=crate::runner::exchange(&socket,"/v1/containers/ensure",Some(json!({"run_id":run_id,
            "settings":settings,"workspace_relative":relative,"owner_run_id":owner_run_id,
            "generation":state.store.get("runs",run_id).map_err(|e|e.to_string())?.and_then(|run|run["execution_generation"].as_u64()).unwrap_or(1)}))).await?;
        let handle = ContainerHandle::from_runner(value, socket)?;
        registry()
            .handles
            .lock()
            .await
            .insert(run_id.into(), handle.clone());
        return Ok(handle);
    }
    if settings["worker_container_scope"] != "shared" || owner_run_id != Some(run_id) {
        return ensure_container_runtime(state, run_id, host_workspace, settings, owner_run_id)
            .await;
    }
    let lifecycle = container_lifecycle_lock(run_id);
    let _lifecycle_guard = lifecycle.lock().await;
    let identity = runtime_policy(settings);
    let pool = super::shared_pools::identity(state, settings)?;
    let root = super::shared_pools::root(state, &pool)?;
    let slot = root.join(super::encode_run_id(run_id));
    if host_workspace.canonicalize().map_err(|e| e.to_string())?
        != slot.canonicalize().map_err(|e| e.to_string())?
    {
        return Err("Shared Worker workspace must be enrolled in its dedicated pool slot".into());
    }
    let runtime = format!("{SHARED_CONTAINER_RUN_ID}-{pool}");
    let setup = container_lifecycle_lock(&format!("setup:{runtime}"));
    let _setup = setup.lock().await;
    {
        let handles = registry().handles.lock().await;
        if let Some(handle) = handles.get(run_id).filter(|h| h.link.is_alive()) {
            if handle.policy_identity != identity {
                return Err("Active Run pool policy cannot change".into());
            }
            return Ok(handle.clone());
        }
    }
    let mut runtime_settings = settings.clone();
    runtime_settings["worker_container_scope"] = json!("run");
    let base = ensure_container_runtime(state, &runtime, &root, &runtime_settings, None).await?;
    let registration: Result<(String, String), String> = async {
    let owner_key=format!("{runtime}:{run_id}");
    let token=match state.store.get("shared_container_owners",&owner_key).map_err(|e|e.to_string())?{
        Some(record)=>{if record["policy"]!=identity{return Err("Saved shared owner policy differs from requested policy".into());}record["token"].as_str().filter(|v|v.len()>=32).ok_or("Shared owner token is invalid")?.to_owned()},
        None=>{let token=uuid::Uuid::new_v4().simple().to_string();state.store.put("shared_container_owners",&owner_key,&json!({"run_id":run_id,"runtime_id":runtime,"policy":identity,"token":token})).map_err(|e|e.to_string())?;token}
    };
    let workspace=format!("{SHARED_CONTAINER_WORKSPACE}/{}",super::encode_run_id(run_id));
    let health=base.link.health().await?;if !health.scoped_teardown || !health.scoped_processes{return Err("Shared runtime requires Run subreaper support; rebuild or update the Worker image".into());}
    let registered=base.link.register_run(run_id,&token,&workspace).await?;
    if !registered.error.is_empty() || !registered.ok{return Err(format!("Shared Run owner registration failed: {}",registered.error));}
    Ok((token, workspace))
    }.await;
    let (token, workspace) = match registration {
        Ok(value) => value,
        Err(error) => {
            let occupied =
                registry().handles.lock().await.values().any(|handle| {
                    handle.scope == "shared" && handle.container_id == base.container_id
                });
            if !occupied {
                let physical = container_lifecycle_lock(&base.run_id);
                let _physical = physical.lock().await;
                if let Err(cleanup) = destroy_runtime(&base).await {
                    return Err(format!(
                        "{error}; empty shared runtime cleanup unconfirmed: {cleanup}"
                    ));
                }
            }
            return Err(error);
        }
    };
    let mut handle = base;
    handle.run_id = run_id.into();
    handle.host_workspace = slot;
    handle.container_workspace = workspace;
    handle.scope = "shared".into();
    handle.owner_token = token;
    handle.policy_identity = identity;
    registry()
        .handles
        .lock()
        .await
        .insert(run_id.into(), handle.clone());
    Ok(handle)
}

async fn ensure_container_runtime(
    state: &AppState,
    run_id: &str,
    host_workspace: &Path,
    settings: &Value,
    owner_run_id: Option<&str>,
) -> Result<ContainerHandle, String> {
    let lifecycle = container_lifecycle_lock(run_id);
    let _lifecycle_guard = lifecycle.lock().await;
    let policy_identity = runtime_policy(settings);
    let guard = registry().handles.lock().await;
    if let Some(existing) = guard
        .get(run_id)
        .filter(|handle| handle.link.is_alive() && handle.policy_identity != policy_identity)
    {
        return Err(format!("Worker container {} policy changed", existing.name));
    }
    if let Some(existing) = guard.get(run_id)
        && existing.link.is_alive()
    {
        return Ok(existing.clone());
    }

    if run_id.starts_with(SHARED_CONTAINER_RUN_ID)
        && guard.values().any(|handle| {
            handle.scope == "shared"
                && handle.name == format!("muteki-worker-{}", sanitize_container_suffix(run_id))
                && !handle.link.is_alive()
        })
    {
        return Err("Shared runtime link was lost; settle its registered owners before replacing the runtime".into());
    }
    drop(guard);
    let status = inspect_worker_image_status().await;
    if status["daemon"]["ok"].as_bool() != Some(true) {
        return Err(format!(
            "Docker daemon 未就绪: {}",
            status["daemon"]["detail"].as_str().unwrap_or("unknown")
        ));
    }
    if status["pulled"]["ok"].as_bool() != Some(true) {
        return Err(format!(
            "未找到可用的 Worker 容器镜像: {}",
            status["pulled"]["detail"].as_str().unwrap_or("missing")
        ));
    }
    let image = settings
        .get("_runner_image_id")
        .unwrap_or(&status["image_id"])
        .as_str()
        .filter(|id| id.starts_with("sha256:"))
        .ok_or("Worker image inspect did not return an immutable image ID")?
        .to_owned();

    let requested_net = settings["worker_network"].as_str().unwrap_or("bridge");
    let net_res = project_worker_network(requested_net);
    let Some(effective_net) = net_res.effective else {
        return Err(net_res
            .error
            .unwrap_or_else(|| "Worker 容器网络配置无效".to_owned()));
    };
    if effective_net == "none" {
        return Err(
            "worker_network=none cannot reach the RCP control receiver or remote model endpoints"
                .to_owned(),
        );
    }
    let gateway_mount = managed_gateway_mount(&net_res.requested).await?;
    let oci_runtime = settings["worker_oci_runtime"]
        .as_str()
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(str::to_owned);
    if let Some(runtime) = &oci_runtime {
        let info = crate::host_process::command("docker")
            .args(["info", "--format", "{{json .Runtimes}}"])
            .kill_on_drop(true)
            .output()
            .await
            .map_err(|e| e.to_string())?;
        let runtimes: Value = serde_json::from_slice(&info.stdout).unwrap_or(Value::Null);
        if !info.status.success() || runtimes.get(runtime.as_str()).is_none() {
            return Err(format!(
                "Docker 未注册 OCI runtime “{runtime}”（已注册：{}）；请先在该节点安装并配置它，或在设置中改回默认 runtime",
                runtimes
                    .as_object()
                    .map(|rows| rows.keys().cloned().collect::<Vec<_>>().join(", "))
                    .unwrap_or_default()
            ));
        }
    }
    let limits = resolve_worker_resource_limits(Some(settings))?;

    std::fs::create_dir_all(host_workspace).map_err(|e| e.to_string())?;
    let (workspace_uid, workspace_gid) = workspace_owner_ids();
    let ownership_root = host_workspace.to_path_buf();
    tokio::task::spawn_blocking(move || {
        chown_tree_to_worker(
            &ownership_root,
            &ownership_root,
            workspace_uid,
            workspace_gid,
        )
    })
    .await
    .map_err(|error| format!("Worker workspace ownership task failed: {error}"))?;
    let state_dir = native_chat::node_state_dir(state);
    let control_dir = state_dir
        .join("runtime")
        .join("control")
        .join(sanitize_container_suffix(run_id));
    crate::private_files::ensure_dir(&state_dir, &control_dir)?;
    let accounts_dir = state_dir
        .join("runtime")
        .join("account-projections")
        .join(sanitize_container_suffix(run_id));
    crate::private_files::ensure_dir(&state_dir, &accounts_dir)?;

    let receiver = get_receiver().await?;
    let connect_addr = receiver.connect_addr(&effective_net).await?;
    let token = receiver.expect(run_id);
    let token_file = control_dir.join("token");
    write_private_file(&state_dir, &token_file, format!("{token}\n").as_bytes())?;

    let container_name = format!("muteki-worker-{}", sanitize_container_suffix(run_id));
    let inspected = crate::host_process::command("docker")
        .args(["inspect", &container_name])
        .kill_on_drop(true)
        .output()
        .await
        .map_err(|e| e.to_string())?;
    if inspected.status.success() {
        let rows: Value = serde_json::from_slice(&inspected.stdout).map_err(|e| e.to_string())?;
        let labels = &rows[0]["Config"]["Labels"];
        let state_identity = state_dir
            .canonicalize()
            .map_err(|e| e.to_string())?
            .to_string_lossy()
            .into_owned();
        let owner_matches =
            labels["muteki.runtime_id"] == run_id || labels["muteki.owner_run_id"] == run_id;
        if labels["muteki.state_dir"] != state_identity || !owner_matches {
            return Err("Container name is occupied by an unverified runtime owner".into());
        }
        if rows[0]["State"]["Running"] == true || rows[0]["State"]["Restarting"] == true {
            return Err("Existing runtime is still running without an authenticated control link; reconcile its execution before replacement".into());
        }
        let old_id = rows[0]["Id"]
            .as_str()
            .filter(|id| id.len() == 64 && id.bytes().all(|c| c.is_ascii_hexdigit()))
            .ok_or("Existing runtime identity is invalid")?;
        let removed = crate::host_process::command("docker")
            .args(["rm", "-f", old_id])
            .kill_on_drop(true)
            .output()
            .await
            .map_err(|e| e.to_string())?;
        if !removed.status.success() {
            return Err(format!(
                "Stale Worker runtime removal failed: {}",
                String::from_utf8_lossy(&removed.stderr)
            ));
        }
    } else if !docker_reports_absent(&inspected.stderr) {
        return Err(format!(
            "Worker runtime absence could not be verified: {}",
            String::from_utf8_lossy(&inspected.stderr).trim()
        ));
    }

    let mut args: Vec<String> = vec![
        "run".to_owned(),
        "-d".to_owned(),
        "--name".to_owned(),
        container_name.clone(),
        "--network".to_owned(),
        effective_net.clone(),
        "--memory".to_owned(),
        limits.memory.clone(),
        "--cpus".to_owned(),
        limits.docker_cpus(),
        "--pids-limit".to_owned(),
        limits.pids_limit.to_string(),
        "-e".to_owned(),
        format!("MUTEKI_WORKER_OUTPUT_LIMIT={}", limits.output_limit_bytes),
        "-e".to_owned(),
        format!("MUTEKI_WORKER_DISK_LIMIT={}", limits.disk_limit_bytes),
        "-e".to_owned(),
        "MUTEKI_WORKER_UID=0".to_owned(),
        "-e".to_owned(),
        "MUTEKI_WORKER_GID=0".to_owned(),
        "-e".to_owned(),
        format!("MUTEKI_WORKSPACE_UID={workspace_uid}"),
        "-e".to_owned(),
        format!("MUTEKI_WORKSPACE_GID={workspace_gid}"),
        "--user".to_owned(),
        "0:0".to_owned(),
    ];
    if let Some(runtime) = &oci_runtime {
        args.extend(["--runtime".to_owned(), runtime.clone()]);
    }
    args.extend([
        "--label".into(),
        format!("muteki.runtime_id={run_id}"),
        "--label".into(),
        format!(
            "muteki.state_dir={}",
            state_dir
                .canonicalize()
                .map_err(|e| e.to_string())?
                .display()
        ),
    ]);
    if let Some(owner_run_id) = owner_run_id {
        args.extend([
            "--label".to_owned(),
            format!("muteki.owner_run_id={owner_run_id}"),
            "--label".to_owned(),
            format!("muteki.state_dir={}", state_dir.display()),
        ]);
    }

    if let Some(gateway) = &gateway_mount {
        args.extend([
            "--mount".to_owned(),
            format!(
                "type=bind,source={},target=/run/src/policy.json,readonly",
                gateway.root.join("worker-policy.json").display()
            ),
        ]);
        let ca = gateway.root.join("ca.pem");
        if ca.is_file() {
            args.extend([
                "--mount".to_owned(),
                format!(
                    "type=bind,source={},target=/run/src/ca.pem,readonly",
                    ca.display()
                ),
            ]);
            for key in [
                "SSL_CERT_FILE",
                "REQUESTS_CA_BUNDLE",
                "CURL_CA_BUNDLE",
                "NODE_EXTRA_CA_CERTS",
                "GIT_SSL_CAINFO",
            ] {
                args.extend(["-e".to_owned(), format!("{key}=/run/src/ca.pem")]);
            }
        }
        if let Some(proxy) = &gateway.proxy {
            for key in [
                "HTTP_PROXY",
                "HTTPS_PROXY",
                "ALL_PROXY",
                "http_proxy",
                "https_proxy",
                "all_proxy",
            ] {
                args.extend(["-e".to_owned(), format!("{key}={proxy}")]);
            }
            args.extend([
                "-e".to_owned(),
                "NO_PROXY=".to_owned(),
                "-e".to_owned(),
                "no_proxy=".to_owned(),
            ]);
        }
    }

    // Agents run as container root. Keep Docker's standard capabilities and
    // enable network administration and debugging for the installed toolchain.
    args.extend([
        "--cap-add".to_owned(),
        "NET_ADMIN".to_owned(),
        "--cap-add".to_owned(),
        "SYS_PTRACE".to_owned(),
    ]);
    if effective_net != "host"
        && effective_net != "none"
        && !effective_net.starts_with("container:")
    {
        args.extend([
            "--add-host".to_owned(),
            "host.docker.internal:host-gateway".to_owned(),
        ]);
    }

    if run_id.starts_with("btw-") {
        if let Some(owner) = owner_run_id {
            for (source, target) in [
                (super::session_dir(state, owner), "/run-context/workspace"),
                (super::graph::graph_dir(state, owner), "/run-context/graph"),
            ] {
                if source.is_dir() {
                    args.extend([
                        "--mount".into(),
                        format!(
                            "type=bind,source={},target={target},readonly",
                            source.canonicalize().map_err(|e| e.to_string())?.display()
                        ),
                    ]);
                }
            }
        }
    }
    args.extend([
        "-v".to_owned(),
        format!(
            "{}:{}",
            host_workspace.display(),
            if run_id.starts_with(SHARED_CONTAINER_RUN_ID) {
                SHARED_CONTAINER_WORKSPACE
            } else {
                CONTAINER_WORKSPACE
            }
        ),
        "-v".to_owned(),
        format!("{}:{CONTAINER_CONTROL_DIR}", control_dir.display()),
        "-v".to_owned(),
        format!("{}:{CONTAINER_ACCOUNTS_ROOT}:ro", accounts_dir.display()),
    ]);

    args.extend([
        "-w".to_owned(),
        CONTAINER_WORKSPACE.to_owned(),
        image.clone(),
        "--connect".to_owned(),
        connect_addr,
        "--run-id".to_owned(),
        run_id.to_owned(),
        "--token".to_owned(),
        format!("{CONTAINER_CONTROL_DIR}/token"),
    ]);

    if settings["_runner_lease_required"] == true {
        args.push("--lease-required".into());
    }
    if run_id.starts_with(SHARED_CONTAINER_RUN_ID) {
        args.extend([
            "--shared-pool".into(),
            "--workspace".into(),
            SHARED_CONTAINER_WORKSPACE.into(),
        ]);
    }
    let out = crate::host_process::command("docker")
        .args(&args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()
        .await
        .map_err(|e| format!("Failed to spawn docker run: {e}"))?;
    if !out.status.success() {
        receiver.forget(run_id);
        let _ = std::fs::remove_file(&token_file);
        return Err(format!(
            "docker run failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }

    let container_id = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    if container_id.len() != 64 || !container_id.bytes().all(|c| c.is_ascii_hexdigit()) {
        receiver.forget(run_id);
        return Err("Docker did not return an immutable container identity; reconcile creation before retrying".into());
    }
    let link = match receiver.wait_for(run_id, Duration::from_secs(30)).await {
        Ok(l) => l,
        Err(e) => {
            receiver.forget(run_id);
            let _ = std::fs::remove_file(&token_file);
            let logs = crate::host_process::command("docker")
                .args(["logs", "--tail", "30", &container_id])
                .output()
                .await
                .ok()
                .map(|o| {
                    format!(
                        "{}{}",
                        String::from_utf8_lossy(&o.stdout),
                        String::from_utf8_lossy(&o.stderr)
                    )
                })
                .unwrap_or_default();
            let _ = crate::host_process::command("docker")
                .args(["rm", "-f", &container_id])
                .output()
                .await;
            return Err(format!("{e} (container logs: {})", logs.trim()));
        }
    };
    let _ = std::fs::remove_file(&token_file);

    let handle = ContainerHandle {
        name: container_name,
        container_id,
        run_id: run_id.to_owned(),
        image,
        host_workspace: host_workspace.to_path_buf(),
        container_workspace: CONTAINER_WORKSPACE.to_owned(),
        host_control_dir: control_dir,
        host_accounts_dir: Some(accounts_dir),
        link,
        scope: "run".to_owned(),
        owner_token: String::new(),
        policy_identity,
        network_mode: effective_net,
        has_gateway_proxy: gateway_mount.as_ref().is_some_and(|g| g.proxy.is_some()),
        limits,
        worker_uid: 0,
        worker_gid: 0,
        workspace_uid,
        workspace_gid,
    };
    registry()
        .handles
        .lock()
        .await
        .insert(run_id.to_owned(), handle.clone());
    Ok(handle)
}

pub async fn register_run_container_owner(owner_run_id: &str, container_id: &str) {
    let mut owners = registry().owners.lock().await;
    let owned = owners.entry(owner_run_id.to_owned()).or_default();
    if !owned.iter().any(|id| id == container_id) {
        owned.push(container_id.to_owned());
    }
}

/// Called only after Docker confirms this immutable runtime has no execution.
/// Do not remove path-based transient directories here: a replacement runtime
/// may already use the same logical pool path with a different container ID.
pub(crate) async fn forget_stopped_runtime(identity: &str) {
    let removed: std::collections::HashSet<String> = {
        let mut handles = registry().handles.lock().await;
        let removed = handles
            .iter()
            .filter(|(_, handle)| handle.container_id == identity)
            .map(|(id, _)| id.clone())
            .collect();
        handles.retain(|_, handle| handle.container_id != identity);
        removed
    };
    let mut owners = registry().owners.lock().await;
    for ids in owners.values_mut() {
        ids.retain(|id| !removed.contains(id));
    }
    owners.retain(|_, ids| !ids.is_empty());
}

pub async fn teardown_owned_run_containers(owner_run_id: &str) -> Result<(), String> {
    let owned = registry()
        .owners
        .lock()
        .await
        .get(owner_run_id)
        .cloned()
        .unwrap_or_default();
    for container_id in owned {
        teardown_run_container_checked(&container_id).await?;
    }
    Ok(())
}

pub async fn teardown_run_container_recorded(state: &AppState, run_id: &str) -> Result<(), String> {
    teardown_recorded(state, run_id, false).await
}

pub(super) async fn teardown_recorded_before_delete(
    state: &AppState,
    run_id: &str,
) -> Result<(), String> {
    teardown_recorded(state, run_id, true).await
}

async fn teardown_recorded(
    state: &AppState,
    run_id: &str,
    allow_unrecorded: bool,
) -> Result<(), String> {
    if crate::runner_remote::controller::stop_recorded(state, run_id).await? {
        registry().handles.lock().await.remove(run_id);
        return Ok(());
    }
    if let Some(socket) = crate::runner::configured_socket() {
        let Some(current) = crate::runner::run_status_if_recorded(&socket, run_id).await? else {
            return if allow_unrecorded || !super::run_uses_container(state, run_id)? {
                Ok(())
            } else {
                Err("Unknown Runner Run".into())
            };
        };
        let generation = current["run"]["generation"]
            .as_u64()
            .ok_or("Run execution generation is unconfirmed")?;
        if state
            .store
            .get("runs", run_id)
            .map_err(|e| e.to_string())?
            .is_some_and(|run| run["execution_generation"].as_u64() != Some(generation))
        {
            return Err("Run execution generation changed before stop".into());
        }
        crate::runner::exchange(
            &socket,
            &format!("/v1/containers/{run_id}/teardown"),
            Some(json!({"generation":generation})),
        )
        .await?;
        registry().handles.lock().await.remove(run_id);
        return Ok(());
    }
    teardown_run_container_checked(run_id).await
}
pub async fn teardown_run_container_checked(run_id: &str) -> Result<(), String> {
    let remote = registry()
        .handles
        .lock()
        .await
        .get(run_id)
        .and_then(|handle| handle.link.remote.clone());
    if let Some(remote) = remote {
        remote.stop().await?;
        registry().handles.lock().await.remove(run_id);
        return Ok(());
    }

    if let Some(socket) = crate::runner::configured_socket() {
        let generation = registry()
            .handles
            .lock()
            .await
            .get(run_id)
            .map(|h| h.link.runner_generation)
            .unwrap_or(1);
        crate::runner::exchange(
            &socket,
            &format!("/v1/containers/{run_id}/teardown"),
            Some(json!({"generation":generation})),
        )
        .await?;
        registry().handles.lock().await.remove(run_id);
        return Ok(());
    }
    let physical_setup = run_id
        .starts_with(SHARED_CONTAINER_RUN_ID)
        .then(|| container_lifecycle_lock(&format!("setup:{run_id}")));
    let _physical_setup = match &physical_setup {
        Some(lock) => Some(lock.lock().await),
        None => None,
    };
    let lifecycle = container_lifecycle_lock(run_id);
    let _lifecycle_guard = lifecycle.lock().await;
    let handle = registry().handles.lock().await.get(run_id).cloned();
    let Some(handle) = handle else {
        return Ok(());
    };
    if handle.scope == "shared" {
        let base = {
            registry()
                .handles
                .lock()
                .await
                .values()
                .find(|other| other.scope == "run" && other.container_id == handle.container_id)
                .cloned()
        }
        .ok_or("Shared physical runtime ownership is unavailable")?;
        let setup = container_lifecycle_lock(&format!("setup:{}", base.run_id));
        let _setup = setup.lock().await;
        let others = registry().handles.lock().await.values().any(|other| {
            other.scope == "shared"
                && other.run_id != run_id
                && other.container_id == handle.container_id
        });
        if others {
            let result = handle
                .link
                .teardown(
                    Some(run_id),
                    Some(&handle.owner_token),
                    Some(&handle.container_workspace),
                )
                .await?;
            if !result.error.is_empty() || !result.scoped_teardown || !result.ok {
                return Err(format!(
                    "Scoped Run teardown is unconfirmed: {}",
                    result.error
                ));
            }
        } else {
            let physical = container_lifecycle_lock(&base.run_id);
            let _physical = physical.lock().await;
            destroy_runtime(&base).await?;
        }
        registry().handles.lock().await.remove(run_id);
        for owned in registry().owners.lock().await.values_mut() {
            owned.retain(|id| id != run_id);
        }
        return Ok(());
    }
    if run_id.starts_with(SHARED_CONTAINER_RUN_ID)
        && registry()
            .handles
            .lock()
            .await
            .values()
            .any(|other| other.scope == "shared" && other.container_id == handle.container_id)
    {
        return Err("Physical shared runtime still has active Run owners".into());
    }
    destroy_runtime(&handle).await
}

/// Only called with exclusive lifecycle ownership (and the pool setup lock for shared runtimes).
async fn destroy_runtime(handle: &ContainerHandle) -> Result<(), String> {
    let _ = tokio::time::timeout(
        Duration::from_secs(5),
        handle.link.teardown(None, None, None),
    )
    .await;
    let output = tokio::time::timeout(
        Duration::from_secs(15),
        crate::host_process::command("docker")
            .args(["rm", "-f", &handle.container_id])
            .stdin(std::process::Stdio::null())
            .kill_on_drop(true)
            .output(),
    )
    .await
    .map_err(|_| "Docker removal timed out; runtime stop remains unconfirmed")?
    .map_err(|e| e.to_string())?;
    let missing = docker_reports_absent(&output.stderr);
    if !output.status.success() && !missing {
        return Err(format!(
            "Docker runtime {} exit is unconfirmed: {}",
            handle.container_id,
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    if let Ok(receiver) = get_receiver().await {
        receiver.forget(&handle.run_id);
    }
    let dirs: Vec<_> = [&handle.host_control_dir]
        .into_iter()
        .chain(handle.host_accounts_dir.as_ref())
        .cloned()
        .collect();
    tokio::task::spawn_blocking(move || -> Result<(), String> {
        for dir in dirs {
            if let Err(error) = std::fs::remove_dir_all(dir)
                && error.kind() != std::io::ErrorKind::NotFound
            {
                return Err(error.to_string());
            }
        }
        Ok(())
    })
    .await
    .map_err(|e| e.to_string())??;
    let mut handles = registry().handles.lock().await;
    if handles
        .get(&handle.run_id)
        .is_some_and(|current| current.container_id == handle.container_id)
    {
        handles.remove(&handle.run_id);
    }
    drop(handles);
    for owned in registry().owners.lock().await.values_mut() {
        owned.retain(|id| id != &handle.run_id);
    }
    Ok(())
}

pub async fn teardown_orphan_run_container(
    state: &AppState,
    run_id: &str,
    expected: bool,
) -> Result<(), String> {
    if crate::runner_remote::controller::stop_recorded(state, run_id).await? {
        return Ok(());
    }
    if let Some(socket) = crate::runner::configured_socket() {
        crate::runner::exchange(&socket,&format!("/v1/containers/{run_id}/orphan-teardown"),Some(json!({"expected":expected,
            "generation":state.store.get("runs",run_id).map_err(|e|e.to_string())?.and_then(|run|run["execution_generation"].as_u64()).unwrap_or(1)}))).await?;
        return Ok(());
    }
    let lifecycle = container_lifecycle_lock(run_id);
    let _lifecycle_guard = lifecycle.lock().await;
    if let Some(run) = state.store.get("runs", run_id).map_err(|e| e.to_string())?
        && run["worker_runtime"]["worker_container_scope"] == "shared"
    {
        let runtime = super::shared_pools::registered_runtime(state, run_id)?;
        let setup = container_lifecycle_lock(&format!("setup:{runtime}"));
        let _setup = setup.lock().await;
        let name = format!("muteki-worker-{}", sanitize_container_suffix(&runtime));
        let output = crate::host_process::command("docker")
            .args(["inspect", &name])
            .kill_on_drop(true)
            .output()
            .await
            .map_err(|e| e.to_string())?;
        if !output.status.success() {
            if docker_reports_absent(&output.stderr) {
                return Ok(());
            }
            return Err("Shared runtime absence could not be verified".into());
        }
        let rows: Value = serde_json::from_slice(&output.stdout).map_err(|e| e.to_string())?;
        if rows[0]["State"]["Running"] != true {
            return Ok(());
        }
        let live = registry()
            .handles
            .lock()
            .await
            .get(&runtime)
            .is_some_and(|handle| handle.link.is_alive());
        if live {
            return Ok(());
        }
        return Err("Shared runtime still has no authenticated owner teardown channel".into());
    }
    let state_dir = native_chat::node_state_dir(state);
    let listed = crate::host_process::command("docker")
        .args([
            "ps",
            "-a",
            "--filter",
            &format!("label=muteki.owner_run_id={run_id}"),
            "--filter",
            &format!("label=muteki.state_dir={}", state_dir.display()),
            "--format",
            "{{.Names}}",
        ])
        .stdin(std::process::Stdio::null())
        .output()
        .await;
    let mut names = match listed {
        Ok(output) if output.status.success() => String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter(|name| !name.is_empty())
            .map(str::to_owned)
            .collect::<Vec<_>>(),
        Ok(output) if expected => {
            return Err(format!(
                "Docker Run containers cannot be listed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
        Err(error) if expected => return Err(error.to_string()),
        _ => Vec::new(),
    };
    let labelled = !names.is_empty();
    let deterministic = format!("muteki-worker-{}", sanitize_container_suffix(run_id));
    if !names.iter().any(|name| name == &deterministic) {
        names.push(deterministic);
    }
    for name in names {
        let result = crate::host_process::command("docker")
            .args(["rm", "-f", &name])
            .stdin(std::process::Stdio::null())
            .output()
            .await;
        let removed = match result {
            Ok(output) if output.status.success() || docker_reports_absent(&output.stderr) => true,
            Ok(output) if expected || labelled => {
                return Err(format!(
                    "Docker container {name} exit is unconfirmed: {}",
                    String::from_utf8_lossy(&output.stderr).trim()
                ));
            }
            Err(error) if expected || labelled => return Err(error.to_string()),
            _ => false,
        };
        if removed && let Some(suffix) = name.strip_prefix("muteki-worker-") {
            for dir in [
                state_dir.join("runtime/control").join(suffix),
                state_dir.join("runtime/account-projections").join(suffix),
            ] {
                if let Err(error) = std::fs::remove_dir_all(&dir)
                    && error.kind() != std::io::ErrorKind::NotFound
                {
                    return Err(error.to_string());
                }
            }
        }
    }
    Ok(())
}

pub async fn teardown_run_container(run_id: &str) {
    let _ = teardown_run_container_checked(run_id).await;
}

struct PiContainerAccount {
    id: String,
    secret: String,
    base_url: String,
    model: String,
    capability: Value,
}

fn pi_container_account(
    state: &AppState,
    engine: &str,
    credential_id: &str,
    model: Option<&str>,
) -> Result<Option<PiContainerAccount>, RuntimeError> {
    if engine != "pi" {
        return Ok(None);
    }
    let credentials::CredentialRef::Account(id) = credentials::canonical(credential_id, engine)?
    else {
        return Ok(None);
    };
    let row = credentials::account(&state.store, &id).ok_or_else(|| {
        RuntimeError::new("credential.unavailable", format!("凭据账号不可用：{id}"))
    })?;
    let owner = row["worker_engine"]
        .as_str()
        .or_else(|| row["engine"].as_str())
        .unwrap_or("");
    if !["", "api", "unknown", "pi"].contains(&owner) {
        return Err(RuntimeError::new(
            "credential.engine_mismatch",
            format!("凭据 account:{id} 属于 {owner}，不能用于 pi"),
        ));
    }
    let state_dir = native_chat::state_dir(state);
    let imported = crate::account_material::base(&state_dir, &id, &row)
        .map_err(|error| RuntimeError::new("credential.material_invalid", error.to_string()))?;
    let read_file = |name: &str| -> Option<String> {
        std::fs::read_to_string(imported.join(name))
            .ok()
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty())
    };
    let secret = row["secret"]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .or_else(|| read_file("API_KEY"))
        .ok_or_else(|| RuntimeError::new("credential.unavailable", "Pi 账号缺少 API Key"))?;
    let base_url = row["base_url"]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .or_else(|| read_file("BASE_URL"))
        .ok_or_else(|| RuntimeError::new("credential.unavailable", "Pi 账号缺少 API Base URL"))?;
    let model = model
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .or_else(|| row["default_model"].as_str())
        .or_else(|| row["target_model"].as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .or_else(|| read_file("MODEL"))
        .ok_or_else(|| {
            RuntimeError::new("credential.model_missing", "Pi 自定义端点必须指定模型")
        })?;
    if row["models"].as_array().is_some_and(|models| {
        !models.is_empty() && !models.iter().any(|item| item.as_str() == Some(&model))
    }) {
        return Err(RuntimeError::new(
            "credential.model_mismatch",
            format!("模型 {model} 不属于凭据 account:{id} 的已登记模型"),
        ));
    }
    Ok(Some(PiContainerAccount {
        id,
        secret,
        base_url,
        capability: credentials::pi_model_capabilities(Some(&imported), &row, &model)?,
        model,
    }))
}

pub async fn validate_container_credential(
    state: &AppState,
    engine: &str,
    credential_id: &str,
    model: Option<&str>,
) -> Result<(), RuntimeError> {
    if pi_container_account(state, engine, credential_id, model)?.is_some() {
        return Ok(());
    }
    let desc = descriptors::find(engine)
        .ok_or_else(|| RuntimeError::new("engine.unknown", format!("未知引擎：{engine}")))?;
    credentials::resolve(
        &state.store,
        &native_chat::state_dir(state),
        desc,
        credential_id,
        model,
    )
    .await?;
    Ok(())
}

pub async fn prepare_container_worker_env(
    state: &AppState,
    handle: &ContainerHandle,
    solver_id: &str,
    engine: &str,
    credential_id: &str,
    model: Option<&str>,
    extra_env: &[(String, String)],
) -> Result<(HashMap<String, String>, PathBuf), RuntimeError> {
    let desc = descriptors::find(engine)
        .ok_or_else(|| RuntimeError::new("engine.unknown", format!("未知引擎：{engine}")))?;
    let state_dir = native_chat::node_state_dir(state);
    let pi_account = pi_container_account(state, engine, credential_id, model)?;
    let resolved = if let Some(account) = &pi_account {
        credentials::Resolved {
            credential_id: format!("account:{}", account.id),
            env: Vec::new(),
        }
    } else {
        credentials::resolve(
            &state.store,
            &native_chat::state_dir(state),
            desc,
            credential_id,
            model,
        )
        .await?
    };

    super::record_credential_projection(state, &handle.run_id, solver_id, &resolved.credential_id);
    let safe_solver = sanitize_container_suffix(solver_id);
    let host_home = handle.host_workspace.join("homes").join(&safe_solver);
    crate::private_files::ensure_dir(&state_dir, &host_home)
        .map_err(|e| RuntimeError::new("runtime.io_error", e))?;
    let container_home = handle
        .to_container_path(&host_home)
        .map_err(|e| RuntimeError::new("runtime.io_error", e))?;

    // Pre-seed Claude onboarding bypass so container runs never block on first-run wizard.
    let claude_json = host_home.join(".claude.json");
    if !claude_json.exists() {
        write_private_file(
            &state_dir,
            &claude_json,
            br#"{"hasCompletedOnboarding":true,"bypassPermissionsModeAccepted":true}"#,
        )
        .map_err(|e| RuntimeError::new("runtime.io_error", e))?;
    }

    let mut env_map: HashMap<String, String> = HashMap::new();
    env_map.insert("HOME".to_owned(), container_home.clone());
    env_map.insert(
        "XDG_CONFIG_HOME".to_owned(),
        format!("{container_home}/.config"),
    );
    env_map.insert(
        "XDG_DATA_HOME".to_owned(),
        format!("{container_home}/.local/share"),
    );
    env_map.insert(
        "XDG_CACHE_HOME".to_owned(),
        format!("{container_home}/.cache"),
    );
    env_map.insert(
        "XDG_STATE_HOME".to_owned(),
        format!("{container_home}/.local/state"),
    );

    let rewrite_bridge = handle.link.remote.is_none() && handle.network_mode == "bridge";
    if handle.has_gateway_proxy {
        let configured = crate::network_proxy::resolve(state, rewrite_bridge);
        if configured.mode != "inherit" {
            tracing::debug!(
                run_id = %handle.run_id,
                mode = %configured.mode,
                "Preserving SRC traffic recording gateway proxy on worker container"
            );
        }
    } else {
        let resolved_proxy = crate::network_proxy::resolve(state, rewrite_bridge);
        match resolved_proxy.mode.as_str() {
            "custom" | "direct" => {
                env_map.extend(crate::network_proxy::env_pairs(&resolved_proxy));
            }
            _ => {
                if rewrite_bridge && let Some(proxy_url) = detect_host_proxy_for_container() {
                    for k in ["HTTP_PROXY", "HTTPS_PROXY", "http_proxy", "https_proxy"] {
                        env_map.insert(k.to_owned(), proxy_url.clone());
                    }
                    let no_proxy = "127.0.0.1,localhost,host.docker.internal,172.17.0.1,10.0.0.0/8,172.16.0.0/12,192.168.0.0/16";
                    env_map.insert("NO_PROXY".to_owned(), no_proxy.to_owned());
                    env_map.insert("no_proxy".to_owned(), no_proxy.to_owned());
                }
            }
        }
    }

    let rewrite_localhost_url = |url: &str| -> String {
        if handle.link.remote.is_none() && handle.network_mode == "bridge" {
            match url::Url::parse(url) {
                Ok(mut parsed)
                    if matches!(
                        parsed.host_str(),
                        Some("127.0.0.1" | "localhost" | "[::1]" | "::1")
                    ) =>
                {
                    let _ = parsed.set_host(Some("host.docker.internal"));
                    parsed.to_string()
                }
                _ => url.to_owned(),
            }
        } else {
            url.to_owned()
        }
    };

    if let Some(account) = pi_account {
        let agent_dir = host_home.join(".pi/agent");
        let mut definition = json!({"id":account.model,
            "input":account.capability["input"].as_array().cloned().unwrap_or_else(||vec![json!("text")]),
            "reasoning":account.capability["reasoning"].as_bool().unwrap_or(false)});
        for (source, target) in [
            ("context_window", "contextWindow"),
            ("contextWindow", "contextWindow"),
            ("max_output_tokens", "maxTokens"),
            ("maxTokens", "maxTokens"),
            ("thinkingLevelMap", "thinkingLevelMap"),
        ] {
            if let Some(value) = account.capability.get(source) {
                definition[target] = value.clone();
            }
        }
        let models = json!({"providers": {"muteki": {
            "baseUrl": rewrite_localhost_url(&account.base_url),
            "api": "openai-completions",
            "apiKey": account.secret,
            "models": [definition],
        }}});
        let models_bytes = serde_json::to_vec_pretty(&models)
            .map_err(|e| RuntimeError::new("runtime.io_error", e.to_string()))?;
        write_private_file(&state_dir, &agent_dir.join("models.json"), &models_bytes)
            .map_err(|e| RuntimeError::new("runtime.io_error", e.to_string()))?;
        let window = account.capability["context_window"]
            .as_u64()
            .or_else(|| account.capability["contextWindow"].as_u64());
        let settings = json!({
            "httpIdleTimeoutMs": 300000,
            "compaction": credentials::pi_compaction_settings(window),
            "retry": {"provider": {"timeoutMs": 300000, "maxRetries": 0}},
        });
        let settings_bytes = serde_json::to_vec_pretty(&settings)
            .map_err(|e| RuntimeError::new("runtime.io_error", e.to_string()))?;
        write_private_file(
            &state_dir,
            &agent_dir.join("settings.json"),
            &settings_bytes,
        )
        .map_err(|e| RuntimeError::new("runtime.io_error", e.to_string()))?;
        env_map.insert(
            "PI_CODING_AGENT_DIR".to_owned(),
            format!("{container_home}/.pi/agent"),
        );
        env_map.insert("PI_PROVIDER".to_owned(), "muteki".to_owned());
        env_map.insert("PI_MODEL".to_owned(), account.model);
    } else if resolved.credential_id.starts_with("system:") {
        let host_user_home = std::env::var("HOME").ok().map(PathBuf::from);
        for key in desc.public()["credentials"]["env_keys"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
        {
            let Ok(value) = std::env::var(key) else {
                continue;
            };
            if value.trim().is_empty() {
                continue;
            }
            if let Some(target) = key.strip_suffix("_FILE") {
                if !env_map.contains_key(target) {
                    let bytes = std::fs::read_to_string(&value).map_err(|error| {
                        RuntimeError::new(
                            "credential.material_invalid",
                            format!("cannot read {key}: {error}"),
                        )
                    })?;
                    env_map.insert(target.into(), bytes.trim().into());
                }
            } else if !matches!(
                key,
                "CODEX_HOME"
                    | "KIMI_CODE_HOME"
                    | "GROK_HOME"
                    | "PI_CODING_AGENT_DIR"
                    | "OPENCODE_CONFIG"
                    | "OPENCODE_CONFIG_DIR"
            ) {
                env_map.insert(
                    key.into(),
                    if matches!(
                        key,
                        "OPENAI_BASE_URL"
                            | "ANTHROPIC_BASE_URL"
                            | "CURSOR_ENDPOINT"
                            | "GROK_MODELS_BASE_URL"
                            | "KIMI_MODEL_BASE_URL"
                    ) {
                        rewrite_localhost_url(&value)
                    } else {
                        value
                    },
                );
            }
        }
        for (key, value) in crate::agent_runtime::host_login::container_tokens(engine).await {
            env_map.entry(key).or_insert(value);
        }
        for (key, value) in &resolved.env {
            env_map.insert(key.clone(), value.clone());
        }
        match engine {
            "codex" => {
                if let Some(src_codex) = crate::agent_runtime::host_login::login_root("codex") {
                    let dst_codex = host_home.join(".codex");
                    for name in ["auth.json", "config.toml"] {
                        let path = src_codex.join(name);
                        match std::fs::read(&path) {
                            Ok(bytes) => {
                                write_private_file(&state_dir, &dst_codex.join(name), &bytes)
                                    .map_err(|error| RuntimeError::new("runtime.io_error", error))?
                            }
                            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                            Err(error) => {
                                return Err(RuntimeError::new(
                                    "credential.material_invalid",
                                    format!("cannot project Codex {name}: {error}"),
                                ));
                            }
                        }
                    }
                    env_map.insert("CODEX_HOME".to_owned(), format!("{container_home}/.codex"));
                }
            }
            "claude" => {
                for k in [
                    "ANTHROPIC_API_KEY",
                    "ANTHROPIC_AUTH_TOKEN",
                    "CLAUDE_CODE_OAUTH_TOKEN",
                    "ANTHROPIC_BASE_URL",
                ] {
                    if let Ok(v) = std::env::var(k)
                        && !v.trim().is_empty()
                    {
                        let val = if k.ends_with("_URL") {
                            rewrite_localhost_url(&v)
                        } else {
                            v
                        };
                        env_map.insert(k.to_owned(), val);
                    }
                }
                if let Some(h) = &host_user_home {
                    let src_claude = crate::agent_runtime::host_login::login_root("claude")
                        .unwrap_or_else(|| h.join(".claude"));
                    let dst_claude = host_home.join(".claude");
                    for name in [".credentials.json", "settings.json"] {
                        let p = src_claude.join(name);
                        if let Ok(bytes) = std::fs::read(&p) {
                            write_private_file(&state_dir, &dst_claude.join(name), &bytes)
                                .map_err(|e| RuntimeError::new("runtime.io_error", e))?;
                        }
                    }
                }
            }
            "grok" => {
                if let Some(h) = &host_user_home {
                    let src = crate::agent_runtime::host_login::login_root("grok")
                        .unwrap_or_else(|| h.join(".grok"));
                    let dst = host_home.join(".grok");
                    copy_dir_recursive(&state_dir, &src, &dst)
                        .map_err(|e| RuntimeError::new("runtime.io_error", e))?;
                    env_map.insert("GROK_HOME".to_owned(), format!("{container_home}/.grok"));
                }
            }
            "kimi" => {
                if let Some(h) = &host_user_home {
                    let src = crate::agent_runtime::host_login::login_root("kimi")
                        .unwrap_or_else(|| h.join(".kimi-code"));
                    let dst = host_home.join(".kimi-code");
                    copy_dir_recursive(&state_dir, &src, &dst)
                        .map_err(|e| RuntimeError::new("runtime.io_error", e))?;
                    env_map.insert(
                        "KIMI_CODE_HOME".to_owned(),
                        format!("{container_home}/.kimi-code"),
                    );
                }
            }
            "opencode" => {
                if let Some(h) = &host_user_home {
                    let src = std::env::var_os("XDG_DATA_HOME")
                        .map(PathBuf::from)
                        .unwrap_or_else(|| h.join(".local/share"))
                        .join("opencode");
                    let dst = host_home.join(".local/share/opencode");
                    copy_dir_recursive(&state_dir, &src, &dst)
                        .map_err(|e| RuntimeError::new("runtime.io_error", e))?;
                    let config_src = std::env::var_os("OPENCODE_CONFIG_DIR")
                        .map(PathBuf::from)
                        .unwrap_or_else(|| {
                            std::env::var_os("XDG_CONFIG_HOME")
                                .map(PathBuf::from)
                                .unwrap_or_else(|| h.join(".config"))
                                .join("opencode")
                        });
                    copy_dir_recursive(
                        &state_dir,
                        &config_src,
                        &host_home.join(".config/opencode"),
                    )
                    .map_err(|error| RuntimeError::new("runtime.io_error", error))?;
                    env_map.insert(
                        "OPENCODE_CONFIG_DIR".into(),
                        format!("{container_home}/.config/opencode"),
                    );
                    if let Some(path) = std::env::var_os("OPENCODE_CONFIG") {
                        let bytes = std::fs::read(path).map_err(|error| {
                            RuntimeError::new("credential.material_invalid", error.to_string())
                        })?;
                        write_private_file(
                            &state_dir,
                            &host_home.join(".config/opencode/selected.jsonc"),
                            &bytes,
                        )
                        .map_err(|error| RuntimeError::new("runtime.io_error", error))?;
                        env_map.insert(
                            "OPENCODE_CONFIG".into(),
                            format!("{container_home}/.config/opencode/selected.jsonc"),
                        );
                    }
                }
            }
            "pi" => {
                if let Some(h) = &host_user_home {
                    let src = crate::agent_runtime::host_login::login_root("pi")
                        .unwrap_or_else(|| h.join(".pi/agent"));
                    let dst = host_home.join(".pi/agent");
                    copy_dir_recursive(&state_dir, &src, &dst)
                        .map_err(|e| RuntimeError::new("runtime.io_error", e))?;
                    env_map.insert(
                        "PI_CODING_AGENT_DIR".into(),
                        format!("{container_home}/.pi/agent"),
                    );
                }
            }
            "omp" => {
                if let Some(h) = &host_user_home {
                    let src = crate::agent_runtime::host_login::login_root("omp")
                        .unwrap_or_else(|| h.join(".omp/agent"));
                    let dst = host_home.join(".omp/agent");
                    copy_dir_recursive(&state_dir, &src, &dst)
                        .map_err(|e| RuntimeError::new("runtime.io_error", e))?;
                    env_map.insert(
                        "PI_CODING_AGENT_DIR".into(),
                        format!("{container_home}/.omp/agent"),
                    );
                }
            }
            "droid"
                if env_map
                    .get("FACTORY_API_KEY")
                    .is_none_or(|key| key.trim().is_empty()) =>
            {
                return Err(RuntimeError::new(
                    "credential.container_login_unavailable",
                    "Droid 本机安全登录存储不能直接投递到容器；请选择 Factory API Key 凭据",
                ));
            }
            _ => {}
        }
    } else {
        let acct_id = resolved
            .credential_id
            .strip_prefix("account:")
            .unwrap_or(&resolved.credential_id);
        let safe_acct = sanitize_container_suffix(acct_id);
        let acct_host_dir = handle
            .host_accounts_dir
            .as_ref()
            .map(|d| d.join(&safe_acct));

        for (k, v) in &resolved.env {
            match k.as_str() {
                "XDG_DATA_HOME" | "XDG_CONFIG_HOME" | "XDG_CACHE_HOME" | "XDG_STATE_HOME" => {}
                "OPENCODE_CONFIG" => {}
                _ if v.is_empty() => {
                    env_map.insert(k.clone(), String::new());
                }
                "ANTHROPIC_API_KEY"
                | "ANTHROPIC_AUTH_TOKEN"
                | "CLAUDE_CODE_OAUTH_TOKEN"
                | "OPENAI_API_KEY"
                | "OPENCODE_API_KEY"
                | "CURSOR_API_KEY"
                | "CURSOR_AUTH_TOKEN"
                | "FACTORY_API_KEY"
                | "XAI_API_KEY"
                | "KIMI_MODEL_API_KEY" => {
                    if let Some(acct_dir) = &acct_host_dir {
                        let file_path = acct_dir.join(k);
                        write_private_file(&state_dir, &file_path, v.as_bytes())
                            .map_err(|e| RuntimeError::new("runtime.io_error", e))?;
                        env_map.insert(
                            format!("{k}_FILE"),
                            format!("{CONTAINER_ACCOUNTS_ROOT}/{safe_acct}/{k}"),
                        );
                    }
                    env_map.insert(k.clone(), v.clone());
                }
                "ANTHROPIC_BASE_URL"
                | "OPENAI_BASE_URL"
                | "CURSOR_ENDPOINT"
                | "GROK_MODELS_BASE_URL"
                | "KIMI_MODEL_BASE_URL" => {
                    env_map.insert(k.clone(), rewrite_localhost_url(v));
                }
                "CODEX_HOME" => {
                    let src = Path::new(v);
                    let dst = host_home.join(".codex");
                    copy_dir_recursive(&state_dir, src, &dst)
                        .map_err(|e| RuntimeError::new("runtime.io_error", e))?;
                    env_map.insert("CODEX_HOME".to_owned(), format!("{container_home}/.codex"));
                }
                "KIMI_CODE_HOME" => {
                    let src = Path::new(v);
                    let dst = host_home.join(".kimi-code");
                    copy_dir_recursive(&state_dir, src, &dst)
                        .map_err(|e| RuntimeError::new("runtime.io_error", e))?;
                    env_map.insert(
                        "KIMI_CODE_HOME".to_owned(),
                        format!("{container_home}/.kimi-code"),
                    );
                }
                "PI_CODING_AGENT_DIR" if engine == "omp" => {
                    let src = Path::new(v);
                    let dst = host_home.join(".omp/agent");
                    copy_dir_recursive(&state_dir, src, &dst)
                        .map_err(|e| RuntimeError::new("runtime.io_error", e.to_string()))?;
                    env_map.insert(
                        "PI_CODING_AGENT_DIR".to_owned(),
                        format!("{container_home}/.omp/agent"),
                    );
                }
                "GROK_HOME" => {
                    let src = Path::new(v);
                    let dst = host_home.join(".grok");
                    copy_dir_recursive(&state_dir, src, &dst)
                        .map_err(|e| RuntimeError::new("runtime.io_error", e))?;
                    env_map.insert("GROK_HOME".to_owned(), format!("{container_home}/.grok"));
                }
                _ => {
                    env_map.insert(k.clone(), v.clone());
                }
            }
        }
    }

    for (k, v) in extra_env {
        env_map.insert(k.clone(), v.clone());
    }
    if handle.link.remote.is_none() && handle.network_mode == "bridge" {
        for key in [
            "OPENAI_BASE_URL",
            "ANTHROPIC_BASE_URL",
            "CURSOR_ENDPOINT",
            "GROK_MODELS_BASE_URL",
            "KIMI_MODEL_BASE_URL",
        ] {
            if let Some(value) = env_map.get_mut(key) {
                *value = rewrite_localhost_url(value);
            }
        }
        project_provider_urls(&state_dir, &host_home, &mut env_map, &rewrite_localhost_url)?;
    }

    env_map.insert("USER".to_owned(), "root".to_owned());
    env_map.insert("LOGNAME".to_owned(), "root".to_owned());
    env_map.insert("IS_SANDBOX".to_owned(), "1".to_owned());

    chown_tree_to_worker(
        &handle.host_workspace,
        &host_home,
        handle.workspace_uid,
        handle.workspace_gid,
    );
    chown_ancestors_to_worker(
        &handle.host_workspace,
        &host_home,
        handle.workspace_uid,
        handle.workspace_gid,
    );
    if let Some(accounts) = &handle.host_accounts_dir {
        chown_tree_to_worker(
            accounts,
            accounts,
            handle.workspace_uid,
            handle.workspace_gid,
        );
    }

    if let Some(remote) = &handle.link.remote {
        remote
            .credentials(
                solver_id,
                engine,
                credential_id,
                model,
                &host_home,
                &mut env_map,
            )
            .await
            .map_err(|error| RuntimeError::new("runtime.remote_credentials", error))?;
    }
    if state
        .store
        .get("runs", &handle.run_id)
        .ok()
        .flatten()
        .is_some()
        && super::launch_config::settings(state, &handle.run_id)
            .is_ok_and(|settings| settings["offline"] == true)
    {
        env_map.insert(WEB_TOOLS_ENV.to_owned(), "disabled".to_owned());
    }
    Ok((env_map, host_home))
}

pub const WEB_TOOLS_ENV: &str = "MUTEKI_AGENT_WEB_TOOLS";

pub fn check_web_tools_withholding(engine: &str) -> Result<(), RuntimeError> {
    match engine.trim().to_ascii_lowercase().as_str() {
        "claude" | "codex" | "pi" => Ok(()),
        other => Err(RuntimeError::unsupported(format!(
            "引擎 {other} 无法在命令行禁用内置搜索工具；请在发起任务时开启 Agent 搜索工具，或改用 Claude、Codex、Pi"
        ))),
    }
}

/// Extra CLI arguments passed to a host-local Agent runtime when built-in search/fetch tools are withheld.
pub fn local_web_tools_withholding_args(engine: &str) -> Result<Vec<String>, RuntimeError> {
    let normalized = engine.trim().to_ascii_lowercase();
    check_web_tools_withholding(&normalized)?;
    Ok(match normalized.as_str() {
        "claude" => vec![
            "--disallowedTools".to_owned(),
            "WebSearch,WebFetch".to_owned(),
        ],
        "codex" => vec!["-c".to_owned(), "web_search=\"disabled\"".to_owned()],
        // Restrict Pi to its local file/shell tools and skip user MCP/extensions when web tools are withheld.
        "pi" => vec![
            "--tools".to_owned(),
            "read,bash,edit,write,grep,find,ls".to_owned(),
            "--no-mcp".to_owned(),
            "--no-extensions".to_owned(),
        ],
        _ => unreachable!(),
    })
}

/// The Run asked for the engine's own search/fetch tools to be withheld. Network egress is a
/// separate policy; engines without a CLI switch fail instead of running with the tools enabled.
fn withhold_web_tools(engine: &str, argv: &mut Vec<String>) -> Result<(), RuntimeError> {
    let normalized = engine.trim().to_ascii_lowercase();
    check_web_tools_withholding(&normalized)?;
    match normalized.as_str() {
        "claude" => argv.extend([
            "--disallowedTools".to_owned(),
            "WebSearch,WebFetch".to_owned(),
        ]),
        "codex" => {
            argv.splice(
                2..2,
                ["-c".to_owned(), "web_search=\"disabled\"".to_owned()],
            );
        }
        "pi" => {}
        _ => unreachable!(),
    }
    Ok(())
}
pub fn build_container_cli_invocation(
    engine: &str,
    model: Option<&str>,
    reasoning_effort: Option<&str>,
    prompt: &str,
    env: &HashMap<String, String>,
    interactive: bool,
) -> Result<(Vec<String>, Option<String>), RuntimeError> {
    let (mut argv, stdin) =
        build_engine_invocation(engine, model, reasoning_effort, prompt, env, interactive)?;
    if env.get(WEB_TOOLS_ENV).map(String::as_str) == Some("disabled") {
        withhold_web_tools(&engine.trim().to_ascii_lowercase(), &mut argv)?;
    }
    Ok((argv, stdin))
}
fn build_engine_invocation(
    engine: &str,
    model: Option<&str>,
    reasoning_effort: Option<&str>,
    prompt: &str,
    env: &HashMap<String, String>,
    interactive: bool,
) -> Result<(Vec<String>, Option<String>), RuntimeError> {
    let eng = engine.trim().to_ascii_lowercase();
    let m = model.map(str::trim).filter(|s| !s.is_empty());
    let eff = reasoning_effort
        .map(str::trim)
        .filter(|s| !s.is_empty() && *s != "default");

    match eng.as_str() {
        "claude" => {
            let mut argv = vec![
                "claude".to_owned(),
                "-p".to_owned(),
                "--output-format".to_owned(),
                "stream-json".to_owned(),
                "--verbose".to_owned(),
                "--dangerously-skip-permissions".to_owned(),
                "--no-session-persistence".to_owned(),
            ];
            if let Some(model_id) = m {
                argv.extend(["--model".to_owned(), model_id.to_owned()]);
            }
            if let Some(effort) = eff
                && ["low", "medium", "high", "max"].contains(&effort)
            {
                argv.extend(["--effort".to_owned(), effort.to_owned()]);
            }
            Ok((argv, Some(prompt.to_owned())))
        }
        "codex" => {
            let mut argv = vec!["codex".to_owned(), "exec".to_owned()];
            let pairs = env
                .iter()
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect::<Vec<_>>();
            argv.extend(agent::codex::provider_args(&pairs).map_err(|error| {
                RuntimeError::new("credential.provider_config_invalid", error.to_string())
            })?);
            if let Some(effort) = eff {
                let mapped = if effort == "max" { "xhigh" } else { effort };
                if ["minimal", "low", "medium", "high", "xhigh"].contains(&mapped) {
                    argv.extend([
                        "-c".to_owned(),
                        format!("model_reasoning_effort=\"{mapped}\""),
                    ]);
                }
            }
            argv.extend([
                "--json".to_owned(),
                "--ephemeral".to_owned(),
                "--dangerously-bypass-approvals-and-sandbox".to_owned(),
                "--skip-git-repo-check".to_owned(),
            ]);
            if let Some(model_id) = m {
                argv.extend(["--model".to_owned(), model_id.to_owned()]);
            }
            argv.push("-".to_owned());
            Ok((argv, Some(prompt.to_owned())))
        }
        "cursor" => {
            let mut argv = vec![
                "/home/kali/.local/bin/cursor-agent".to_owned(),
                "-p".to_owned(),
                "--output-format".to_owned(),
                "stream-json".to_owned(),
                "--force".to_owned(),
                "--trust".to_owned(),
            ];
            if let Some(endpoint) = env
                .get("CURSOR_ENDPOINT")
                .filter(|value| !value.trim().is_empty())
            {
                argv.extend(["--endpoint".into(), endpoint.clone()]);
            }
            if let Some(model_id) = m {
                argv.extend(["--model".to_owned(), model_id.to_owned()]);
            }
            argv.push(prompt.to_owned());
            Ok((argv, None))
        }
        "droid" => {
            let mut argv = vec![
                "droid".to_owned(),
                "exec".to_owned(),
                "--skip-permissions-unsafe".to_owned(),
                "--output-format".to_owned(),
                "stream-json".to_owned(),
            ];
            if let Some(model_id) = m {
                argv.extend(["--model".to_owned(), model_id.to_owned()]);
            }
            if let Some(effort) = eff {
                argv.extend(["--reasoning-effort".to_owned(), effort.to_owned()]);
            }
            argv.extend(["--file".to_owned(), "/dev/stdin".to_owned()]);
            Ok((argv, Some(prompt.to_owned())))
        }
        "opencode" => {
            let mut argv = vec![
                "opencode".to_owned(),
                "run".to_owned(),
                "--format".to_owned(),
                "json".to_owned(),
            ];
            let selected = m.or_else(|| {
                env.get("MUTEKI_OPENCODE_MODEL")
                    .map(String::as_str)
                    .filter(|value| !value.is_empty())
            });
            if selected.is_none()
                && env
                    .get("MUTEKI_OPENCODE_PROVIDER")
                    .is_some_and(|value| !value.is_empty())
            {
                return Err(RuntimeError::new(
                    "credential.model_required",
                    "OpenCode 自定义端点必须指定模型 ID",
                ));
            }
            if let Some(model_id) = selected {
                let selected = match env.get("MUTEKI_OPENCODE_PROVIDER").map(String::as_str) {
                    Some(provider)
                        if !provider.is_empty()
                            && !model_id.starts_with(&format!("{provider}/")) =>
                    {
                        format!("{provider}/{model_id}")
                    }
                    _ => model_id.to_owned(),
                };
                argv.extend(["--model".to_owned(), selected]);
            }
            argv.push(prompt.to_owned());
            Ok((argv, None))
        }
        "kimi" => {
            let mut argv = vec![
                "kimi".to_owned(),
                "--prompt".to_owned(),
                prompt.to_owned(),
                "--output-format".to_owned(),
                "stream-json".to_owned(),
            ];
            if let Some(model_id) = m.filter(|_| {
                env.get("KIMI_MODEL_NAME")
                    .is_none_or(|value| value.trim().is_empty())
            }) {
                argv.extend(["--model".to_owned(), model_id.to_owned()]);
            }
            Ok((argv, None))
        }
        "pi" => {
            let mut argv = if interactive {
                vec![
                    "pi".to_owned(),
                    "--mode".to_owned(),
                    "rpc".to_owned(),
                    "--no-session".to_owned(),
                ]
            } else {
                vec![
                    "pi".to_owned(),
                    "-p".to_owned(),
                    "--mode".to_owned(),
                    "json".to_owned(),
                    "--no-session".to_owned(),
                ]
            };
            if env.get("MUTEKI_PI_NO_EXTENSIONS").map(String::as_str) == Some("1") {
                argv.extend([
                    "--no-extensions".to_owned(),
                    "--no-skills".to_owned(),
                    "--no-context-files".to_owned(),
                ]);
            }
            if env.get("MUTEKI_PI_NO_BUILTIN_TOOLS").map(String::as_str) == Some("1") {
                argv.push("--no-builtin-tools".to_owned());
            }
            if let Some(system) = env.get("MUTEKI_PI_SYSTEM_PROMPT") {
                argv.extend([
                    "--no-skills".to_owned(),
                    "--no-context-files".to_owned(),
                    "--system-prompt".to_owned(),
                    system.clone(),
                ]);
            }
            for (var_name, flag) in [
                ("MUTEKI_PI_EXTRA_SKILLS", "--skill"),
                ("MUTEKI_PI_EXTRA_EXTENSIONS", "--extension"),
            ] {
                if let Some(raw) = env.get(var_name)
                    && let Ok(items) = serde_json::from_str::<Vec<String>>(raw)
                {
                    for item in items {
                        let trimmed = item.trim();
                        if !trimmed.is_empty() {
                            argv.extend([flag.to_owned(), trimmed.to_owned()]);
                        }
                    }
                }
            }
            let provider = env
                .get("MUTEKI_PI_PROVIDER")
                .or_else(|| env.get("PI_PROVIDER"))
                .filter(|value| !value.is_empty());
            let custom_model = provider.and_then(|provider| {
                env.get("MUTEKI_PI_MODEL")
                    .or_else(|| env.get("PI_MODEL"))
                    .map(|id| {
                        let id = id.strip_prefix(&format!("{provider}/")).unwrap_or(id);
                        format!("{provider}/{id}")
                    })
            });
            if let Some(model_id) = custom_model.as_deref().or(m) {
                if let Some((prov, rest)) = model_id.split_once('/') {
                    argv.extend([
                        "--provider".to_owned(),
                        prov.to_owned(),
                        "--model".to_owned(),
                        rest.to_owned(),
                    ]);
                } else {
                    argv.extend(["--model".to_owned(), model_id.to_owned()]);
                }
            }
            Ok((argv, (!interactive).then(|| prompt.to_owned())))
        }
        "omp" => {
            let mut argv = vec![
                "/home/kali/.local/bin/omp".to_owned(),
                "-p".to_owned(),
                "--mode".to_owned(),
                "json".to_owned(),
                "--no-session".to_owned(),
            ];
            if let Some(system) = env.get("MUTEKI_OMP_SYSTEM_PROMPT") {
                argv.extend(["--system-prompt".to_owned(), system.clone()]);
            }
            let provider = env
                .get("MUTEKI_OMP_PROVIDER")
                .filter(|value| !value.is_empty());
            let selected = env
                .get("MUTEKI_OMP_MODEL")
                .map(String::as_str)
                .filter(|value| !value.is_empty())
                .or(m);
            if let Some(model_id) = selected {
                if let Some(provider) = provider {
                    argv.extend([
                        "--provider".into(),
                        provider.clone(),
                        "--model".into(),
                        model_id
                            .strip_prefix(&format!("{provider}/"))
                            .unwrap_or(model_id)
                            .into(),
                    ]);
                } else if let Some((prov, rest)) = model_id.split_once('/') {
                    argv.extend([
                        "--provider".to_owned(),
                        prov.to_owned(),
                        "--model".to_owned(),
                        rest.to_owned(),
                    ]);
                } else {
                    argv.extend(["--model".to_owned(), model_id.to_owned()]);
                }
            }
            Ok((argv, Some(prompt.to_owned())))
        }
        "grok" => {
            let mut argv = vec![
                "/home/kali/.grok/bin/grok".to_owned(),
                "--permission-mode".to_owned(),
                "bypassPermissions".to_owned(),
                "--no-subagents".to_owned(),
                "--output-format".to_owned(),
                "streaming-messages-json".to_owned(),
            ];
            if let Some(model_id) = m {
                argv.extend(["--model".to_owned(), model_id.to_owned()]);
            }
            argv.extend(["-p".to_owned(), prompt.to_owned()]);
            Ok((argv, None))
        }
        _ => Err(RuntimeError::unsupported(format!(
            "引擎 {eng} 不支持容器内 CLI 执行"
        ))),
    }
}

#[derive(Debug, Default)]
pub struct ParsedCliEvent {
    pub assistant_texts: Vec<String>,
    pub reported_model: Option<String>,
    pub model_identity_source: Option<&'static str>,
    pub reasoning_texts: Vec<String>,
    pub commands_started: Vec<(String, String)>,
    pub commands_completed: Vec<(String, String, String, Option<i64>)>,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cost_usd: f64,
    pub error_message: Option<String>,
}

pub fn parse_container_cli_json_line(line: &str) -> Option<ParsedCliEvent> {
    let trimmed = line.trim();
    if !trimmed.starts_with('{') {
        return None;
    }
    let obj: Value = serde_json::from_str(trimmed).ok()?;
    let mut ev = ParsedCliEvent::default();
    let ty = obj["type"]
        .as_str()
        .or_else(|| (obj["role"] == "assistant").then_some("assistant"))
        .unwrap_or("");

    match ty {
        "item.started" => {
            let item = &obj["item"];
            if item["type"].as_str() == Some("command_execution") {
                let id = item["id"].as_str().unwrap_or("cmd").to_owned();
                let cmd = item["command"].as_str().unwrap_or("").to_owned();
                if !cmd.is_empty() {
                    ev.commands_started.push((id, cmd));
                }
            }
        }
        "item.completed" => {
            let item = &obj["item"];
            match item["type"].as_str().unwrap_or("") {
                "agent_message" | "message" => {
                    if let Some(text) = item["text"].as_str()
                        && !text.trim().is_empty()
                    {
                        ev.assistant_texts.push(text.to_owned());
                    }
                }
                "reasoning" => {
                    if let Some(text) = item["text"].as_str()
                        && !text.trim().is_empty()
                    {
                        ev.reasoning_texts.push(text.to_owned());
                    }
                }
                "command_execution" => {
                    let id = item["id"].as_str().unwrap_or("cmd").to_owned();
                    let cmd = item["command"].as_str().unwrap_or("").to_owned();
                    let out = item["aggregated_output"]
                        .as_str()
                        .or_else(|| item["output"].as_str())
                        .unwrap_or("")
                        .to_owned();
                    let exit = item["exit_code"].as_i64();
                    ev.commands_completed.push((id, cmd, out, exit));
                }
                _ => {}
            }
        }
        "turn.completed" => {
            let usage = &obj["usage"];
            ev.input_tokens = usage["input_tokens"].as_u64().unwrap_or(0);
            ev.output_tokens = usage["output_tokens"].as_u64().unwrap_or(0);
        }
        "turn.failed" | "error" => {
            let msg = obj["error"]["message"]
                .as_str()
                .or_else(|| obj["message"].as_str())
                .unwrap_or("CLI error");
            ev.error_message = Some(msg.to_owned());
        }
        "assistant" | "message" | "message_end" => {
            let msg = obj.get("message").unwrap_or(&obj);
            if msg["role"].as_str().is_some_and(|role| role != "assistant") {
                return Some(ev);
            }
            if let Some(model) = msg["responseModel"]
                .as_str()
                .filter(|model| !model.is_empty())
            {
                ev.reported_model = Some(model.to_owned());
                ev.model_identity_source = Some("provider_response");
            } else if let Some(model) = msg["model"].as_str().filter(|model| !model.is_empty()) {
                ev.reported_model = Some(model.to_owned());
                ev.model_identity_source = Some(if ty == "message_end" {
                    "cli_message"
                } else {
                    "provider_response"
                });
            }
            if msg["stopReason"].as_str() == Some("error") {
                ev.error_message = msg["errorMessage"]
                    .as_str()
                    .or_else(|| msg["error"].as_str())
                    .map(str::to_owned);
            }
            if let Some(content) = msg["content"].as_array() {
                for block in content {
                    match block["type"].as_str().unwrap_or("") {
                        "text" => {
                            if let Some(t) = block["text"].as_str()
                                && !t.trim().is_empty()
                            {
                                ev.assistant_texts.push(t.to_owned());
                            }
                        }
                        "thinking" => {
                            if let Some(t) = block["thinking"].as_str()
                                && !t.trim().is_empty()
                            {
                                ev.reasoning_texts.push(t.to_owned());
                            }
                        }
                        "tool_use" => {
                            let id = block["id"].as_str().unwrap_or("tool").to_owned();
                            let name = block["name"].as_str().unwrap_or("tool");
                            let cmd = block["input"]["command"]
                                .as_str()
                                .map(str::to_owned)
                                .unwrap_or_else(|| format!("{name} {}", block["input"]));
                            ev.commands_started.push((id, cmd));
                        }
                        _ => {}
                    }
                }
            } else if let Some(t) = msg["content"].as_str().or_else(|| obj["text"].as_str())
                && !t.trim().is_empty()
            {
                ev.assistant_texts.push(t.to_owned());
            }
            let usage = &msg["usage"];
            if usage.is_object() {
                ev.input_tokens = usage["input_tokens"]
                    .as_u64()
                    .or_else(|| usage["input"].as_u64())
                    .unwrap_or(0);
                ev.output_tokens = usage["output_tokens"]
                    .as_u64()
                    .or_else(|| usage["output"].as_u64())
                    .unwrap_or(0);
            }
        }
        "tool_execution_start" => {
            let id = obj["toolCallId"].as_str().unwrap_or("tool").to_owned();
            let name = obj["toolName"].as_str().unwrap_or("tool");
            let cmd = obj["args"]["command"]
                .as_str()
                .map(str::to_owned)
                .unwrap_or_else(|| format!("{name} {}", obj["args"]));
            ev.commands_started.push((id, cmd));
        }
        "tool_execution_end" => {
            let id = obj["toolCallId"].as_str().unwrap_or("tool").to_owned();
            let name = obj["toolName"].as_str().unwrap_or("tool").to_owned();
            let result = &obj["result"];
            let output = result["content"]
                .as_array()
                .map(|parts| {
                    parts
                        .iter()
                        .filter_map(|part| part["text"].as_str())
                        .collect::<Vec<_>>()
                        .join("\n")
                })
                .or_else(|| result["text"].as_str().map(str::to_owned))
                .or_else(|| result.as_str().map(str::to_owned))
                .unwrap_or_default();
            ev.commands_completed.push((
                id,
                name,
                output,
                Some(i64::from(obj["isError"].as_bool().unwrap_or(false))),
            ));
        }
        "result" => {
            if let Some(res_text) = obj["result"].as_str()
                && !res_text.trim().is_empty()
            {
                ev.assistant_texts.push(res_text.to_owned());
            }
            if let Some(cost) = obj["total_cost_usd"]
                .as_f64()
                .or_else(|| obj["cost_usd"].as_f64())
            {
                ev.cost_usd = cost;
            }
            let usage = &obj["usage"];
            if usage.is_object() {
                ev.input_tokens = usage["input_tokens"].as_u64().unwrap_or(0);
                ev.output_tokens = usage["output_tokens"].as_u64().unwrap_or(0);
            }
            if obj["is_error"].as_bool() == Some(true) {
                ev.error_message = obj["result"]
                    .as_str()
                    .or_else(|| obj["error"].as_str())
                    .map(str::to_owned);
            }
        }
        "text" => {
            if let Some(t) = obj["part"]["text"]
                .as_str()
                .or_else(|| obj["text"].as_str())
                && !t.trim().is_empty()
            {
                ev.assistant_texts.push(t.to_owned());
            }
        }
        _ => {}
    }

    Some(ev)
}

pub(super) fn cli_prompt_result(
    requested: Option<&str>,
    reported: Option<&str>,
    identity_source: Option<&str>,
    reply: &str,
) -> Result<Value, RuntimeError> {
    if let (Some(expected), Some(actual)) = (requested.filter(|model| !model.is_empty()), reported)
    {
        let matches = expected == actual
            || actual
                .split_once('/')
                .is_some_and(|(_, model)| model == expected)
            || expected
                .split_once('/')
                .is_some_and(|(_, model)| model == actual)
            || expected.strip_suffix(":cloud") == Some(actual);
        if !matches {
            return Err(RuntimeError::new(
                "model.fallback",
                format!("请求 {expected}，CLI 报告由 {actual} 应答"),
            ));
        }
    }
    Ok(json!({
        "reply": reply.trim(),
        "model": reported.or(requested).unwrap_or(""),
        "requested_model": requested,
        "model_identity_source": identity_source.unwrap_or("requested_only"),
    }))
}

/// Probes and catalog commands run Worker processes too, so they wait in the same node queue.
async fn admit_probe(
    state: &AppState,
    container: &str,
    tag: &str,
    settings: &Value,
    wait: Duration,
) -> Result<crate::runner_resources::Reservation, RuntimeError> {
    let node = crate::runner_remote::controller::selected(settings)
        .unwrap_or("local")
        .to_owned();
    crate::runner_resources::admit_wait(state, container, tag, &node, wait, || {})
        .await
        .map_err(|error| RuntimeError::new("runtime.admission_timeout", error))
}

pub async fn run_container_model_probe(
    state: &AppState,
    engine: &str,
    credential_id: &str,
    model: Option<&str>,
    reasoning_effort: Option<&str>,
    settings: &Value,
) -> Result<Value, RuntimeError> {
    run_container_model_prompt(
        state,
        engine,
        credential_id,
        model,
        reasoning_effort,
        settings,
        "Reply with the single word OK and nothing else.",
        45,
        None,
    )
    .await
}

/// Read the Pi model inventory through its local RPC process in the same
/// writable account projection used for a container Worker. This does not
/// submit a model turn.
pub async fn run_container_pi_catalog(
    state: &AppState,
    credential_id: &str,
    settings: &Value,
    endpoint_models: Option<&[Value]>,
) -> Result<Vec<Value>, RuntimeError> {
    let probe_id = format!("catalog-{}", &uuid::Uuid::new_v4().to_string()[..8]);
    let workspace = native_chat::node_state_dir(state)
        .join("runtime")
        .join("probes")
        .join(&probe_id);
    std::fs::create_dir_all(&workspace)
        .map_err(|e| RuntimeError::new("runtime.io_error", e.to_string()))?;
    let script = workspace.join("pi-model-catalog.mjs");
    std::fs::write(
        &script,
        include_str!("../../../../docker/worker-common/pi_model_catalog.mjs"),
    )
    .map_err(|e| RuntimeError::new("runtime.io_error", e.to_string()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o644))
            .map_err(|e| RuntimeError::new("runtime.io_error", e.to_string()))?;
    }
    let handle = match ensure_run_container(state, &probe_id, &workspace, settings).await {
        Ok(handle) => handle,
        Err(error) => {
            let _ = std::fs::remove_dir_all(&workspace);
            return Err(RuntimeError::new("runtime.container_failed", error));
        }
    };
    let result = async {
        let selected = endpoint_models.and_then(|models| models.first())
            .and_then(|model| model["id"].as_str());
        let (env, host_home) = prepare_container_worker_env(
            state, &handle, "catalog-1", "pi", credential_id, selected, &[],
        ).await?;
        if let Some(models) = endpoint_models {
            let config_path = host_home.join(".pi/agent/models.json");
            let mut config: Value = serde_json::from_slice(&std::fs::read(&config_path)
                .map_err(|error| RuntimeError::new("model_catalog.configuration_failed", error.to_string()))?)
                .map_err(|error| RuntimeError::new("model_catalog.configuration_failed", error.to_string()))?;
            config["providers"]["muteki"]["models"] = json!(models);
            write_private_file(&native_chat::node_state_dir(state), &config_path, &serde_json::to_vec_pretty(&config)
                .map_err(|error| RuntimeError::new("model_catalog.configuration_failed", error.to_string()))?)
                .map_err(|error| RuntimeError::new("model_catalog.configuration_failed", error.to_string()))?;
            chown_tree_to_worker(&handle.host_workspace, &config_path, handle.workspace_uid, handle.workspace_gid);
        }
        let reservation = admit_probe(state, &probe_id, "catalog-1", settings, Duration::from_secs(60)).await?;
        let spec = WorkerSpec {
        reservation_id: Some(reservation.id().to_owned()),
            owner_run_id: None,
            owner_token: None,
            owner_workspace: None,
            argv: vec!["node".to_owned(), format!("{CONTAINER_WORKSPACE}/pi-model-catalog.mjs"), "pi".to_owned()],
            cwd: CONTAINER_WORKSPACE.to_owned(),
            stdin: None,
            stdin_mode: None,
            env,
            timeout_sec: 45,
            tag: Some("catalog-1".to_owned()),
            output_limit_bytes: handle.limits.output_limit_bytes,
            disk_limit_bytes: handle.limits.disk_limit_bytes,
        };
        let (_wid, mut rx) = handle.link.start_worker(&spec).await
            .map_err(|e| RuntimeError::new("model_catalog.start_failed", e))?;
        let mut stdout = String::new();
        let mut stderr = String::new();
        let mut exit_rc = None;
        let deadline = Instant::now() + Duration::from_secs(55);
        while Instant::now() < deadline {
            let Ok(Some(frame)) = tokio::time::timeout(Duration::from_secs(5), rx.recv()).await else { continue };
            match frame.t.as_str() {
                "transport_error"=>return Err(RuntimeError::new("runtime.execution_unknown",frame.error)),
                "out" => { stdout.push_str(&frame.line); stdout.push('\n'); }
                "err" => { stderr.push_str(&frame.line); stderr.push('\n'); }
                "exit" => { exit_rc = Some(frame.rc); break; }
                _ => {}
            }
        }
        match exit_rc {
            None => return Err(RuntimeError::new("model_catalog.timeout", "Pi 容器模型目录超时")),
            Some(rc) if rc != 0 => return Err(RuntimeError::new("model_catalog.command_failed", format!("Pi 模型目录退出 {rc}: {}", stderr.trim()))),
            _ => {}
        }
        let payload: Value = serde_json::from_str(stdout.trim())
            .map_err(|_| RuntimeError::new("model_catalog.invalid_response", "Pi RPC 返回的模型目录不是 JSON"))?;
        let rows = payload["models"].as_array()
            .ok_or_else(|| RuntimeError::new("model_catalog.invalid_response", "Pi RPC 模型目录缺少 models"))?;
        let mut models: Vec<Value> = rows.iter().filter_map(|row| {
            let id = row["id"].as_str()?.trim();
            if id.is_empty() { return None; }
            let label = row["label"].as_str().unwrap_or(id);
            Some(json!({"id":id,"label":label,"provider":row["provider"],
                "input":row["input"],"contextWindow":row["contextWindow"],"maxTokens":row["maxTokens"],
                "reasoning":crate::agent_runtime::catalog::reasoning(&row["levels"], &row["default"], "pi_rpc")}))
        }).collect();
        if let Some(expected) = endpoint_models {
            models.retain(|model| model["provider"] == "muteki");
            if expected.iter().any(|row| !models.iter().any(|model| model["id"] == row["id"])) {
                return Err(RuntimeError::new("model_catalog.incomplete", "Pi RPC 返回的端点模型目录不完整"));
            }
        }
        if models.is_empty() {
            return Err(RuntimeError::new("model_catalog.empty", "Pi RPC 没有返回可用模型"));
        }
        Ok(models)
    }.await;
    teardown_run_container(&probe_id).await;
    let _ = std::fs::remove_dir_all(&workspace);
    result
}

/// Run a descriptor's pure CLI catalog command in the Worker image.
/// The command receives the same private account projection as a Worker but
/// never submits a model prompt.
async fn container_catalog_stdout(
    state: &AppState,
    settings: &Value,
    handle: &ContainerHandle,
    env: HashMap<String, String>,
    argv: Vec<String>,
    stdin: Option<String>,
    engine: &str,
    tag: &str,
) -> Result<String, RuntimeError> {
    let reservation = admit_probe(
        state,
        &handle.run_id,
        tag,
        settings,
        Duration::from_secs(60),
    )
    .await?;
    let spec = WorkerSpec {
        reservation_id: Some(reservation.id().to_owned()),
        owner_run_id: None,
        owner_token: None,
        owner_workspace: None,
        argv,
        cwd: CONTAINER_WORKSPACE.to_owned(),
        stdin,
        stdin_mode: None,
        env,
        timeout_sec: 60,
        tag: Some(tag.to_owned()),
        output_limit_bytes: handle.limits.output_limit_bytes,
        disk_limit_bytes: handle.limits.disk_limit_bytes,
    };
    let (_worker_id, mut rx) = handle
        .link
        .start_worker(&spec)
        .await
        .map_err(|e| RuntimeError::new("model_catalog.start_failed", e))?;
    let mut stdout = String::new();
    let mut exit_rc = None;
    let deadline = Instant::now() + Duration::from_secs(70);
    while Instant::now() < deadline {
        let Ok(Some(frame)) = tokio::time::timeout(Duration::from_secs(5), rx.recv()).await else {
            continue;
        };
        match frame.t.as_str() {
            "transport_error" => {
                return Err(RuntimeError::new("runtime.execution_unknown", frame.error));
            }
            "out" => {
                stdout.push_str(&frame.line);
                stdout.push('\n');
            }
            "exit" => {
                exit_rc = Some(frame.rc);
                break;
            }
            _ => {}
        }
    }
    match exit_rc {
        None => Err(RuntimeError::new(
            "model_catalog.timeout",
            format!("{engine} 容器模型目录超时"),
        )),
        Some(rc) if rc != 0 => Err(RuntimeError::new(
            "model_catalog.command_failed",
            format!("{engine} 模型目录命令退出 {rc}"),
        )),
        _ => Ok(stdout),
    }
}

/// Codex's bundled catalog is local data in the CLI. Run it without a network
/// or account projection so this refresh cannot use a host login or send a
/// model turn. The source descriptor defines this exact fallback argv.
pub async fn run_container_codex_bundled_catalog() -> Result<Vec<Value>, RuntimeError> {
    if let Some(socket) = crate::runner::configured_socket() {
        let value = crate::runner::request(&socket, "/v1/catalog/codex-bundled")
            .await
            .map_err(|e| RuntimeError::new("runner.catalog_failed", e))?;
        return serde_json::from_value(value)
            .map_err(|e| RuntimeError::new("runner.catalog_invalid", e.to_string()));
    }
    let descriptor = descriptors::find("codex")
        .ok_or_else(|| RuntimeError::new("engine.unknown", "未知引擎：codex"))?;
    let model_spec = &descriptor.public()["models"];
    let parser = model_spec["parser"].as_str().unwrap_or("");
    let argv: Vec<&str> = model_spec["fallback_argv"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();
    if argv.is_empty() {
        return Err(RuntimeError::unsupported("Codex 没有 CLI 内置模型目录命令"));
    }
    let image = resolve_worker_image();
    let name = format!(
        "muteki-codex-catalog-{}",
        &uuid::Uuid::new_v4().to_string()[..8]
    );
    let mut command = crate::host_process::command("docker");
    command.args([
        "run",
        "--rm",
        "--pull=never",
        "--network",
        "none",
        "--user",
        "0:0",
        "--name",
        &name,
        "--entrypoint",
        "codex",
        "-e",
        "CODEX_HOME=/tmp/muteki-codex-empty",
        "-e",
        "HOME=/tmp",
        "-e",
        "NO_COLOR=1",
        &image,
    ]);
    command
        .args(&argv)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    let output = tokio::time::timeout(Duration::from_secs(70), command.output()).await;
    let output = match output {
        Ok(Ok(output)) => output,
        Ok(Err(error)) => {
            return Err(RuntimeError::new(
                "model_catalog.start_failed",
                error.to_string(),
            ));
        }
        Err(_) => {
            let _ = crate::host_process::command("docker")
                .args(["rm", "-f", &name])
                .output()
                .await;
            return Err(RuntimeError::new(
                "model_catalog.timeout",
                "Codex 内置模型目录超时",
            ));
        }
    };
    if !output.status.success() {
        return Err(RuntimeError::new(
            "model_catalog.command_failed",
            format!("Codex 内置模型目录命令退出 {}", output.status),
        ));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let models = crate::agent_runtime::catalog::parse_cli_output(
        parser,
        &stdout,
        "codex debug models --bundled",
    )?;
    if models.is_empty() {
        return Err(RuntimeError::new(
            "model_catalog.empty",
            "Codex 内置目录没有返回模型",
        ));
    }
    Ok(models)
}

pub async fn run_container_cli_catalog(
    state: &AppState,
    engine: &str,
    credential_id: &str,
    settings: &Value,
) -> Result<Vec<Value>, RuntimeError> {
    if !matches!(
        engine,
        "kimi" | "omp" | "droid" | "grok" | "opencode" | "cursor"
    ) {
        return Err(RuntimeError::unsupported(format!(
            "{engine} 没有已接入的容器 CLI 模型目录"
        )));
    }
    let descriptor = descriptors::find(engine)
        .ok_or_else(|| RuntimeError::new("engine.unknown", format!("未知引擎：{engine}")))?;
    let parser = descriptor.public()["models"]["parser"]
        .as_str()
        .unwrap_or("");
    let stdin = crate::agent_runtime::catalog::catalog_stdin(&descriptor.public()["models"]);
    let mut argv = vec![crate::native_credentials::container_binary(engine).to_owned()];
    argv.extend(
        descriptor.public()["models"]["argv"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|v| v.as_str().map(str::to_owned)),
    );
    let probe_id = format!("catalog-{}", &uuid::Uuid::new_v4().to_string()[..8]);
    let workspace = native_chat::node_state_dir(state)
        .join("runtime")
        .join("probes")
        .join(&probe_id);
    std::fs::create_dir_all(&workspace)
        .map_err(|e| RuntimeError::new("runtime.io_error", e.to_string()))?;
    let handle = match ensure_run_container(state, &probe_id, &workspace, settings).await {
        Ok(handle) => handle,
        Err(error) => {
            let _ = std::fs::remove_dir_all(&workspace);
            return Err(RuntimeError::new("runtime.container_failed", error));
        }
    };
    let result = async {
        let (env, _) = prepare_container_worker_env(
            state,
            &handle,
            "catalog-1",
            engine,
            credential_id,
            None,
            &[("NO_COLOR".to_owned(), "1".to_owned())],
        )
        .await?;
        let first = container_catalog_stdout(
            state,
            settings,
            &handle,
            env.clone(),
            argv.clone(),
            stdin.clone(),
            engine,
            "catalog-1",
        )
        .await;
        let mut models = match first {
            Ok(stdout) => crate::agent_runtime::catalog::parse_cli_output(
                parser,
                stdout.trim(),
                &argv.join(" "),
            )?,
            Err(error) if engine == "opencode" && error.code == "model_catalog.command_failed" => {
                Vec::new()
            }
            Err(error) => return Err(error),
        };
        if engine == "opencode" && models.is_empty() {
            let fallback = vec![argv[0].clone(), "models".to_owned()];
            let stdout = container_catalog_stdout(
                state,
                settings,
                &handle,
                env,
                fallback.clone(),
                None,
                engine,
                "catalog-2",
            )
            .await?;
            models = crate::agent_runtime::catalog::parse_cli_output(
                parser,
                stdout.trim(),
                &fallback.join(" "),
            )?;
        }
        if models.is_empty() {
            return Err(RuntimeError::new(
                "model_catalog.empty",
                format!("{engine} CLI 没有返回可用模型"),
            ));
        }
        Ok(models)
    }
    .await;
    teardown_run_container(&probe_id).await;
    let _ = std::fs::remove_dir_all(&workspace);
    result
}

pub async fn run_container_model_prompt(
    state: &AppState,
    engine: &str,
    credential_id: &str,
    model: Option<&str>,
    reasoning_effort: Option<&str>,
    settings: &Value,
    prompt: &str,
    timeout_sec: u64,
    owner_run_id: Option<&str>,
) -> Result<Value, RuntimeError> {
    let timeout_sec = timeout_sec.max(1);
    let probe_id = format!("probe-{}", &uuid::Uuid::new_v4().to_string()[..8]);
    let state_dir = native_chat::node_state_dir(state);
    let workspace = state_dir.join("runtime").join("probes").join(&probe_id);
    std::fs::create_dir_all(&workspace)
        .map_err(|e| RuntimeError::new("runtime.io_error", e.to_string()))?;

    let mut probe_settings = settings.clone();
    if crate::runner_remote::controller::selected(&probe_settings).is_some()
        && probe_settings
            .get("_runner_material_key")
            .and_then(Value::as_str)
            .unwrap_or("")
            .is_empty()
    {
        probe_settings["_runner_material_key"] = json!(
            crate::runner_remote::controller::material_key(engine, credential_id, model)
        );
    }

    let handle =
        ensure_run_container_owned(state, &probe_id, &workspace, &probe_settings, owner_run_id)
            .await
            .map_err(|e| RuntimeError::new("runtime.container_failed", e))?;
    if let Some(owner_run_id) = owner_run_id {
        register_run_container_owner(owner_run_id, &probe_id).await;
    }
    struct ProbeCleanup {
        id: String,
        workspace: PathBuf,
        armed: bool,
    }
    impl Drop for ProbeCleanup {
        fn drop(&mut self) {
            if !self.armed {
                return;
            }
            let id = self.id.clone();
            let workspace = self.workspace.clone();
            if let Ok(runtime) = tokio::runtime::Handle::try_current() {
                runtime.spawn(async move {
                    if teardown_run_container_checked(&id).await.is_ok() {
                        let _ = std::fs::remove_dir_all(workspace);
                    }
                });
            }
        }
    }
    let mut cleanup = ProbeCleanup {
        id: probe_id.clone(),
        workspace: workspace.clone(),
        armed: true,
    };

    let res = async {
        let (env_map, _) = prepare_container_worker_env(
            state,
            &handle,
            "probe-1",
            engine,
            credential_id,
            model,
            &[],
        )
        .await?;
        let (mut argv, stdin) = build_container_cli_invocation(
            engine,
            model,
            reasoning_effort,
            prompt,
            &env_map,
            false,
        )?;
        if engine == "pi" {
            argv.push("--no-tools".to_owned());
        }
        let reservation = admit_probe(
            state,
            &probe_id,
            "probe-1",
            &probe_settings,
            Duration::from_secs(timeout_sec),
        )
        .await?;
        let spec = WorkerSpec {
            reservation_id: Some(reservation.id().to_owned()),
            owner_run_id: None,
            owner_token: None,
            owner_workspace: None,
            argv,
            cwd: CONTAINER_WORKSPACE.to_owned(),
            stdin,
            stdin_mode: None,
            env: env_map,
            timeout_sec: timeout_sec as i64,
            tag: Some("probe-1".to_owned()),
            output_limit_bytes: handle.limits.output_limit_bytes,
            disk_limit_bytes: handle.limits.disk_limit_bytes,
        };
        let (_wid, mut rx) = handle
            .link
            .start_worker(&spec)
            .await
            .map_err(|e| RuntimeError::new("runtime.container_failed", e))?;
        let mut reply_text = String::new();
        let mut reported_model = None;
        let mut model_identity_source = None;
        let mut model_error = None;
        let mut stderr_lines = Vec::new();
        let mut exit_rc = 0;
        let mut exited = false;
        let deadline = Instant::now() + Duration::from_secs(timeout_sec + 10);
        while Instant::now() < deadline {
            let Ok(Some(frame)) = tokio::time::timeout(Duration::from_secs(5), rx.recv()).await
            else {
                continue;
            };
            match frame.t.as_str() {
                "transport_error" => {
                    return Err(RuntimeError::new("runtime.execution_unknown", frame.error));
                }
                "out" => {
                    if let Some(ev) = parse_container_cli_json_line(&frame.line) {
                        if let Some(err) = ev.error_message {
                            model_error = Some(err);
                        }
                        if ev.reported_model.is_some() {
                            reported_model = ev.reported_model;
                            model_identity_source = ev.model_identity_source;
                        }
                        for t in ev.assistant_texts {
                            reply_text.push_str(&t);
                        }
                    }
                }
                "err" => {
                    if !frame.line.trim().is_empty() {
                        stderr_lines.push(frame.line.trim().to_owned());
                    }
                }
                "exit" => {
                    exit_rc = frame.rc;
                    exited = true;
                    break;
                }
                _ => {}
            }
        }
        if !exited {
            return Err(RuntimeError::new(
                "runtime.probe_timeout",
                "容器模型请求超时",
            ));
        }
        if exit_rc != 0 || model_error.is_some() {
            let err_tail = if stderr_lines.is_empty() {
                String::new()
            } else {
                stderr_lines
                    .iter()
                    .rev()
                    .take(4)
                    .cloned()
                    .collect::<Vec<_>>()
                    .into_iter()
                    .rev()
                    .collect::<Vec<_>>()
                    .join(" | ")
            };
            let detail = match (model_error, err_tail.is_empty()) {
                (Some(err), false) => format!("{err} ({err_tail})"),
                (Some(err), true) => err,
                (None, false) => err_tail,
                (None, true) => format!("container worker exited with rc={exit_rc}"),
            };
            return Err(RuntimeError::new("runtime.probe_failed", detail));
        }
        if reply_text.trim().is_empty() {
            return Err(RuntimeError::new(
                "runtime.probe_empty",
                stderr_lines
                    .last()
                    .cloned()
                    .unwrap_or_else(|| "容器模型没有返回文本".to_owned()),
            ));
        }
        let mut result = cli_prompt_result(
            model,
            reported_model.as_deref(),
            model_identity_source,
            &reply_text,
        )?;
        result["container"] = json!(handle.name);
        Ok(result)
    }
    .await;

    if let Err(error) = teardown_run_container_checked(&probe_id).await {
        let detail = match &res {
            Ok(_) => format!("Model response received; container cleanup is unconfirmed: {error}"),
            Err(cause) => format!(
                "{}; container cleanup is unconfirmed: {error}",
                cause.message
            ),
        };
        return Err(RuntimeError::new(
            "runtime.container_cleanup_failed",
            detail,
        ));
    }
    cleanup.armed = false;
    let _ = std::fs::remove_dir_all(&workspace);
    res
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn lifecycle_lock_serializes_one_container_without_blocking_others() {
        let id = uuid::Uuid::new_v4().to_string();
        let first = container_lifecycle_lock(&id);
        let held = first.lock().await;
        let same = container_lifecycle_lock(&id);
        assert!(same.try_lock().is_err());
        let other = container_lifecycle_lock(&format!("{id}-other"));
        assert!(other.try_lock().is_ok());
        drop(held);
        assert!(same.try_lock().is_ok());
    }

    #[tokio::test]
    #[ignore = "requires standalone Runner and MUTEKI_TEST_RUNNER_STATE_DIR"]
    async fn web_handle_executes_and_streams_through_runner() {
        let root = PathBuf::from(std::env::var("MUTEKI_TEST_RUNNER_STATE_DIR").unwrap())
            .canonicalize()
            .unwrap();
        let store = crate::store::Store::open(root.join("muteki.sqlite")).unwrap();
        store.put("system", "state_dir", &json!(root)).unwrap();
        let state = AppState::new(store);
        state.store.put("credential_accounts","projection-test",&json!({"id":"projection-test","engine":"claude","secret":"synthetic-projection-only","base_url":"https://example.invalid"})).unwrap();
        for scope in ["run", "shared"] {
            let id = format!("webproxy-{}", uuid::Uuid::new_v4().simple());
            let workspace = crate::runs::session_dir(&state, &id);
            std::fs::create_dir_all(&workspace).unwrap();
            let settings = json!({"worker_container_scope":scope,"worker_network":"bridge","worker_privilege":"default"});
            let handle = ensure_run_container_owned(&state, &id, &workspace, &settings, Some(&id))
                .await
                .unwrap();
            assert!(handle.link.runner_socket.is_some());
            assert!(handle.link.writer.lock().await.is_none());
            assert!(handle.link.health().await.unwrap().ok);
            let reservation =
                crate::runner_resources::Reservation::enqueue(&state, &id, "proxy-solver").unwrap();
            assert!(
                reservation
                    .try_admit(
                        &state,
                        &crate::runner_resources::recommend(4, 6 * 1024 * 1024 * 1024, "test")
                    )
                    .unwrap()
            );
            let (projected_env, home) = prepare_container_worker_env(
                &state,
                &handle,
                "proxy-solver",
                "claude",
                "account:projection-test",
                None,
                &[],
            )
            .await
            .unwrap();
            assert!(home.join(".claude.json").is_file());
            let spec=WorkerSpec {reservation_id:Some(reservation.id().to_owned()),owner_run_id:None,owner_token:None,owner_workspace:None,
                argv:vec!["/bin/sh".into(),"-c".into(),"test \"$(cat \"$ANTHROPIC_API_KEY_FILE\")\" = \"$ANTHROPIC_API_KEY\" || exit 8; read line; printf 'before-gap\\n'; sleep 2; printf '%s' \"$line\"; printf persisted > proxy-result.txt".into()],
                cwd:handle.container_workspace.clone(),stdin:None,stdin_mode:Some("stream".into()),env:projected_env,
                timeout_sec:20,tag:Some("proxy-solver".into()),output_limit_bytes:1048576,disk_limit_bytes:10485760};
            let (worker, mut frames) = handle.link.start_worker(&spec).await.unwrap();
            assert!(
                handle
                    .link
                    .write_stdin(&worker, "runner-proxy-ok\n")
                    .await
                    .unwrap()
            );
            let prefix = tokio::time::timeout(Duration::from_secs(5), frames.recv())
                .await
                .unwrap()
                .unwrap();
            assert_eq!(prefix.t, "out");
            assert_eq!(prefix.line, "before-gap");
            let socket = crate::runner::configured_socket().unwrap();
            let disconnected = socket.with_extension("disconnected");
            std::fs::rename(&socket, &disconnected).unwrap();
            tokio::time::sleep(Duration::from_millis(1500)).await;
            std::fs::rename(&disconnected, &socket).unwrap();
            let mut output = String::new();
            tokio::time::timeout(Duration::from_secs(30), async {
                loop {
                    let frame = frames.recv().await.expect("missing exit receipt");
                    if frame.t == "out" {
                        output.push_str(&frame.line);
                    }
                    if frame.t == "exit" {
                        assert_eq!(frame.rc, 0);
                        break;
                    }
                }
            })
            .await
            .unwrap();
            assert_eq!(output.matches("runner-proxy-ok").count(), 1);
            assert!(!output.contains("before-gap"));
            assert!(handle.link.is_alive());
            assert_eq!(
                std::fs::read_to_string(workspace.join("proxy-result.txt")).unwrap(),
                "persisted"
            );
            let (next, mut checkpoint) = handle.link.start_worker(&spec).await.unwrap();
            assert_ne!(worker, next);
            assert!(
                handle
                    .link
                    .write_stdin(&next, "checkpoint-ok\n")
                    .await
                    .unwrap()
            );
            tokio::time::timeout(Duration::from_secs(30), async {
                loop {
                    let frame = checkpoint.recv().await.unwrap();
                    if frame.t == "exit" {
                        assert_eq!(frame.rc, 0);
                        break;
                    }
                }
            })
            .await
            .unwrap();
            drop(reservation);
            let replacement =
                crate::runner_resources::Reservation::enqueue(&state, &id, "proxy-solver").unwrap();
            assert!(
                replacement
                    .try_admit(
                        &state,
                        &crate::runner_resources::recommend(4, 6 * 1024 * 1024 * 1024, "test")
                    )
                    .unwrap()
            );
            assert!(
                handle
                    .link
                    .start_worker(&spec)
                    .await
                    .err()
                    .unwrap()
                    .contains("does not authorize")
            );
            let mut current = spec.clone();
            current.reservation_id = Some(replacement.id().to_owned());
            current.argv = vec!["/bin/true".into()];
            current.stdin_mode = None;
            let (_, mut final_frames) = handle.link.start_worker(&current).await.unwrap();
            tokio::time::timeout(Duration::from_secs(20), async {
                loop {
                    let frame = final_frames.recv().await.unwrap();
                    if frame.t == "exit" {
                        assert_eq!(frame.rc, 0);
                        break;
                    }
                }
            })
            .await
            .unwrap();
            drop(replacement);
            assert!(
                state
                    .store
                    .list("runner_dispatches")
                    .unwrap()
                    .iter()
                    .filter(|r| r["run_id"] == id)
                    .all(|r| r["status"] == "released")
            );
            #[cfg(unix)]
            {
                let outside = tempfile::tempdir().unwrap();
                let sentinel = outside.path().join("untouched");
                std::fs::write(&sentinel, b"original").unwrap();
                let escape = handle.host_workspace.join("homes/escape");
                std::os::unix::fs::symlink(outside.path(), &escape).unwrap();
                assert!(
                    prepare_container_worker_env(
                        &state,
                        &handle,
                        "escape",
                        "claude",
                        "account:projection-test",
                        None,
                        &[]
                    )
                    .await
                    .is_err()
                );
                let key = handle
                    .host_accounts_dir
                    .as_ref()
                    .unwrap()
                    .join("projection-test/ANTHROPIC_API_KEY");
                std::fs::remove_file(&key).unwrap();
                std::os::unix::fs::symlink(&sentinel, &key).unwrap();
                assert!(
                    prepare_container_worker_env(
                        &state,
                        &handle,
                        "proxy-solver",
                        "claude",
                        "account:projection-test",
                        None,
                        &[]
                    )
                    .await
                    .is_err()
                );
                assert_eq!(std::fs::read(&sentinel).unwrap(), b"original");
                assert_eq!(std::fs::read_dir(outside.path()).unwrap().count(), 1);
                std::fs::remove_file(&key).unwrap();
                std::fs::remove_file(&escape).unwrap();
            }
            teardown_run_container_checked(&id).await.unwrap();
        }
    }

    #[test]
    fn network_projection_preserves_source_modes() {
        let bridge = project_worker_network("bridge");
        assert_eq!(bridge.effective.as_deref(), Some("bridge"));
        assert!(bridge.error.is_none());

        let host = project_worker_network("host");
        assert_eq!(host.effective.as_deref(), Some("host"));
        let gateway = "muteki-src-egress-0123456789abcdef01234567";
        let enforced = project_worker_network(&format!("container:{gateway}"));
        assert_eq!(
            enforced.effective.as_deref(),
            Some(format!("container:{gateway}").as_str())
        );
        let declared = project_worker_network(&format!("src-record:{gateway}"));
        assert_eq!(declared.effective.as_deref(), Some("bridge"));
    }

    #[tokio::test]
    async fn control_receiver_validates_one_shot_token_handshake() {
        let receiver = ControlReceiver::bind().await.unwrap();
        let token = receiver.expect("test-run-1");
        let port = receiver.port();

        let client = tokio::spawn(async move {
            let mut stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
            let hello = format!(
                "{{\"hello\":1,\"run_id\":\"test-run-1\",\"token\":{token:?},\"version\":\"test/1\"}}\n"
            );
            stream.write_all(hello.as_bytes()).await.unwrap();
            let mut reader = BufReader::new(stream);
            let mut ack = String::new();
            reader.read_line(&mut ack).await.unwrap();
            let v: Value = serde_json::from_str(ack.trim()).unwrap();
            assert_eq!(v["ok"], json!(true));
        });

        let link = receiver
            .wait_for("test-run-1", Duration::from_secs(5))
            .await
            .unwrap();
        assert_eq!(link.run_id, "test-run-1");
        assert_eq!(link.peer_version, "test/1");
        client.await.unwrap();
        receiver.forget("test-run-1");
    }

    #[tokio::test]
    async fn gateway_proxy_containers_preserve_recording_proxy_env() {
        let dir = tempfile::tempdir().unwrap();
        let store = crate::store::Store::open(dir.path().join("muteki.sqlite")).unwrap();
        store
            .put("system", "state_dir", &json!(dir.path()))
            .unwrap();
        store
            .put(
                "credential_accounts",
                "gw-test",
                &json!({"id":"gw-test","engine":"claude","secret":"synthetic-gw-secret"}),
            )
            .unwrap();
        let state = AppState::new(store);
        crate::network_proxy::save(
            &state,
            &json!({"mode": "custom", "url": "http://127.0.0.1:7890"}),
        )
        .unwrap();

        let workspace = dir.path().join("ws");
        std::fs::create_dir_all(&workspace).unwrap();
        let accounts = dir.path().join("accounts");
        std::fs::create_dir_all(&accounts).unwrap();
        let handle = ContainerHandle {
            name: "muteki-worker-gw-test".to_owned(),
            container_id: "cid-gw-test".to_owned(),
            run_id: "run-gw-test".to_owned(),
            image: "img-test".to_owned(),
            host_workspace: workspace.clone(),
            container_workspace: CONTAINER_WORKSPACE.to_owned(),
            host_control_dir: dir.path().join("control"),
            host_accounts_dir: Some(accounts),
            link: SupervisorLink::runner("run-gw-test".to_owned(), PathBuf::new(), 1),
            scope: "run".to_owned(),
            owner_token: String::new(),
            policy_identity: String::new(),
            network_mode: "bridge".to_owned(),
            has_gateway_proxy: true,
            limits: resolve_worker_resource_limits(None).unwrap(),
            worker_uid: 0,
            worker_gid: 0,
            workspace_uid: 1000,
            workspace_gid: 1000,
        };

        // Even with custom global proxy configured, gateway proxy container must keep its container proxy intact.
        let (env_custom, _) = prepare_container_worker_env(
            &state,
            &handle,
            "solver-1",
            "claude",
            "account:gw-test",
            None,
            &[],
        )
        .await
        .unwrap();
        for k in [
            "HTTP_PROXY",
            "HTTPS_PROXY",
            "ALL_PROXY",
            "http_proxy",
            "https_proxy",
            "all_proxy",
        ] {
            assert!(
                !env_custom.contains_key(k),
                "expected {k} not to be overwritten for gateway proxy container"
            );
        }

        // Switching to direct global mode must also leave the container gateway proxy untouched.
        crate::network_proxy::save(&state, &json!({"mode": "direct"})).unwrap();
        let (env_direct, _) = prepare_container_worker_env(
            &state,
            &handle,
            "solver-1",
            "claude",
            "account:gw-test",
            None,
            &[],
        )
        .await
        .unwrap();
        assert!(!env_direct.contains_key("HTTP_PROXY"));
        assert!(!env_direct.contains_key("HTTPS_PROXY"));
    }
}
