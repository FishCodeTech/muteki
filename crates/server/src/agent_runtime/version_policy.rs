//! Minimum supported Agent CLI version policy across CTF, Pentest, SRC modes
//! and active Worker ("出战 Worker") admission.
//!
//! Default minimum versions are anchored to the host development baseline.
//! Actual installed versions are always resolved from Ground Truth (`--version`
//! probes on local binaries or `IMAGE_CAPABILITIES.json` inside the Worker
//! container image) rather than static assumptions.

use crate::{
    AppState,
    agent_runtime::{RuntimeError, binaries, descriptors, version_check},
    runtime_api,
    store::Store,
};
use axum::{
    Json, Router,
    extract::{Query, State},
    http::StatusCode,
    routing::get,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, HashMap},
    path::Path,
    sync::{Arc, Mutex, OnceLock},
    time::{Duration, Instant},
};

pub const SETTINGS_NAMESPACE: &str = "settings";
pub const SETTINGS_KEY: &str = "agent_version_policy";

/// Stable typed error codes for minimum version admission rejections.
pub const ERR_AGENT_VERSION_BELOW_MIN: &str = "agent.version_below_minimum";
pub const ERR_WORKER_VERSION_UNSUPPORTED: &str = "worker.agent_version_unsupported";

const LOCAL_VERSION_CACHE_TTL: Duration = Duration::from_secs(60);

#[derive(Clone, Copy, Debug)]
pub struct BaselineSpec {
    pub engine: &'static str,
    pub display_name: &'static str,
    pub upgrade_command: &'static str,
    pub alt_upgrade_command: &'static str,
    pub upgrade_note: &'static str,
    pub container_upgrade_note: &'static str,
}

impl BaselineSpec {
    pub fn baseline_version(&self) -> &'static str {
        static VERSIONS: OnceLock<BTreeMap<String, String>> = OnceLock::new();
        &VERSIONS.get_or_init(|| {
            serde_json::from_str(include_str!("minimum_versions.json"))
                .expect("embedded Agent minimum versions must be valid JSON")
        })[self.engine]
    }
}

