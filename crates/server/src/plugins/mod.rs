//! Managed Agent extension packages for chat: an immutable snapshot registry,
//! per-engine skill catalogs, visual replies, and the package MCP servers the
//! host connects to on behalf of a session.
//!
//! Packages are snapshot-ID-named directories under `<state>/chat-plugins/packages`.
//! The registry (`registry.sqlite3`) holds one JSON record per package and a
//! small policy table; every read-modify-write holds both an in-process lock
//! and an exclusive lock on `registry.lock`, so a second process sharing the
//! state directory observes the same serialization.

pub mod api;
pub mod inspect;
pub mod mcp_client;
mod runtime;
mod worker_staging;
mod sources;
pub mod visualize;

pub use inspect::{MODES, PluginError, Result};
pub use worker_staging::StagedWorkerComponents;

use crate::agent_runtime::descriptors;
use inspect::{compatibility, contained, engines};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

const VISUALIZE: &str = "muteki-visualize";
const DEFAULT_INSTALL_KEY: &str = "default-install:muteki-visualize:v1";
const SUPPRESSED_KEY: &str = "default-suppressed:muteki-visualize";
const BUNDLED_VISUALIZE: [(&str, &str); 3] = [
    ("plugin.json", include_str!("../../assets/agent_plugins/muteki-visualize/plugin.json")),
    ("skills/visualize/SKILL.md", include_str!("../../assets/agent_plugins/muteki-visualize/skills/visualize/SKILL.md")),
    (
        "skills/visualize/assets/explorer.html",
        include_str!("../../assets/agent_plugins/muteki-visualize/skills/visualize/assets/explorer.html"),
    ),
];
pub(crate) const MANAGED_SOURCE: &str = "Muteki 聊天插件";

fn sha_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn store_error(_: impl std::fmt::Display) -> PluginError {
    PluginError::new("chat_plugin.registry_unavailable", "扩展注册表不可用")
}

fn strings(value: &Value) -> Vec<String> {
    value.as_array().into_iter().flatten().filter_map(Value::as_str).map(str::to_owned).collect()
}

fn modes_of(record: &Value) -> Vec<String> {
    if record["modes"].is_array() { strings(&record["modes"]) } else { vec!["chat".into()] }
}

fn allowed_modes_of(record: &Value) -> Vec<String> {
    if record["allowed_modes"].is_array() {
        strings(&record["allowed_modes"])
    } else {
        MODES.iter().map(|m| (*m).to_owned()).collect()
    }
}

fn declared_engines_of(record: &Value) -> Vec<String> {
    if record["declared_engines"].is_array() { strings(&record["declared_engines"]) } else { engines() }
}

fn has_mcp(record: &Value) -> bool {
    record["mcp"].as_object().is_some_and(|m| !m.is_empty())
}

fn hooks_approved(record: &Value) -> bool {
    record["enabled"] != false
}

pub(super) fn snapshot_id(record: &Value) -> &str {
    record["snapshot_id"].as_str()
        .or_else(|| record["digest"].as_str())
        .unwrap_or("")
}

fn approved_snapshot_id(record: &Value) -> &str {
    record["hooks_approved_snapshot_id"].as_str()
        .or_else(|| record["hooks_approved_digest"].as_str())
        .unwrap_or("")
}

fn has_hooks(skill: &Value) -> bool {
    skill["hooks"].as_object().is_some_and(|m| !m.is_empty())
}

fn skill_portable(skill: &Value, engine: &str, native: &[String]) -> bool {
    skill["requires_native"] != true || native.iter().any(|e| e == engine)
}

pub fn host_discovery_enabled() -> bool {
    std::env::var("MUTEKI_HOST_DISCOVERY").map_or(true, |v| v != "0")
}

/// One managed skill an engine may use in chat.
#[derive(Debug, Clone)]
pub struct ManagedSkill {
    pub row: Value,
    pub path: PathBuf,
    pub package_root: PathBuf,
}

pub struct Registry {
    root: PathBuf,
    lock: Mutex<()>,
    diagnostics: Mutex<HashMap<String, String>>,
    verified: Mutex<HashMap<String, HashSet<String>>>,
    runtime: runtime::Runtime,
}

/// The registry is opened on first use from the state directory recorded in
/// the store, so tests and tools that never touch extensions pay nothing.
#[derive(Default)]
pub struct Plugins {
    registry: Mutex<Option<Arc<Registry>>>,
}

impl Plugins {
    pub fn open(&self, state_dir: &Path) -> Result<Arc<Registry>> {
        let mut slot = self.registry.lock().unwrap();
        if let Some(registry) = slot.as_ref() {
            return Ok(registry.clone());
        }
        let registry = Arc::new(Registry::open(&state_dir.join("chat-plugins"))?);
        *slot = Some(registry.clone());
        Ok(registry)
    }

    /// The registry if it was already opened; never opens it.
    pub fn opened(&self) -> Option<Arc<Registry>> {
        self.registry.lock().unwrap().clone()
    }
}

impl Registry {
    pub fn mcp_circuit_snapshots(&self) -> Vec<Value> {
        self.runtime.circuit_snapshots()
    }

    pub fn reset_mcp_circuits(&self, server_filter: Option<&str>) -> usize {
        self.runtime.reset_circuits(server_filter)
    }
}

pub fn registry(s: &crate::AppState) -> Result<Arc<Registry>> {
    s.plugins.open(&crate::native_chat::state_dir(s))
}