pub const BASELINE_SPECS: &[BaselineSpec] = &[
    BaselineSpec {
        engine: "codex",
        display_name: "Codex",
        upgrade_command: "npm install -g @openai/codex@latest",
        alt_upgrade_command: "",
        upgrade_note: "升级本机 Codex CLI 后在设置中点击「重新检测」即可解除限制。",
        container_upgrade_note: "容器模式下请拉取或重新构建包含新版 Codex CLI 的 muteki-worker 镜像。",
    },
    BaselineSpec {
        engine: "claude",
        display_name: "Claude Code",
        upgrade_command: "claude update",
        alt_upgrade_command: "npm install -g @anthropic-ai/claude-code@latest",
        upgrade_note: "可执行 `claude update` 或通过 npm 全局更新 `@anthropic-ai/claude-code`。",
        container_upgrade_note: "容器镜像内 Claude Code 低于基准版本时，请更新或重构建 muteki-worker 镜像，或临时切换为「本机直跑」。",
    },
    BaselineSpec {
        engine: "cursor",
        display_name: "Cursor Agent",
        upgrade_command: "curl https://cursor.com/install -fsS | bash",
        alt_upgrade_command: "",
        upgrade_note: "Cursor CLI (`cursor-agent`) 采用日期构建号（YYYY.MM.DD-sha），重新运行官方安装脚本即可升级。",
        container_upgrade_note: "容器模式下请更新 muteki-worker 镜像中的 cursor-agent 版本，或切换为「本机直跑」。",
    },
    BaselineSpec {
        engine: "grok",
        display_name: "Grok CLI",
        upgrade_command: "grok update",
        alt_upgrade_command: "",
        upgrade_note: "执行 `grok update` 升级本机 Grok CLI 至最低基准版本及以上。",
        container_upgrade_note: "容器镜像内 Grok CLI 低于基准版本时，请更新 muteki-worker 镜像或切换为「本机直跑」。",
    },
    BaselineSpec {
        engine: "opencode",
        display_name: "OpenCode",
        upgrade_command: "npm install -g @opencode/cli@latest",
        alt_upgrade_command: "",
        upgrade_note: "仅支持 OpenCode 2.x 及以上；请升级 `@opencode/cli`。",
        container_upgrade_note: "容器模式下请更新 muteki-worker 镜像中的 `@opencode/cli`。",
    },
    BaselineSpec {
        engine: "pi",
        display_name: "Pi",
        upgrade_command: "npm install -g @earendil-works/pi-coding-agent@latest",
        alt_upgrade_command: "",
        upgrade_note: "执行 npm 全局升级 `@earendil-works/pi-coding-agent` 至基准版本及以上。",
        container_upgrade_note: "容器镜像内 Pi 低于基准版本时，请更新 muteki-worker 镜像或切换为「本机直跑」。",
    },
    BaselineSpec {
        engine: "kimi",
        display_name: "Kimi Code",
        upgrade_command: "curl -fsSL https://code.kimi.ai/kimi-code/install.sh | bash",
        alt_upgrade_command: "uv tool upgrade kimi-cli",
        upgrade_note: "运行官方安装脚本或 `uv tool upgrade kimi-cli` 升级 Kimi CLI。",
        container_upgrade_note: "容器模式下请更新 muteki-worker 镜像中的 Kimi CLI。",
    },
    BaselineSpec {
        engine: "omp",
        display_name: "Oh My Pi",
        upgrade_command: "omp update",
        alt_upgrade_command: "",
        upgrade_note: "执行 `omp update` 升级 Oh My Pi CLI。",
        container_upgrade_note: "容器模式下请更新 muteki-worker 镜像中的 omp CLI。",
    },
    BaselineSpec {
        engine: "devin",
        display_name: "Devin CLI",
        upgrade_command: "devin update",
        alt_upgrade_command: "",
        upgrade_note: "执行 `devin update` 升级本机 Devin CLI。",
        container_upgrade_note: "Devin CLI 默认在本机直跑环境运行；容器镜像若未内置请切换为「本机直跑」。",
    },
    BaselineSpec {
        engine: "droid",
        display_name: "Factory Droid",
        upgrade_command: "npm install -g droid@latest",
        alt_upgrade_command: "curl -fsSL https://app.factory.ai/cli | sh",
        upgrade_note: "升级 Factory Droid CLI 至最低基准版本及以上。",
        container_upgrade_note: "容器镜像内 Droid CLI 低于基准版本时，请更新 muteki-worker 镜像或切换为「本机直跑」。",
    },
    BaselineSpec {
        engine: "agy",
        display_name: "Antigravity",
        upgrade_command: "agy update",
        alt_upgrade_command: "",
        upgrade_note: "Antigravity 用于对话与 SRC 模式主控 Agent（不作为出战 Worker）；低于基准时请执行 `agy update`。",
        container_upgrade_note: "Antigravity 仅用于宿主机对话与 SRC 模式，不进入出战 Worker 容器。",
    },
];

pub fn spec(engine: &str) -> Option<&'static BaselineSpec> {
    let norm = normalize_engine_id(engine);
    BASELINE_SPECS.iter().find(|item| item.engine == norm)
}