/// Where a thread's visual replies live and which skills shape them. The
/// scope is the session directory, so a session started ahead of its thread
/// in the same directory carries the same instructions.
pub struct VisualContext {
    pub scope: String,
    pub engine: String,
    pub workspace_root: String,
}

pub fn visual_context(s: &crate::AppState, thread_id: &str, doc: &Value) -> VisualContext {
    let engine = doc["runtime"]["adapter_id"]
        .as_str()
        .and_then(descriptors::for_adapter)
        .map(|(engine, _)| engine.engine().to_owned())
        .unwrap_or_default();
    let scope = crate::native_chat::session_cwd(s, doc, thread_id)
        .map(|path| path.to_string_lossy().into_owned())
        .unwrap_or_else(|_| thread_id.to_owned());
    VisualContext { scope, engine, workspace_root: crate::chat::workspace_root(s, doc) }
}

struct FileLock(std::fs::File);

impl Drop for FileLock {
    fn drop(&mut self) {
        let _ = fs2::FileExt::unlock(&self.0);
    }
}

impl Registry {
    pub fn open(root: &Path) -> Result<Self> {
        std::fs::create_dir_all(root).map_err(store_error)?;
        set_mode(root, 0o700);
        let root = std::fs::canonicalize(root).map_err(store_error)?;
        let registry = Self {
            root,
            lock: Mutex::new(()),
            diagnostics: Mutex::default(),
            verified: Mutex::default(),
            runtime: runtime::Runtime::default(),
        };
        {
            let _guard = registry.transaction()?;
            let conn = registry.connect()?;
            conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS packages (id TEXT PRIMARY KEY, value TEXT NOT NULL);
                 CREATE TABLE IF NOT EXISTS policy (id TEXT PRIMARY KEY, value TEXT NOT NULL);",
            )
            .map_err(store_error)?;
            set_mode(&registry.db(), 0o600);
            for mut record in registry.records()? {
                let mut changed = false;
                if record["mode_scope_version"] != 1 {
                    record["modes"] = json!(["chat"]);
                    record["mode_scope_version"] = json!(1);
                    changed = true;
                }
                if record["scope_version"] != 2 {
                    record["enabled"] = json!(record["engines"].as_array().is_some_and(|e| !e.is_empty()));
                    record["engines"] = json!(engines());
                    record["scope_version"] = json!(2);
                    changed = true;
                }
                if record["component_schema_version"] != 6 {
                    let inspected = inspect::inspect(Path::new(record["root"].as_str().unwrap_or("")))?;
                    for key in ["components", "requirements", "declared_engines", "skills", "environment", "allowed_modes", "component_schema_version"] {
                        record[key] = inspected[key].clone();
                    }
                    changed = true;
                }
                if changed {
                    registry.save(&record)?;
                }
            }
            registry.install_default_visualize_once()?;
        }
        Ok(registry)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn db(&self) -> PathBuf {
        self.root.join("registry.sqlite3")
    }

    fn connect(&self) -> Result<Connection> {
        let conn = Connection::open(self.db()).map_err(store_error)?;
        conn.busy_timeout(std::time::Duration::from_secs(10)).map_err(store_error)?;
        Ok(conn)
    }

    /// Serialize a read-modify-write across threads and processes.
    fn transaction(&self) -> Result<(std::sync::MutexGuard<'_, ()>, FileLock)> {
        let guard = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        let path = self.root.join("registry.lock");
        let file = std::fs::OpenOptions::new().create(true).append(true).open(&path).map_err(store_error)?;
        set_mode(&path, 0o600);
        fs2::FileExt::lock_exclusive(&file).map_err(store_error)?;
        Ok((guard, FileLock(file)))
    }

    pub fn records(&self) -> Result<Vec<Value>> {
        let conn = self.connect()?;
        let mut statement = conn.prepare("SELECT value FROM packages ORDER BY id").map_err(store_error)?;
        let rows = statement.query_map([], |row| row.get::<_, String>(0)).map_err(store_error)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(serde_json::from_str(&row.map_err(store_error)?).map_err(store_error)?);
        }
        Ok(out)
    }

    pub fn get(&self, id: &str) -> Result<Option<Value>> {
        Ok(self.records()?.into_iter().find(|r| r["id"] == id))
    }

    fn save(&self, record: &Value) -> Result<()> {
        self.connect()?
            .execute(
                "INSERT OR REPLACE INTO packages VALUES (?1, ?2)",
                params![record["id"].as_str().unwrap_or(""), record.to_string()],
            )
            .map_err(store_error)?;
        Ok(())
    }

    fn policy(&self, key: &str) -> Result<Option<String>> {
        self.connect()?
            .query_row("SELECT value FROM policy WHERE id=?1", [key], |row| row.get::<_, String>(0))
            .optional()
            .map_err(store_error)
    }

    fn set_policy(&self, key: &str, value: &str) -> Result<()> {
        self.connect()?
            .execute("INSERT OR REPLACE INTO policy VALUES (?1, ?2)", params![key, value])
            .map_err(store_error)?;
        Ok(())
    }

    pub fn control_enabled(&self) -> bool {
        self.policy("control").ok().flatten().is_none_or(|value| value == "true")
    }

    pub fn set_control(&self, enabled: bool) -> Result<()> {
        self.set_policy("control", if enabled { "true" } else { "false" })
    }

    fn install_default_visualize_once(&self) -> Result<()> {
        if self.policy(DEFAULT_INSTALL_KEY)?.is_some() {
            return Ok(());
        }
        if self.get(VISUALIZE)?.is_none() && self.policy(SUPPRESSED_KEY)?.as_deref() != Some("true") {
            self.install_visualize_locked()?;
        }
        self.connect()?
            .execute("INSERT OR IGNORE INTO policy VALUES (?1, 'true')", [DEFAULT_INSTALL_KEY])
            .map_err(store_error)?;
        Ok(())
    }


    /// Enabled packages that reach `engine` in `mode`.
    pub fn enabled(&self, engine: &str, mode: &str) -> Result<Vec<Value>> {
        if !MODES.contains(&mode) {
            return Err(PluginError::invalid("未知使用场景"));
        }
        if mode != "chat" && !inspect::managed_environment(engine) {
            return Ok(Vec::new());
        }
        let native = inspect::native_skill_engines();
        let workers = inspect::worker_component_engines();
        Ok(self
            .records()?
            .into_iter()
            .filter(|r| {
                r["enabled"] != false
                    && modes_of(r).iter().any(|m| m == mode)
                    && allowed_modes_of(r).iter().any(|m| m == mode)
                    && if mode == "chat" {
                        compatibility(r, None, false)[engine]["status"] != "blocked"
                    } else {
                        declared_engines_of(r).iter().any(|e| e == engine)
                            && (r["skills"].as_array().into_iter().flatten().any(|s| skill_portable(s, engine, &native))
                                || has_mcp(r)
                                || r["components"].as_array().into_iter().flatten().any(|c| {
                                    matches!(c["kind"].as_str(), Some("extensions" | "agents"))
                                        && workers.iter().any(|e| e == engine)
                                        && strings(&c["engines"]).iter().any(|e| e == engine)
                                }))
                    }
            })
            .collect())
    }

    /// Changes whenever what a session of `engine` would receive changes.
    pub fn revision(&self, engine: &str, mode: &str) -> Result<String> {
        let values: Vec<Value> = self
            .enabled(engine, mode)?
            .iter()
            .map(|r| json!([r["id"], snapshot_id(r), approved_snapshot_id(r)]))
            .collect();
        let native = if mode == "chat" && descriptors::find(engine).is_some() && host_discovery_enabled() {
            native_revision(engine)
        } else {
            String::new()
        };
        let control = mode == "chat" && self.control_enabled();
        Ok(json!([9, mode, values, control, native]).to_string())
    }

    /// Changes whenever what a new chat session of `engine` receives from
    /// managed packages changes; the engine's own configuration is left to
    /// the engine.
    pub fn session_revision(&self, engine: &str) -> Result<String> {
        let values: Vec<Value> = self
            .enabled(engine, "chat")?
            .iter()
            .map(|r| json!([r["id"], snapshot_id(r), approved_snapshot_id(r)]))
            .collect();
        Ok(json!([values, self.control_enabled()]).to_string())
    }

    pub fn validate_modes(modes: &Value) -> Result<Vec<String>> {
        let list = strings(modes);
        let valid = modes.as_array().is_some_and(|items| {
            !items.is_empty() && items.len() == list.len() && list.iter().all(|m| MODES.contains(&m.as_str()))
        });
        if !valid {
            return Err(PluginError::invalid("请至少选择一个有效使用场景：聊天、CTF 或渗透测试"));
        }
        Ok(MODES.iter().filter(|m| list.iter().any(|l| l == *m)).map(|m| (*m).to_owned()).collect())
    }

    fn validate_record_modes(record: &Value, modes: &Value) -> Result<Vec<String>> {
        let selected = Self::validate_modes(modes)?;
        let allowed = allowed_modes_of(record);
        if selected.iter().any(|m| !allowed.contains(m)) {
            return Err(PluginError::new("chat_plugin.mode_unsupported", "此插件不支持所选场景"));
        }
        if selected.iter().any(|m| m != "chat") {
            let declared: HashSet<String> = declared_engines_of(record).into_iter().collect();
            let native: HashSet<String> = inspect::native_skill_engines().into_iter().collect();
            let workers: HashSet<String> = inspect::worker_component_engines().into_iter().collect();
            let portable = !declared.is_empty()
                && (has_mcp(record)
                    || record["skills"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .any(|s| s["requires_native"] != true || declared.intersection(&native).next().is_some()));
            let native_component = record["components"].as_array().into_iter().flatten().any(|c| {
                matches!(c["kind"].as_str(), Some("extensions" | "agents"))
                    && strings(&c["engines"]).iter().any(|e| declared.contains(e) && workers.contains(e))
            });
            if !portable && !native_component {
                return Err(PluginError::invalid("此包没有可投放给 Worker 的 Skill、MCP 或兼容原生插件"));
            }
        }
        Ok(selected)
    }


    /// Fetch, normalize and inspect a source without the registry lock.
    fn stage(&self, source: &Value) -> Result<Staged> {
        let kind = source["kind"].as_str().unwrap_or("local-dir").to_owned();
        if !["local-dir", "archive", "git"].contains(&kind.as_str()) {
            return Err(PluginError::new("chat_plugin.unsupported_source", "支持本地目录、压缩包和固定 Git 版本"));
        }
        let text = |key: &str| match &source[key] {
            Value::String(s) => s.clone(),
            Value::Null => String::new(),
            other => other.to_string(),
        };
        let work = tempfile::Builder::new().prefix("import-").tempdir_in(&self.root).map_err(store_error)?;
        let package = match kind.as_str() {
            "local-dir" => {
                let origin = sources::resolve_local_dir(&text("path"))?;
                let home = std::env::var_os("HOME").map(PathBuf::from).and_then(|h| std::fs::canonicalize(h).ok());
                if home.as_deref() == Some(origin.as_path()) || self.root.starts_with(&origin) {
                    return Err(PluginError::new("chat_plugin.source_too_broad", "请选择具体插件目录"));
                }
                let package = work.path().join("package");
                sources::copy_package(&origin, &package)?;
                package
            }
            "archive" => sources::unpack(Path::new(&text("path")), &work.path().join("pkg"))?,
            _ => sources::fetch_git(&text("url"), &text("ref"), work.path())?,
        };
        let normalized = work.path().join("normalized");
        sources::copy_package(&package, &normalized)?;
        let mut record = inspect::inspect(&normalized)?;
        record["snapshot_id"] = json!(uuid::Uuid::new_v4().to_string());
        record["source_kind"] = json!(kind);
        Ok(Staged { _work: work, normalized, record })
    }

    /// Publish a staged package. The caller holds the registry transaction.
    fn publish(&self, staged: Staged, modes: Option<&Value>) -> Result<Value> {
        let Staged { _work, normalized, mut record } = staged;
        let id = record["id"].as_str().unwrap_or("").to_owned();
        let snapshot = snapshot_id(&record).to_owned();
        let target = self.root.join("packages").join(&id).join(&snapshot);
        let previous = self.get(&id)?;
        let requested = match modes {
            Some(value) if !value.is_null() => value.clone(),
            _ => previous.as_ref().map(|p| json!(modes_of(p))).unwrap_or_else(|| json!(["chat"])),
        };
        record["engines"] = json!(engines());
        record["enabled"] = json!(true);
        record["scope_version"] = json!(2);
        record["modes"] = json!(Self::validate_record_modes(&record, &requested)?);
        record["mode_scope_version"] = json!(1);
        if std::fs::symlink_metadata(&target).is_err() {
            let published = target
                .parent()
                .map(std::fs::create_dir_all)
                .transpose()
                .and_then(|_| std::fs::rename(&normalized, &target));
            if published.is_err() {
                return Err(PluginError::new("chat_plugin.install_failed", "无法写入扩展包目录"));
            }
        }
        record["root"] = json!(target.to_string_lossy());
        record["previous"] = match previous {
            Some(mut previous) => {
                if let Some(object) = previous.as_object_mut() {
                    object.remove("previous");
                }
                previous
            }
            None => Value::Null,
        };
        self.save(&record)?;
        Ok(self.public(&record))
    }

    /// Install from `{kind: local-dir|archive|git, path, url, ref}`.
    pub fn install(&self, source: &Value, modes: Option<&Value>) -> Result<Value> {
        let staged = self.stage(source)?;
        let _guard = self.transaction()?;
        self.publish(staged, modes)
    }

    fn install_visualize_locked(&self) -> Result<Value> {
        let bundle = tempfile::Builder::new().prefix("bundled-").tempdir_in(&self.root).map_err(store_error)?;
        let package = bundle.path().join(VISUALIZE);
        for (relative, body) in BUNDLED_VISUALIZE {
            let path = package.join(relative);
            std::fs::create_dir_all(path.parent().unwrap()).map_err(store_error)?;
            std::fs::write(&path, body).map_err(store_error)?;
        }
        let staged = self.stage(&json!({"kind": "local-dir", "path": package.to_string_lossy()}))?;
        let result = self.publish(staged, Some(&json!(["chat"])))?;
        self.connect()?.execute("DELETE FROM policy WHERE id=?1", [SUPPRESSED_KEY]).map_err(store_error)?;
        Ok(result)
    }

    /// The bundled chat visualizer, through the normal package lifecycle.
    pub fn install_visualize(&self) -> Result<Value> {
        let _guard = self.transaction()?;
        self.install_visualize_locked()
    }

    /// A package made only of MCP servers, materialized as a plugin.json.
    pub fn add_mcp(&self, name: &str, servers: &Value, modes: Option<&Value>) -> Result<Value> {
        inspect::validate_mcp(servers, None)?;
        if servers.as_object().is_none_or(Map::is_empty) {
            return Err(PluginError::invalid("请至少配置一个 MCP 服务"));
        }
        let temp = tempfile::Builder::new().tempdir_in(&self.root).map_err(store_error)?;
        let manifest = json!({"name": name, "version": "local", "mcpServers": servers});
        std::fs::write(temp.path().join("plugin.json"), manifest.to_string()).map_err(store_error)?;
        self.install(&json!({"kind": "local-dir", "path": temp.path().to_string_lossy()}), modes)
    }

    pub fn update(
        &self,
        id: &str,
        enabled: Option<bool>,
        rollback: bool,
        native_hooks: Option<bool>,
        requested_snapshot_id: &str,
        modes: Option<&Value>,
    ) -> Result<Value> {
        let _guard = self.transaction()?;
        let Some(mut record) = self.get(id)? else {
            return Err(PluginError::invalid("插件不存在"));
        };
        if rollback {
            let Some(previous) = record["previous"].as_object().cloned() else {
                return Err(PluginError::invalid("没有可回滚的版本"));
            };
            let mut restored = Value::Object(previous);
            restored["engines"] = json!(engines());
            restored["enabled"] = record["enabled"].clone();
            restored["scope_version"] = json!(2);
            restored["modes"] = json!(modes_of(&record));
            restored["mode_scope_version"] = json!(1);
            restored["previous"] = Value::Null;
            record = restored;
        }
        if let Some(enabled) = enabled {
            record["enabled"] = json!(enabled);
        }
        if let Some(modes) = modes.filter(|m| !m.is_null()) {
            record["modes"] = json!(Self::validate_record_modes(&record, modes)?);
        }
        if let Some(approve) = native_hooks {
            let _ = requested_snapshot_id;
            record["hooks_approved_snapshot_id"] = if approve { json!(snapshot_id(&record)) } else { json!("") };
        }
        self.save(&record)?;
        Ok(self.public(&record))
    }

    /// Remove a package from the registry. Snapshot files stay for in-flight
    /// skill references; no host path is touched.
    pub fn uninstall(&self, id: &str) -> Result<()> {
        let _guard = self.transaction()?;
        self.connect()?.execute("DELETE FROM packages WHERE id=?1", [id]).map_err(store_error)?;
        if id == VISUALIZE {
            self.set_policy(SUPPRESSED_KEY, "true")?;
        }
        Ok(())
    }


    pub fn public(&self, record: &Value) -> Value {
        let native = inspect::native_skill_engines();
        let workers = inspect::worker_component_engines();
        let declared = declared_engines_of(record);
        let worker_modes = allowed_modes_of(record).iter().any(|m| m != "chat");
        let mut worker_compatibility = Map::new();
        for engine in engines() {
            let mut components = Vec::new();
            if record["skills"].as_array().into_iter().flatten().any(|s| {
                skill_portable(s, &engine, &native) && (!has_hooks(s) || hooks_approved(record))
            }) {
                components.push("Skill");
            }
            if has_mcp(record) {
                components.push("MCP");
            }
            if workers.contains(&engine)
                && record["components"].as_array().into_iter().flatten().any(|c| {
                    matches!(c["kind"].as_str(), Some("extensions" | "agents")) && strings(&c["engines"]).contains(&engine)
                })
            {
                components.push("原生插件");
            }
            if !worker_modes || !inspect::managed_environment(&engine) {
                components.clear();
            }
            let declared_here = declared.contains(&engine);
            worker_compatibility.insert(
                engine,
                json!({
                    "status": if !components.is_empty() && declared_here { "available" } else { "unavailable" },
                    "components": if declared_here { components } else { Vec::new() },
                }),
            );
        }
        let id = record["id"].as_str().unwrap_or("");
        let diagnostic = self.diagnostics.lock().unwrap().get(id).cloned().filter(|d| !d.is_empty());
        let verified = self.verified.lock().unwrap().clone();
        let mut out = Map::new();
        for key in ["id", "name", "version", "description", "enabled", "engines", "snapshot_id", "digest", "skills", "native_components"] {
            out.insert(key.into(), record.get(key).cloned().unwrap_or(Value::Null));
        }
        out.insert("modes".into(), json!(modes_of(record)));
        out.insert("allowed_modes".into(), json!(allowed_modes_of(record)));
        out.insert("worker_compatibility".into(), Value::Object(worker_compatibility));
        out.insert("mcp_servers".into(), json!(record["mcp"].as_object().map(|m| m.keys().cloned().collect::<Vec<_>>()).unwrap_or_default()));
        out.insert("origin".into(), json!("managed"));
        out.insert("can_rollback".into(), json!(record["previous"].is_object()));
        out.insert("diagnostics".into(), json!(diagnostic.into_iter().collect::<Vec<_>>()));
        out.insert("compatibility".into(), compatibility(record, Some(&verified), true));
        out.insert("native_hooks_enabled".into(), json!(hooks_approved(record)));
        out.insert("hook_commands".into(), json!(inspect::hook_commands(record)));
        out.insert(
            "hook_definitions".into(),
            json!(record["components"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|c| matches!(c["kind"].as_str(), Some("hooks" | "skill_hooks")))
                .map(|c| c["config"].clone())
                .collect::<Vec<_>>()),
        );
        Value::Object(out)
    }

    /// Skills from enabled packages that `engine` can use in chat.
    pub fn skill_rows(&self, engine: &str, include_automatic: bool) -> Result<Vec<ManagedSkill>> {
        let native = inspect::native_skill_engines();
        let verified = self.verified.lock().unwrap().clone();
        let mut rows = Vec::new();
        for record in self.enabled(engine, "chat")? {
            if compatibility(&record, Some(&verified), true)[engine]["status"] == "blocked" {
                continue;
            }
            let root = PathBuf::from(record["root"].as_str().unwrap_or(""));
            let id = record["id"].as_str().unwrap_or("");
            let snapshot = snapshot_id(&record);
            for skill in record["skills"].as_array().into_iter().flatten() {
                if has_hooks(skill) && !hooks_approved(&record) {
                    continue;
                }
                let requires_native = skill["requires_native"] == true;
                if requires_native && !native.iter().any(|e| e == engine) {
                    continue;
                }
                if skill["invocable"] == false && !include_automatic {
                    continue;
                }
                let name = skill["name"].as_str().unwrap_or("");
                let Ok(path) = contained(&root, skill["path"].as_str().unwrap_or("")) else { continue };
                rows.push(ManagedSkill {
                    row: json!({
                        "id": format!("managed:{engine}:{id}:{}:{name}", &snapshot[..snapshot.len().min(12)]),
                        "kind": "skill", "name": format!("{id}:{name}"),
                        "description": skill["description"], "source": MANAGED_SOURCE, "scope": "chat",
                        "engine": engine,
                        "component_kind": skill["component_kind"].as_str().unwrap_or("skill"),
                        "native_engine": if requires_native { engine } else { "" },
                        "native_name": if requires_native { format!("muteki-{id}:{name}") } else { String::new() },
                    }),
                    path,
                    package_root: root.clone(),
                });
            }
        }
        Ok(rows)
    }

    /// The session instructions listing managed skills, when there are any.
    pub fn skill_catalog_instructions(&self, engine: &str) -> Result<Option<String>> {
        self.skill_catalog_instructions_with_builtins(engine, &[])
    }

    /// The session instructions listing built-in session skills and managed
    /// chat plugin skills, when there are any.
    pub fn skill_catalog_instructions_with_builtins(&self, engine: &str, builtins: &[Value]) -> Result<Option<String>> {
        let mut rows: Vec<Value> = builtins.to_vec();
        rows.extend(
            self.skill_rows(engine, true)?
                .into_iter()
                .filter(|skill| skill.row["native_engine"].as_str().unwrap_or("").is_empty())
                .map(|skill| {
                    let description: String = skill.row["description"].as_str().unwrap_or("").chars().take(600).collect();
                    json!({"name": skill.row["name"], "description": description, "path": skill.path.to_string_lossy()})
                }),
        );
        if rows.is_empty() {
            return Ok(None);
        }
        Ok(Some(format!(
            "以下 Skills 目录来自 Muteki 当前会话内置能力与已启用的聊天插件。仅在任务需要时读取相应 SKILL.md，并按其所在目录解析相对资源路径。这些插件仅作用于本次聊天，不要安装或写入用户的 Agent 配置目录。\n{}",
            Value::Array(rows)
        )))
    }


    pub fn visualization_root(&self, scope: &str) -> std::io::Result<PathBuf> {
        let root = self.root.join("workspaces").join(&sha_hex(scope.as_bytes())[..24]).join("visualizations");
        std::fs::create_dir_all(&root)?;
        Ok(root)
    }

    fn visualize_skill(&self, engine: &str, workspace_root: &str) -> Option<PathBuf> {
        let managed = self.skill_rows(engine, false).ok().unwrap_or_default();
        managed
            .into_iter()
            .map(|skill| (skill.row["name"].as_str().unwrap_or("").to_owned(), skill.path))
            .chain(crate::composer::native_skill_paths(engine, workspace_root))
            .find(|(name, _)| name.rsplit(':').next() == Some("visualize"))
            .map(|(_, path)| path)
    }

    pub fn visualization_assets(&self, engine: &str, workspace_root: &str) -> Value {
        let managed = self.skill_rows(engine, false).ok().unwrap_or_default();
        let candidates = managed
            .into_iter()
            .map(|skill| (skill.row["name"].as_str().unwrap_or("").to_owned(), skill.path))
            .chain(crate::composer::native_skill_paths(engine, workspace_root));
        for (name, path) in candidates {
            if name.rsplit(':').next() != Some("visualize") {
                continue;
            }
            let Some(root) = path.parent() else { continue };
            let mut result = Map::new();
            for file in ["visualize.css", "visualize.html", "calendar.js"] {
                let Ok(asset) = contained(root, &format!("assets/{file}")) else { continue };
                if std::fs::metadata(&asset).is_ok_and(|m| m.is_file() && m.len() <= 600_000)
                    && let Ok(text) = std::fs::read_to_string(&asset)
                {
                    result.insert(file.into(), json!(text));
                }
            }
            if !result.is_empty() {
                return Value::Object(result);
            }
        }
        json!({})
    }

    /// The host instructions for visual replies in a session whose visual
    /// workspace is named by `scope`.
    pub fn visualization_instructions(&self, scope: &str, engine: &str, workspace_root: &str) -> std::io::Result<String> {
        let root = self.visualization_root(scope)?;
        Ok(visualize::instructions(&root, self.visualize_skill(engine, workspace_root).as_deref()))
    }

    fn snapshot_dir(&self, thread_id: &str) -> PathBuf {
        self.root.join("visualizations").join(sha_hex(thread_id.as_bytes()))
    }

    pub fn publish_visualizations(&self, thread_id: &str, scope: &str, messages: &[Value], engine: &str, workspace_root: &str) -> std::io::Result<()> {
        let workspace = self.visualization_root(scope)?;
        let assets = || self.visualization_assets(engine, workspace_root);
        for message in messages.iter().filter(|m| m["role"] == "assistant") {
            let id = message["message_id"].as_str().unwrap_or("");
            let references = visualize::references(message["text"].as_str().unwrap_or(""));
            let count = references.len();
            for (index, reference) in references.into_iter().enumerate() {
                visualize::snapshot(&self.snapshot_dir(thread_id), &workspace, id, index, count, &reference, &assets)?;
            }
        }
        Ok(())
    }

    /// The published document for one reference attached to exactly one
    /// assistant message.
    pub fn visualization_document(
        &self,
        thread_id: &str,
        scope: &str,
        messages: &[Value],
        engine: &str,
        workspace_root: &str,
        path: &str,
        message_id: &str,
    ) -> std::result::Result<Value, VisualFailure> {
        let matches: Vec<(&Value, usize, usize, visualize::Reference)> = messages
            .iter()
            .filter(|m| m["role"] == "assistant" && (message_id.is_empty() || m["message_id"] == message_id))
            .flat_map(|m| {
                let references = visualize::references(m["text"].as_str().unwrap_or(""));
                let count = references.len();
                references.into_iter().enumerate().filter(|(_, r)| r.path == path).map(move |(index, r)| (m, index, count, r))
            })
            .collect();
        let owners: HashSet<&str> = matches.iter().map(|(m, _, _, _)| m["message_id"].as_str().unwrap_or("")).collect();
        if owners.len() != 1 {
            return Err(VisualFailure::Rejected(visualize::VisualizationError::new(
                "visualization.not_attached",
                "图形未附着到当前消息，或来源消息不唯一",
            )));
        }
        let (message, index, count, reference) = &matches[0];
        let workspace = self.visualization_root(scope).map_err(VisualFailure::Store)?;
        let assets = || self.visualization_assets(engine, workspace_root);
        let id = message["message_id"].as_str().unwrap_or("");
        let result = visualize::snapshot(&self.snapshot_dir(thread_id), &workspace, id, *index, *count, reference, &assets).map_err(VisualFailure::Store)?;
        if let Some(error) = result.get("error") {
            return Err(VisualFailure::Rejected(visualize::VisualizationError::new(
                error["code"].as_str().unwrap_or("visualization.source_unavailable"),
                error["message"].as_str().unwrap_or(""),
            )));
        }
        Ok(result)
    }


    pub async fn prepare_tools(self: &Arc<Self>, engine: &str, mode: &str, scope: &str) -> Result<Vec<Value>> {
        runtime::prepare_tools(self, engine, mode, scope).await
    }

    pub async fn invoke(self: &Arc<Self>, engine: &str, name: &str, arguments: Value, mode: &str, scope: &str) -> Result<Value> {
        runtime::invoke(self, engine, name, arguments, mode, scope).await
    }

    pub async fn check(self: &Arc<Self>, record: &Value) -> Value {
        runtime::check(self, record).await
    }

    pub async fn invalidate(&self) {
        let active: HashSet<String> = self
            .records()
            .unwrap_or_default()
            .iter()
            .filter(|r| r["enabled"] != false)
            .flat_map(|r| {
                let prefix = format!("{}:{}", r["id"].as_str().unwrap_or(""), snapshot_id(r));
                r["mcp"].as_object().into_iter().flat_map(|m| m.keys()).map(move |server| format!("{prefix}:{server}")).collect::<Vec<_>>()
            })
            .collect();
        self.runtime.invalidate(&active).await;
        self.verified.lock().unwrap().clear();
        self.diagnostics.lock().unwrap().clear();
    }

    pub fn runtime_mcp_health(&self, engine: &str, mode: &str, scope: &str) -> Result<Vec<Value>> {
        runtime::health(self, engine, mode, scope)
    }

    pub fn cached_tool_names(&self, engine: &str, mode: &str, scope: &str) -> Vec<String> {
        let Ok(revision) = self.revision(engine, mode) else {
            return Vec::new();
        };
        let key = format!("{mode}:{engine}:{scope}:{revision}");
        self.runtime.cached_tool_names(&key)
    }

    /// Stop the MCP processes a finished Run owned.
    pub async fn release_scope(&self, scope: &str) {
        self.runtime.release_scope(scope).await;
    }

    pub async fn close(&self) {
        self.runtime.close().await;
    }
}