pub fn normalize_engine_id(raw: &str) -> String {
    let trimmed = raw.trim().to_ascii_lowercase();
    if let Some((desc, _)) = descriptors::for_adapter(&trimmed) {
        return desc.engine().to_owned();
    }
    let stripped = trimmed
        .strip_prefix("cli-")
        .or_else(|| trimmed.strip_prefix("cli."))
        .unwrap_or(&trimmed);
    if let Some(desc) = descriptors::find(stripped) {
        return desc.engine().to_owned();
    }
    if let Some((desc, _)) = descriptors::for_adapter(stripped) {
        return desc.engine().to_owned();
    }
    stripped.to_owned()
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ModeSwitches {
    #[serde(default = "default_true")]
    pub ctf: bool,
    #[serde(default = "default_true")]
    pub pentest: bool,
    #[serde(default = "default_true")]
    pub src: bool,
}

fn default_true() -> bool {
    true
}

impl Default for ModeSwitches {
    fn default() -> Self {
        Self {
            ctf: true,
            pentest: true,
            src: true,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AgentVersionPolicy {
    #[serde(default = "default_true")]
    pub enforce: bool,
    #[serde(default)]
    pub modes: ModeSwitches,
    #[serde(default)]
    pub updated_at: String,
}

impl Default for AgentVersionPolicy {
    fn default() -> Self {
        Self {
            enforce: true,
            modes: ModeSwitches::default(),
            updated_at: String::new(),
        }
    }
}

impl AgentVersionPolicy {
    pub fn min_version_for(&self, engine: &str) -> String {
        spec(&normalize_engine_id(engine))
            .map(|item| item.baseline_version().to_owned())
            .unwrap_or_default()
    }

    pub fn is_enforced_for_mode(&self, mode: Option<&str>) -> bool {
        if !self.enforce {
            return false;
        }
        match mode.map(str::trim) {
            Some("ctf") => self.modes.ctf,
            Some("pentest") => self.modes.pentest,
            Some("src") => self.modes.src,
            Some("") | None => self.modes.ctf || self.modes.pentest || self.modes.src,
            Some(_) => true,
        }
    }
}

pub fn baseline_map() -> BTreeMap<String, String> {
    BASELINE_SPECS
        .iter()
        .map(|item| (item.engine.to_owned(), item.baseline_version().to_owned()))
        .collect()
}

pub fn load_policy(store: &Store) -> AgentVersionPolicy {
    let mut policy = AgentVersionPolicy::default();
    let Some(saved) = store.get(SETTINGS_NAMESPACE, SETTINGS_KEY).ok().flatten() else {
        return policy;
    };
    if let Some(enforce) = saved["enforce"].as_bool() {
        policy.enforce = enforce;
    }
    if let Some(ctf) = saved["modes"]["ctf"].as_bool() {
        policy.modes.ctf = ctf;
    }
    if let Some(pentest) = saved["modes"]["pentest"].as_bool() {
        policy.modes.pentest = pentest;
    }
    if let Some(src) = saved["modes"]["src"].as_bool() {
        policy.modes.src = src;
    }
    if let Some(ts) = saved["updated_at"].as_str() {
        policy.updated_at = ts.to_owned();
    }
    policy
}

pub fn policy_summary_json(store: &Store) -> Value {
    let policy = load_policy(store);
    json!({
        "enforce": policy.enforce,
        "modes": policy.modes,
        "baseline_versions": baseline_map(),
        "updated_at": policy.updated_at,
    })
}

/// Enriches a runtime health snapshot (`agent_runtime_health` row) with minimum
/// version policy fields and degradation hints.
pub fn annotate_health_snapshot(store: &Store, engine: &str, health: &mut Value) {
    let Some(obj) = health.as_object_mut() else {
        return;
    };
    let norm = normalize_engine_id(engine);
    let policy = load_policy(store);
    let min_ver = policy.min_version_for(&norm);
    let baseline = spec(&norm).map(|s| s.baseline_version()).unwrap_or("");
    let upgrade_cmd = spec(&norm).map(|s| s.upgrade_command).unwrap_or("");
    let alt_upgrade_cmd = spec(&norm).map(|s| s.alt_upgrade_command).unwrap_or("");
    let upgrade_note = spec(&norm).map(|s| s.upgrade_note).unwrap_or("");
    let container_upgrade_note = spec(&norm).map(|s| s.container_upgrade_note).unwrap_or("");

    let runtime_ver = obj
        .get("runtime_version")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
    let existing_installed = obj
        .get("version_check")
        .and_then(|vc| vc.get("installed_version"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
    let installed = if !existing_installed.is_empty() {
        existing_installed
    } else {
        version_check::installed_version(&norm, &runtime_ver)
    };

    let cmp = (!installed.is_empty() && !min_ver.is_empty())
        .then(|| version_check::compare_for_engine(&norm, &installed, &min_ver))
        .flatten();
    let meets_min = cmp.is_some_and(|ord| ord >= 0);
    let below_raw = cmp == Some(-1);
    let below_enforced = policy.enforce && below_raw;

    let vc_entry = obj.entry("version_check").or_insert_with(|| json!({}));
    if !vc_entry.is_object() {
        *vc_entry = json!({});
    }
    if let Some(vc) = vc_entry.as_object_mut() {
        if vc.get("installed_version").and_then(Value::as_str).unwrap_or("").is_empty()
            && !installed.is_empty()
        {
            vc.insert("installed_version".into(), json!(installed));
        }
        vc.insert("baseline_version".into(), json!(baseline));
        vc.insert("min_supported_version".into(), json!(min_ver));
        vc.insert("meets_min_version".into(), json!(meets_min));
        vc.insert("below_minimum_raw".into(), json!(below_raw));
        vc.insert("below_minimum".into(), json!(below_enforced));
        vc.insert("policy_enforced".into(), json!(policy.enforce));
        vc.insert(
            "policy_modes".into(),
            json!({
                "ctf": policy.modes.ctf,
                "pentest": policy.modes.pentest,
                "src": policy.modes.src,
            }),
        );
        vc.insert("upgrade_command".into(), json!(upgrade_cmd));
        vc.insert("alt_upgrade_command".into(), json!(alt_upgrade_cmd));
        vc.insert("upgrade_note".into(), json!(upgrade_note));
        vc.insert("container_upgrade_note".into(), json!(container_upgrade_note));
    }

    if below_enforced {
        let warning = if !upgrade_cmd.is_empty() {
            format!(
                "当前版本 {installed} 低于最低支持版本 {min_ver}，禁止设置为出战 Worker；建议升级：`{upgrade_cmd}`"
            )
        } else {
            format!("当前版本 {installed} 低于最低支持版本 {min_ver}，禁止设置为出战 Worker；请先升级 CLI")
        };
        let degs = obj.entry("degradations").or_insert_with(|| json!([]));
        if let Some(arr) = degs.as_array_mut() {
            let already = arr
                .iter()
                .any(|item| item.as_str().is_some_and(|s| s.contains("低于最低支持版本")));
            if !already {
                arr.push(json!(warning));
            }
        }
    }
}

#[derive(Clone)]
struct LocalVersionCacheEntry {
    at: Instant,
    mtime_nanos: Option<u128>,
    size: Option<u64>,
    raw_version: String,
}

fn local_version_cache() -> &'static Mutex<HashMap<String, LocalVersionCacheEntry>> {
    static CACHE: OnceLock<Mutex<HashMap<String, LocalVersionCacheEntry>>> = OnceLock::new();
    CACHE.get_or_init(Default::default)
}

fn file_signature(path: &Path) -> (Option<u128>, Option<u64>) {
    let Ok(meta) = std::fs::metadata(path) else {
        return (None, None);
    };
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos());
    (mtime, Some(meta.len()))
}

fn cached_health_runtime_version(store: &Store, desc: &descriptors::EngineDescriptor, bin_path: &Path) -> Option<String> {
    let bin_str = bin_path.to_string_lossy();
    for adapter_id in [&desc.identity.default_adapter_id, &desc.identity.cli_adapter_id] {
        if adapter_id.is_empty() {
            continue;
        }
        let key = format!("{adapter_id}:default");
        if let Some(h) = store.get(runtime_api::HEALTH, &key).ok().flatten() {
            let h_bin = h["binary_path"].as_str().unwrap_or("");
            let rv = h["runtime_version"].as_str().unwrap_or("").trim();
            if !rv.is_empty() && (h_bin.is_empty() || h_bin == bin_str) {
                return Some(rv.to_owned());
            }
        }
    }
    None
}

/// Resolves the installed local binary version for `desc`, using cached file
/// metadata + runtime health unless `fresh` is requested.
pub async fn resolve_local_engine_version(
    state: &AppState,
    desc: &descriptors::EngineDescriptor,
    explicit_binary: Option<&Path>,
    fresh: bool,
) -> Result<(std::path::PathBuf, String, String), RuntimeError> {
    let binary = binaries::resolve(desc, explicit_binary).await?;
    let canonical = binary.path.canonicalize().unwrap_or_else(|_| binary.path.clone());
    let cache_key = canonical.to_string_lossy().into_owned();
    let (mtime_nanos, size) = file_signature(&canonical);

    if !fresh {
        if let Some(entry) = local_version_cache().lock().unwrap().get(&cache_key).cloned() {
            if entry.at.elapsed() < LOCAL_VERSION_CACHE_TTL
                && entry.mtime_nanos == mtime_nanos
                && entry.size == size
            {
                let parsed = version_check::installed_version(desc.engine(), &entry.raw_version);
                return Ok((binary.path, entry.raw_version, parsed));
            }
        }
        if explicit_binary.is_none()
            && let Some(rv) = cached_health_runtime_version(&state.store, desc, &binary.path)
        {
            let parsed = version_check::installed_version(desc.engine(), &rv);
            if !parsed.is_empty() {
                local_version_cache().lock().unwrap().insert(
                    cache_key,
                    LocalVersionCacheEntry {
                        at: Instant::now(),
                        mtime_nanos,
                        size,
                        raw_version: rv.clone(),
                    },
                );
                return Ok((binary.path, rv, parsed));
            }
        }
    }

    let raw = runtime_api::run_version(&binary.path)
        .await
        .map_err(|detail| RuntimeError::new("runtime.binary_unrunnable", detail))?;
    let parsed = version_check::installed_version(desc.engine(), &raw);
    local_version_cache().lock().unwrap().insert(
        cache_key,
        LocalVersionCacheEntry {
            at: Instant::now(),
            mtime_nanos,
            size,
            raw_version: raw.clone(),
        },
    );
    Ok((binary.path, raw, parsed))
}

fn mode_display_label(mode: Option<&str>) -> &'static str {
    match mode.map(str::trim) {
        Some("ctf") => "CTF 模式",
        Some("pentest") => "Pentest 渗透模式",
        Some("src") => "SRC 模式",
        _ => "出战 Worker",
    }
}

/// Verifies that a local CLI engine meets the configured minimum version for `mode`.
pub async fn verify_local_engine(
    state: &AppState,
    engine: &str,
    explicit_binary: Option<&Path>,
    mode: Option<&str>,
) -> Result<(), RuntimeError> {
    let norm = normalize_engine_id(engine);
    let policy = load_policy(&state.store);
    if !policy.is_enforced_for_mode(mode) {
        return Ok(());
    }
    let Some(desc) = descriptors::find(&norm) else {
        return Ok(());
    };
    let min_ver = policy.min_version_for(&norm);
    if min_ver.is_empty() {
        return Ok(());
    }
    let (_, raw_ver, installed) = resolve_local_engine_version(state, desc, explicit_binary, false).await?;
    if installed.is_empty() {
        return Ok(());
    }
    if version_check::compare_for_engine(&norm, &installed, &min_ver) == Some(-1) {
        let display = spec(&norm)
            .map(|s| s.display_name)
            .unwrap_or(desc.identity.display_name.as_str());
        let upgrade_cmd = spec(&norm).map(|s| s.upgrade_command).unwrap_or("");
        let target_label = if mode == Some("src") && desc.public()["worker"].is_null() {
            "SRC 模式主控 Agent"
        } else {
            "出战 Worker / 模式执行引擎"
        };
        let upgrade_hint = if !upgrade_cmd.is_empty() {
            format!("建议升级：请在终端执行 `{upgrade_cmd}` 升级至 {min_ver} 及以上版本后重试。")
        } else {
            format!("请将本机 {display} CLI 升级至 {min_ver} 及以上版本后重试。")
        };
        return Err(RuntimeError::new(
            ERR_AGENT_VERSION_BELOW_MIN,
            format!(
                "{}版本准入拦截：{display} 当前本机版本 {installed}（原始输出：{raw_ver}）低于最低支持版本 {min_ver}，禁止设为{target_label}。{upgrade_hint}",
                mode_display_label(mode)
            ),
        ));
    }
    Ok(())
}

/// Verifies that a Worker engine meets the configured minimum version on the
/// selected execution backend (`local` or local `container`).
pub async fn verify_worker_engine(
    state: &AppState,
    engine: &str,
    backend: &str,
    node_id: &str,
    mode: Option<&str>,
    explicit_binary: Option<&Path>,
    preloaded_img_status: Option<&Value>,
) -> Result<(), RuntimeError> {
    let norm = normalize_engine_id(engine);
    let policy = load_policy(&state.store);
    if !policy.is_enforced_for_mode(mode) {
        return Ok(());
    }
    let min_ver = policy.min_version_for(&norm);
    if min_ver.is_empty() {
        return Ok(());
    }
    let display = spec(&norm)
        .map(|s| s.display_name)
        .or_else(|| descriptors::find(&norm).map(|d| d.identity.display_name.as_str()))
        .unwrap_or(&norm);
    let upgrade_cmd = spec(&norm).map(|s| s.upgrade_command).unwrap_or("");

    if backend == "container" {
        if !node_id.is_empty() && node_id != "local" {
            return Ok(());
        }
        let owned_img;
        let img = match preloaded_img_status {
            Some(v) => v,
            None => {
                owned_img = crate::runs::container_exec::inspect_worker_image_status().await;
                &owned_img
            }
        };
        if img["pulled"]["ok"].as_bool() != Some(true) {
            return Ok(());
        }
        let cap_entry = &img["capabilities"]["engines"][&norm];
        if !cap_entry.is_object() || cap_entry["available"].as_bool() == Some(false) {
            return Ok(());
        }
        let raw_ver = cap_entry["version"].as_str().unwrap_or("").trim();
        let installed = version_check::installed_version(&norm, raw_ver);
        if !installed.is_empty()
            && version_check::compare_for_engine(&norm, &installed, &min_ver) == Some(-1)
        {
            let host_hint = if !upgrade_cmd.is_empty() {
                format!(
                    "建议更新或重新构建 muteki-worker 容器镜像（将镜像内 {display} 升级至 >= {min_ver}；本机 CLI 升级命令：`{upgrade_cmd}`），或在运行环境中切换为「本机直跑」。"
                )
            } else {
                format!(
                    "建议更新或重新构建 muteki-worker 容器镜像（将镜像内 {display} 升级至 >= {min_ver}），或切换为「本机直跑」。"
                )
            };
            return Err(RuntimeError::new(
                ERR_AGENT_VERSION_BELOW_MIN,
                format!(
                    "{}版本准入拦截：{display} 当前容器镜像内版本 {installed} 低于最低支持版本 {min_ver}，禁止设置为出战 Worker。{host_hint}",
                    mode_display_label(mode)
                ),
            ));
        }
        return Ok(());
    }

    verify_local_engine(state, &norm, explicit_binary, mode).await
}

/// Evaluates all declared Agent engines against the current minimum version
/// policy for both `local` (host CLI) and `container` (`muteki-worker` image).
pub async fn evaluate_all(state: &AppState, fresh: bool) -> Value {
    let policy = load_policy(&state.store);
    let img_status = crate::runs::container_exec::inspect_worker_image_status().await;
    let image_pulled = img_status["pulled"]["ok"].as_bool() == Some(true);
    let image_name = img_status["image"].as_str().unwrap_or("").to_owned();
    let image_declared = img_status["capabilities"]["declared"].as_bool() == Some(true);

    let mut tasks = Vec::with_capacity(BASELINE_SPECS.len());
    let state_arc = Arc::new(());
    let _ = state_arc;

    let futures: Vec<_> = BASELINE_SPECS
        .iter()
        .map(|item| async move {
            let Some(desc) = descriptors::find(item.engine) else {
                return (item.engine, Err(RuntimeError::unknown_provider(item.engine)));
            };
            let configured_bin = runtime_api::default_runtime_key(item.engine)
                .and_then(|key| state.store.get(runtime_api::INSTANCES, &key).ok().flatten())
                .and_then(|cfg| cfg["binary_path"].as_str().map(str::trim).filter(|s| !s.is_empty()).map(std::path::PathBuf::from));
            let res = resolve_local_engine_version(state, desc, configured_bin.as_deref(), fresh).await;
            (item.engine, res)
        })
        .collect();
    let local_results = futures_util::future::join_all(futures).await;
    for item in local_results {
        tasks.push(item);
    }

    let mut local_map: HashMap<&'static str, Result<(std::path::PathBuf, String, String), RuntimeError>> =
        tasks.into_iter().collect();

    let mut rows = Vec::with_capacity(BASELINE_SPECS.len());
    for item in BASELINE_SPECS {
        let desc = descriptors::find(item.engine);
        let worker_capable = desc.is_some_and(|d| !d.public()["worker"].is_null());
        let min_ver = policy.min_version_for(item.engine);

        let local_json = match local_map.remove(item.engine) {
            Some(Ok((bin_path, raw_ver, installed))) => {
                let cmp = (!installed.is_empty() && !min_ver.is_empty())
                    .then(|| version_check::compare_for_engine(item.engine, &installed, &min_ver))
                    .flatten();
                let (status, meets_min, below_min, detail) = match cmp {
                    Some(-1) => (
                        "below_minimum",
                        false,
                        true,
                        format!(
                            "本机版本 {installed} 低于最低支持版本 {min_ver}，禁止设为出战 Worker；建议升级：`{}`",
                            item.upgrade_command
                        ),
                    ),
                    Some(_) => (
                        "supported",
                        true,
                        false,
                        format!("本机版本 {installed} 满足最低支持版本（>= {min_ver}）"),
                    ),
                    None if installed.is_empty() => (
                        "unknown",
                        false,
                        false,
                        format!("未能从 `{raw_ver}` 解析出可比较版本号"),
                    ),
                    None => (
                        "uncomparable",
                        false,
                        false,
                        format!("版本 {installed} 与目标 {min_ver} 格式暂无法比较"),
                    ),
                };
                json!({
                    "available": true,
                    "status": status,
                    "binary_path": bin_path.to_string_lossy(),
                    "raw_version": raw_ver,
                    "installed_version": installed,
                    "meets_minimum": meets_min,
                    "below_minimum": below_min && policy.enforce,
                    "below_minimum_raw": below_min,
                    "detail": detail,
                })
            }
            Some(Err(err)) => json!({
                "available": false,
                "status": if err.code.contains("not_found") || err.code.contains("missing") { "missing" } else { "error" },
                "binary_path": "",
                "raw_version": "",
                "installed_version": "",
                "meets_minimum": false,
                "below_minimum": false,
                "below_minimum_raw": false,
                "error_code": err.code,
                "detail": err.message,
            }),
            None => json!({
                "available": false,
                "status": "missing",
                "binary_path": "",
                "raw_version": "",
                "installed_version": "",
                "meets_minimum": false,
                "below_minimum": false,
                "below_minimum_raw": false,
                "detail": "未找到引擎描述",
            }),
        };

        let container_json = if !worker_capable {
            json!({
                "available": false,
                "status": "not_worker",
                "installed_version": "",
                "meets_minimum": false,
                "below_minimum": false,
                "below_minimum_raw": false,
                "detail": "仅支持宿主机对话与 SRC 模式主控，不作为出战 Worker",
            })
        } else if !image_pulled {
            json!({
                "available": false,
                "status": "image_unavailable",
                "installed_version": "",
                "meets_minimum": false,
                "below_minimum": false,
                "below_minimum_raw": false,
                "detail": img_status["pulled"]["detail"].as_str().unwrap_or("本地未拉取 muteki-worker 容器镜像"),
            })
        } else if !image_declared {
            json!({
                "available": true,
                "status": "undeclared",
                "installed_version": "",
                "meets_minimum": false,
                "below_minimum": false,
                "below_minimum_raw": false,
                "detail": "当前容器镜像未携带 IMAGE_CAPABILITIES 清单",
            })
        } else {
            let cap_row = &img_status["capabilities"]["engines"][item.engine];
            if !cap_row.is_object() || cap_row["available"].as_bool() == Some(false) {
                json!({
                    "available": false,
                    "status": "not_in_image",
                    "installed_version": "",
                    "meets_minimum": false,
                    "below_minimum": false,
                    "below_minimum_raw": false,
                    "detail": "当前容器镜像未内置此 CLI，请使用「本机直跑」",
                })
            } else {
                let raw_ver = cap_row["version"].as_str().unwrap_or("").trim();
                let installed = version_check::installed_version(item.engine, raw_ver);
                let cmp = (!installed.is_empty() && !min_ver.is_empty())
                    .then(|| version_check::compare_for_engine(item.engine, &installed, &min_ver))
                    .flatten();
                let (status, meets_min, below_min, detail) = match cmp {
                    Some(-1) => (
                        "below_minimum",
                        false,
                        true,
                        format!(
                            "容器镜像内版本 {installed} 低于最低支持版本 {min_ver}，容器模式下禁止设为出战 Worker；请更新镜像或改用本机直跑"
                        ),
                    ),
                    Some(_) => (
                        "supported",
                        true,
                        false,
                        format!("容器镜像内版本 {installed} 满足最低支持版本（>= {min_ver}）"),
                    ),
                    None => (
                        "unknown",
                        false,
                        false,
                        format!("容器镜像内版本 `{raw_ver}` 暂无法比较"),
                    ),
                };
                json!({
                    "available": true,
                    "status": status,
                    "raw_version": raw_ver,
                    "installed_version": installed,
                    "meets_minimum": meets_min,
                    "below_minimum": below_min && policy.enforce,
                    "below_minimum_raw": below_min,
                    "detail": detail,
                })
            }
        };

        rows.push(json!({
            "engine": item.engine,
            "cli_engine": if worker_capable { format!("cli-{}", item.engine) } else { String::new() },
            "display_name": desc.map(|d| d.identity.display_name.as_str()).unwrap_or(item.display_name),
            "worker_capable": worker_capable,
            "supported_modes": if worker_capable {
                json!(["ctf", "pentest", "src"])
            } else {
                json!(["src"])
            },
            "baseline_version": item.baseline_version(),
            "min_version": min_ver,
            "upgrade_command": item.upgrade_command,
            "alt_upgrade_command": item.alt_upgrade_command,
            "upgrade_note": item.upgrade_note,
            "container_upgrade_note": item.container_upgrade_note,
            "local": local_json,
            "container": container_json,
        }));
    }

    json!({
        "policy": {
            "enforce": policy.enforce,
            "modes": policy.modes,
            "updated_at": policy.updated_at,
        },
        "baseline_versions": baseline_map(),
        "worker_image": {
            "image": image_name,
            "pulled": image_pulled,
            "declared": image_declared,
        },
        "engines": rows,
        "evaluated_at": chrono::Utc::now().to_rfc3339(),
    })
}


type ApiResult = Result<Json<Value>, (StatusCode, Json<Value>)>;

fn bad_request(code: &str, message: impl Into<String>) -> (StatusCode, Json<Value>) {
    let msg = message.into();
    (
        StatusCode::UNPROCESSABLE_ENTITY,
        Json(json!({
            "detail": msg,
            "error": {
                "code": code,
                "message": msg,
            }
        })),
    )
}

fn storage_err(err: impl std::fmt::Display) -> (StatusCode, Json<Value>) {
    let msg = format!("无法读写 Agent 最低版本策略存储: {err}");
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({
            "detail": msg,
            "error": {
                "code": "version_policy.storage_error",
                "message": msg,
            }
        })),
    )
}

pub fn router() -> Router<Arc<AppState>> {
    Router::new().route(
        "/api/settings/agent-version-policy",
        get(get_version_policy).put(put_version_policy),
    )
}

pub(crate) async fn get_version_policy(
    State(state): State<Arc<AppState>>,
    Query(query): Query<HashMap<String, String>>,
) -> ApiResult {
    let fresh = matches!(
        query.get("fresh").map(String::as_str),
        Some("1" | "true" | "yes")
    );
    Ok(Json(evaluate_all(&state, fresh).await))
}

pub(crate) async fn put_version_policy(
    State(state): State<Arc<AppState>>,
    Json(patch): Json<Value>,
) -> ApiResult {
    let Some(obj) = patch.as_object() else {
        return Err(bad_request(
            "version_policy.invalid_payload",
            "请求体必须是 JSON 对象",
        ));
    };

    if obj.contains_key("min_versions") {
        return Err(bad_request(
            "version_policy.min_versions_fixed",
            "Agent 最低版本由程序内置，不支持修改",
        ));
    }

    let mut policy = load_policy(&state.store);

    if obj.get("reset_to_baseline").and_then(Value::as_bool) == Some(true) {
        policy = AgentVersionPolicy::default();
    }

    if let Some(enforce) = obj.get("enforce") {
        let Some(flag) = enforce.as_bool() else {
            return Err(bad_request(
                "version_policy.invalid_enforce",
                "enforce 字段必须是布尔值",
            ));
        };
        policy.enforce = flag;
    }

    if let Some(modes) = obj.get("modes") {
        let Some(modes_obj) = modes.as_object() else {
            return Err(bad_request(
                "version_policy.invalid_modes",
                "modes 字段必须是对象",
            ));
        };
        for (key, value) in modes_obj {
            let Some(flag) = value.as_bool() else {
                return Err(bad_request(
                    "version_policy.invalid_modes",
                    format!("modes.{key} 必须是布尔值"),
                ));
            };
            match key.as_str() {
                "ctf" => policy.modes.ctf = flag,
                "pentest" => policy.modes.pentest = flag,
                "src" => policy.modes.src = flag,
                _ => {
                    return Err(bad_request(
                        "version_policy.unknown_mode",
                        format!("未知模式开关: {key}（仅支持 ctf、pentest、src）"),
                    ));
                }
            }
        }
    }

    policy.updated_at = chrono::Utc::now().to_rfc3339();
    let stored = serde_json::to_value(&policy).map_err(storage_err)?;
    state
        .store
        .put(SETTINGS_NAMESPACE, SETTINGS_KEY, &stored)
        .map_err(storage_err)?;

    Ok(Json(evaluate_all(&state, false).await))
}