struct Staged {
    _work: tempfile::TempDir,
    normalized: PathBuf,
    record: Value,
}

pub enum VisualFailure {
    Rejected(visualize::VisualizationError),
    Store(std::io::Error),
}

fn set_mode(path: &Path, mode: u32) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode));
    }
    #[cfg(not(unix))]
    let _ = (path, mode);
}


fn native_root(engine: &str) -> Option<PathBuf> {
    let environment = &descriptors::find(engine)?.public()["environment"];
    let home = std::env::var_os("HOME").map(PathBuf::from)?;
    let variable = environment["home_env_var"].as_str().unwrap_or("");
    match std::env::var_os(variable).filter(|v| !variable.is_empty() && !v.is_empty()) {
        Some(value) => {
            let root = PathBuf::from(value);
            Some(if environment["home_env_is_parent"] == true { root.join(engine) } else { root })
        }
        None => Some(home.join(environment["home_relative"].as_str().unwrap_or(""))),
    }
}

/// MCP servers configured in the engine's own home: identities and status
/// only, since commands, URLs, env and headers often carry secrets.
pub fn native_mcp(engine: &str) -> Vec<Value> {
    if !host_discovery_enabled() {
        return Vec::new();
    }
    let Some(root) = native_root(engine) else { return Vec::new() };
    let environment = &descriptors::find(engine).map(|d| d.public()["environment"].clone()).unwrap_or(Value::Null);
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
    let mut paths: Vec<PathBuf> = strings(&environment["mcp_files"]).iter().map(|f| root.join(f)).collect();
    paths.extend(strings(&environment["extra_mcp_home_files"]).iter().map(|f| home.join(f)));
    let mut rows: Map<String, Value> = Map::new();
    for path in paths {
        let Ok(meta) = std::fs::metadata(&path) else { continue };
        if !meta.is_file() || meta.len() > 2_000_000 {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else { continue };
        let parsed: Option<Value> = if path.extension().is_some_and(|e| e == "toml") {
            toml::from_str::<toml::Value>(&text).ok().and_then(|v| serde_json::to_value(v).ok())
        } else {
            serde_json::from_str(&text).ok()
        };
        let Some(value) = parsed else { continue };
        let servers = value.get("mcpServers").or_else(|| value.get("mcp_servers")).or_else(|| value.get("mcp"));
        let Some(servers) = servers.and_then(Value::as_object) else { continue };
        for (name, config) in servers {
            let Some(config) = config.as_object() else { continue };
            let enabled = config.get("enabled").and_then(Value::as_bool).unwrap_or(true)
                && !config.get("disabled").and_then(Value::as_bool).unwrap_or(false);
            let stdio = config.get("command").is_some_and(|c| !c.is_null() && c != "" && c != &json!([]));
            rows.insert(
                name.clone(),
                json!({"name": name, "source": engine, "kind": "mcp", "enabled": enabled,
                       "status": "configured", "transport": if stdio { "stdio" } else { "http" }}),
            );
        }
    }
    rows.into_values().collect()
}

const CONFIGURATION_NAMES: [&str; 12] = [
    "config.toml", "settings.json", "settings.yaml", "settings.yml", "models.json", "models.yml",
    "mcp.json", "cli-config.json", "agent-cli-state.json", "acp-config.json", "opencode.json", "opencode.jsonc",
];
const CURSOR_RUNTIME_FIELDS: &[&str] = &[
    "authInfo", "privacyCache", "autoReviewAvailabilityCache", "serverConfigCache",
    "model", "selectedModel", "modelParameters", "modelSelectionHistory", "modelSlashCommands",
    "hasChangedDefaultModel", "maxMode", "maxModeAutoEnabled", "exploreSubagentModel",
];
const ASSET_NAMES: [&str; 7] = ["skills", "skills-cursor", "commands", "agents", "prompts", "plugins", "extensions"];

/// A revision of the engine's own configuration and capability assets.
pub fn native_revision(engine: &str) -> String {
    if !host_discovery_enabled() {
        return "host-discovery-disabled".into();
    }
    let Some(root) = native_root(engine) else { return String::new() };
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
    let mut entries: Vec<(String, PathBuf)> = CONFIGURATION_NAMES.iter().chain(ASSET_NAMES.iter()).map(|n| ((*n).to_owned(), root.join(n))).collect();
    if let Some(descriptor) = descriptors::find(engine) {
        let environment = &descriptor.public()["environment"];
        for file in strings(&environment["extra_mcp_home_files"]) {
            entries.push((format!("extra-mcp:{file}"), home.join(&file)));
        }
        for skill_root in strings(&environment["user_skill_roots"]) {
            if skill_root != ".agents/skills" {
                entries.push((format!("user-skills:{skill_root}"), home.join(&skill_root)));
            }
        }
    }
    entries.push(("common-skills".into(), home.join(".agents/skills")));
    let mut observed = Vec::new();
    for (name, path) in entries {
        for entry in walkdir::WalkDir::new(&path).max_depth(6).follow_links(false).into_iter().filter_map(|e| e.ok()) {
            let Ok(meta) = entry.metadata() else { continue };
            if engine == "cursor" && name == "cli-config.json" && entry.depth() == 0 && entry.path().is_file() {
                let fingerprint = match std::fs::read(entry.path()) {
                    Ok(bytes) => match serde_json::from_slice::<Value>(&bytes) {
                        Ok(mut config) => {
                            if let Some(object) = config.as_object_mut() {
                                object.retain(|key, _| !CURSOR_RUNTIME_FIELDS.contains(&key.as_str()));
                            }
                            config.sort_all_objects();
                            sha_hex(config.to_string().as_bytes())
                        }
                        Err(_) => sha_hex(&bytes),
                    },
                    Err(error) => format!("unreadable:{:?}", error.kind()),
                };
                observed.push(json!([name, fingerprint]));
                continue;
            }
            let mtime = meta.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map_or(0, |d| d.as_nanos());
            let relative = entry.path().strip_prefix(&path).unwrap_or(entry.path());
            observed.push(json!([name, relative.to_string_lossy(), meta.len(), mtime]));
        }
    }
    Value::Array(observed).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn open(dir: &Path) -> Registry {
        Registry::open(&dir.join("chat-plugins")).expect("registry opens")
    }

    #[test]
    fn default_visualizer_installs_once_and_respects_removal() {
        let dir = tempfile::tempdir().unwrap();
        let registry = open(dir.path());
        let record = registry.get(VISUALIZE).unwrap().expect("bundled visualizer installed");
        assert_eq!(registry.public(&record)["modes"], json!(["chat"]));
        assert!(registry.skill_rows("codex", false).unwrap().iter().any(|s| s.row["name"] == "muteki-visualize:visualize"));
        registry.uninstall(VISUALIZE).unwrap();
        drop(registry);
        let reopened = open(dir.path());
        assert!(reopened.get(VISUALIZE).unwrap().is_none(), "an explicit removal survives reopening");
        reopened.install_visualize().unwrap();
        assert!(reopened.get(VISUALIZE).unwrap().is_some());
    }

    #[test]
    fn mcp_package_lifecycle_changes_the_session_revision() {
        let dir = tempfile::tempdir().unwrap();
        let registry = open(dir.path());
        let before = registry.session_revision("codex").unwrap();
        let added = registry
            .add_mcp("demo-tools", &json!({"demo": {"url": "http://127.0.0.1:9/mcp"}}), None)
            .unwrap();
        assert_eq!(added["mcp_servers"], json!(["demo"]));
        assert_eq!(added["can_rollback"], false);
        assert_ne!(registry.session_revision("codex").unwrap(), before);
        let id = added["id"].as_str().unwrap().to_owned();
        registry.update(&id, Some(false), false, None, "", None).unwrap();
        assert_eq!(registry.session_revision("codex").unwrap(), before);
        let error = registry.update(&id, None, true, None, "", None).unwrap_err();
        assert_eq!(error.message, "没有可回滚的版本");
        registry.set_control(false).unwrap();
        assert_ne!(registry.session_revision("codex").unwrap(), before);
    }

    #[test]
    fn modes_must_be_known_and_nonempty() {
        assert!(Registry::validate_modes(&json!([])).is_err());
        assert!(Registry::validate_modes(&json!(["chat", "nope"])).is_err());
        assert_eq!(Registry::validate_modes(&json!(["pentest", "chat"])).unwrap(), vec!["chat", "pentest"]);
    }
}
