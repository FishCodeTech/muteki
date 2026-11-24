//! Live connections to package MCP servers and the tool catalog built from
//! them. The host owns every connection; engines reach these tools only
//! through the Muteki capability gateway.

use super::{PluginError, Registry, Result, inspect, mcp_client::Client, snapshot_id};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use regex::Regex;
use serde_json::{Value, json};
use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, Mutex, OnceLock},
    time::{Duration, Instant},
};
use tokio::sync::Mutex as AsyncMutex;

const DISCOVERY_DEADLINE: Duration = Duration::from_secs(12);
const RETRY_AFTER: Duration = Duration::from_secs(15);
const MCP_CIRCUIT_THRESHOLD: u32 = 3;
const MCP_CIRCUIT_COOLDOWN: Duration = Duration::from_secs(600);

type Identity = (String, String, String);
type Slot = Arc<AsyncMutex<Option<Arc<Client>>>>;

#[derive(Clone)]
struct Health {
    status: &'static str,
    cause_code: String,
    retry_after: Option<Instant>,
    observed_at: String,
}

#[derive(Clone)]
struct CircuitState {
    package_id: String,
    server: String,
    scope: String,
    config_hash: String,
    failures: u32,
    open_until: Option<Instant>,
    cause_code: String,
}

#[derive(Default)]
pub struct Runtime {
    workers: Mutex<HashMap<String, Slot>>,
    tools: Mutex<HashMap<String, Vec<Value>>>,
    errors: Mutex<HashMap<String, Vec<Value>>>,
    server_tools: Mutex<HashMap<Identity, Vec<Value>>>,
    health: Mutex<HashMap<Identity, Health>>,
    prepare: Mutex<HashMap<String, Arc<AsyncMutex<()>>>>,
    circuits: Mutex<HashMap<String, CircuitState>>,
}

impl Runtime {
    pub fn cached_tool_names(&self, key: &str) -> Vec<String> {
        let mut names = Vec::new();
        if let Some(entries) = self.tools.lock().unwrap().get(key) {
            for tool in entries {
                for field in ["name", "_tool"] {
                    if let Some(value) = tool[field].as_str().map(str::trim).filter(|s| !s.is_empty()) {
                        names.push(value.to_owned());
                    }
                }
            }
        }
        names
    }
}

fn sanitize(text: &str, pattern: &'static OnceLock<Regex>, expression: &str, limit: usize) -> String {
    let re = pattern.get_or_init(|| Regex::new(expression).unwrap());
    re.replace_all(text, "_").chars().take(limit).collect()
}

fn now() -> String {
    chrono::Utc::now().to_rfc3339()
}

fn worker_key(record: &Value, server: &str, scope: &str) -> String {
    let base = format!("{}:{}:{server}", record["id"].as_str().unwrap_or(""), snapshot_id(record));
    if scope.is_empty() { base } else { format!("{base}|{scope}") }
}

fn config_hash_short(config: &Value) -> String {
    let bytes = serde_json::to_vec(config).unwrap_or_default();
    let hex = super::sha_hex(&bytes);
    hex.chars().take(12).collect()
}

fn circuit_key(record: &Value, server: &str, config: &Value, scope: &str) -> (String, String) {
    let ch = config_hash_short(config);
    let wk = worker_key(record, server, scope);
    (format!("{wk}#{ch}"), ch)
}

/// The JSON-RPC method behind a resource or prompt helper tool.
fn rpc_method(helper: &str) -> Option<&'static str> {
    Some(match helper {
        "list_resources" => "resources/list",
        "list_resource_templates" => "resources/templates/list",
        "read_resource" => "resources/read",
        "list_prompts" => "prompts/list",
        "get_prompt" => "prompts/get",
        _ => return None,
    })
}

impl Runtime {
    fn slot(&self, key: &str) -> Slot {
        self.workers.lock().unwrap().entry(key.to_owned()).or_default().clone()
    }

    fn circuit_check(&self, ckey: &str, server: &str) -> Result<()> {
        let guard = self.circuits.lock().unwrap();
        if let Some(entry) = guard.get(ckey) {
            if let Some(until) = entry.open_until {
                let now_i = Instant::now();
                if until > now_i {
                    let rem = until.duration_since(now_i).as_secs();
                    return Err(PluginError::new(
                        "plugin.circuit_open",
                        format!("MCP server `{server}` circuit open for {rem}s after repeated failures ({})", entry.cause_code),
                    ));
                }
            }
        }
        Ok(())
    }

    fn circuit_record(
        &self,
        ckey: &str,
        package_id: &str,
        server: &str,
        scope: &str,
        config_hash: &str,
        ok: bool,
        cause_code: Option<&str>,
    ) {
        let mut guard = self.circuits.lock().unwrap();
        if ok {
            guard.remove(ckey);
        } else {
            let entry = guard.entry(ckey.to_owned()).or_insert_with(|| CircuitState {
                package_id: package_id.to_owned(),
                server: server.to_owned(),
                scope: scope.to_owned(),
                config_hash: config_hash.to_owned(),
                failures: 0,
                open_until: None,
                cause_code: String::new(),
            });
            entry.failures = entry.failures.saturating_add(1);
            entry.cause_code = cause_code.unwrap_or("mcp_call_failed").to_owned();
            if entry.failures >= MCP_CIRCUIT_THRESHOLD {
                entry.open_until = Some(Instant::now() + MCP_CIRCUIT_COOLDOWN);
            }
        }
    }

    pub fn circuit_snapshots(&self) -> Vec<Value> {
        let now_i = Instant::now();
        self.circuits
            .lock()
            .unwrap()
            .iter()
            .map(|(ckey, st)| {
                let remaining_secs = st
                    .open_until
                    .filter(|u| *u > now_i)
                    .map_or(0, |u| u.duration_since(now_i).as_secs());
                json!({
                    "circuit_key": ckey,
                    "package_id": st.package_id,
                    "server": st.server,
                    "scope": if st.scope.is_empty() { "chat" } else { st.scope.as_str() },
                    "config_hash": st.config_hash,
                    "failures": st.failures,
                    "state": if remaining_secs > 0 { "open" } else { "half_open" },
                    "remaining_secs": remaining_secs,
                    "cause_code": st.cause_code,
                })
            })
            .collect()
    }

    pub fn reset_circuits(&self, server_filter: Option<&str>) -> usize {
        let mut guard = self.circuits.lock().unwrap();
        let before = guard.len();
        if let Some(target) = server_filter.map(str::trim).filter(|v| !v.is_empty()) {
            guard.retain(|_, st| st.server != target && st.package_id != target);
        } else {
            guard.clear();
        }
        before.saturating_sub(guard.len())
    }

    /// The connection for one server, started if absent or dead. Chat keeps
    /// one connection per package version; Worker connections are scoped to
    /// one Run so mutable server state cannot cross engagements.
    async fn worker(&self, registry: &Registry, record: &Value, server: &str, config: &Value, scope: &str) -> Result<Arc<Client>> {
        let (ckey, _) = circuit_key(record, server, config, scope);
        self.circuit_check(&ckey, server)?;
        let key = worker_key(record, server, scope);
        let slot = self.slot(&key);
        let mut guard = slot.lock().await;
        if let Some(client) = guard.as_ref().filter(|c| c.is_alive()) {
            return Ok(client.clone());
        }
        let encoded = URL_SAFE_NO_PAD.encode(key.as_bytes());
        let state = encoded.as_bytes().chunks(100).fold(registry.root().join("mcp-state"), |path, chunk| {
            path.join(std::str::from_utf8(chunk).unwrap_or(""))
        });
        let package = std::path::PathBuf::from(record["root"].as_str().unwrap_or(""));
        let client = match Client::connect(config, &package, &state).await {
            Ok(c) => Arc::new(c),
            Err(err) => {
                return Err(err);
            }
        };
        *guard = Some(client.clone());
        Ok(client)
    }

    fn connected(&self, key: &str) -> bool {
        let slot = self.workers.lock().unwrap().get(key).cloned();
        slot.is_some_and(|slot| slot.try_lock().map_or(true, |guard| guard.as_ref().is_some_and(|c| c.is_alive())))
    }

    pub async fn invalidate(&self, active: &HashSet<String>) {
        self.tools.lock().unwrap().clear();
        self.errors.lock().unwrap().clear();
        self.server_tools.lock().unwrap().clear();
        self.health.lock().unwrap().clear();
        self.prepare.lock().unwrap().clear();
        self.workers.lock().unwrap().retain(|key, _| active.contains(key.split('|').next().unwrap_or("")));
    }

    pub async fn release_scope(&self, scope: &str) {
        if scope.is_empty() {
            return;
        }
        let owned = |key: &str| key.splitn(4, ':').nth(2) == Some(scope);
        self.tools.lock().unwrap().retain(|key, _| !owned(key));
        self.errors.lock().unwrap().retain(|key, _| !owned(key));
        self.prepare.lock().unwrap().retain(|key, _| !owned(key));
        self.server_tools.lock().unwrap().retain(|identity, _| !owned(&identity.0));
        self.health.lock().unwrap().retain(|identity, _| !owned(&identity.0));
        let suffix = format!("|{scope}");
        self.workers.lock().unwrap().retain(|key, _| !key.ends_with(&suffix));
    }

    pub async fn close(&self) {
        self.workers.lock().unwrap().clear();
    }
}

async fn discover(registry: &Registry, engine: &str, mode: &str, scope: &str, record: &Value, server: &str, config: &Value) -> Result<Vec<Value>> {
    let runtime = &registry.runtime;
    let client = runtime.worker(registry, record, server, config, if mode == "chat" { "" } else { scope }).await?;
    let id = record["id"].as_str().unwrap_or("");
    let name = record["name"].as_str().unwrap_or(id);
    let snapshot = snapshot_id(record);
    static LABEL: OnceLock<Regex> = OnceLock::new();
    static TOOL: OnceLock<Regex> = OnceLock::new();
    let label = sanitize(id, &LABEL, r"[^a-zA-Z0-9_]", 8);
    let server_label = sanitize(server, &LABEL, r"[^a-zA-Z0-9_]", 8);
    let engine_label = sanitize(engine, &LABEL, r"[^a-zA-Z0-9_]", 6);
    let prefix = format!(
        "{}{engine_label}_{label}_{server_label}_{}_",
        if mode == "chat" { "chat_" } else { "work_" },
        &snapshot[..snapshot.len().min(8)]
    );
    let mut tools = Vec::new();
    if client.supports("tools") {
        let listed = client.request("tools/list", json!({})).await?;
        for (index, tool) in listed["tools"].as_array().into_iter().flatten().enumerate() {
            let tool_name = tool["name"].as_str().unwrap_or("");
            let suffix = format!("{}_{}", sanitize(tool_name, &TOOL, r"[^a-zA-Z0-9_-]", 16), index);
            let description = tool["description"].as_str().filter(|d| !d.is_empty()).unwrap_or(tool_name);
            tools.push(json!({
                "name": format!("{prefix}{suffix}"),
                "description": format!("[Muteki {name} / {server}] {description}"),
                "input_schema": tool.get("inputSchema").cloned().unwrap_or_else(|| json!({"type": "object"})),
                "_package": id, "_server": server, "_tool": tool_name, "_snapshot_id": snapshot,
            }));
        }
    }
    for (capability, helpers) in [
        ("resources", &["list_resources", "list_resource_templates", "read_resource"][..]),
        ("prompts", &["list_prompts", "get_prompt"][..]),
    ] {
        if !client.supports(capability) {
            continue;
        }
        for helper in helpers {
            let schema = match *helper {
                "read_resource" => json!({"type": "object", "properties": {"uri": {"type": "string"}}, "additionalProperties": false, "required": ["uri"]}),
                "get_prompt" => json!({"type": "object", "properties": {"name": {"type": "string"}, "arguments": {"type": "object", "additionalProperties": {"type": "string"}}}, "additionalProperties": false, "required": ["name"]}),
                _ => json!({"type": "object", "properties": {"cursor": {"type": "string"}}, "additionalProperties": false}),
            };
            tools.push(json!({
                "name": format!("{prefix}{helper}"),
                "description": format!("[Muteki {name} / {server}] MCP {helper}"),
                "input_schema": schema, "_package": id, "_server": server,
                "_tool": helper, "_method": helper, "_snapshot_id": snapshot,
            }));
        }
    }
    Ok(tools)
}

/// The tool catalog for one engine, mode and scope. All servers are probed
/// concurrently under one deadline, so one broken endpoint cannot hold the
/// catalog; a failed server is retried on a later read after a pause.
pub async fn prepare_tools(registry: &Arc<Registry>, engine: &str, mode: &str, scope: &str) -> Result<Vec<Value>> {
    if mode == "chat" && !inspect::gateway_tools(engine) {
        return Ok(Vec::new());
    }
    let (revision, enabled) = {
        let registry = registry.clone();
        let (engine, mode) = (engine.to_owned(), mode.to_owned());
        tokio::task::spawn_blocking(move || Ok::<_, PluginError>((registry.revision(&engine, &mode)?, registry.enabled(&engine, &mode)?)))
            .await
            .map_err(|_| PluginError::new("chat_plugin.registry_unavailable", "扩展注册表不可用"))??
    };
    let key = format!("{mode}:{engine}:{scope}:{revision}");
    let runtime = &registry.runtime;
    let lock = runtime.prepare.lock().unwrap().entry(key.clone()).or_default().clone();
    let _held = lock.lock().await;
    let selected: Vec<(Value, String, Value)> = enabled
        .iter()
        .flat_map(|record| {
            record["mcp"]
                .as_object()
                .into_iter()
                .flatten()
                .filter(|(_, config)| config["disabled"] != true)
                .map(move |(server, config)| (record.clone(), server.clone(), config.clone()))
        })
        .collect();
    let mut server_tools: HashMap<Identity, Vec<Value>> = HashMap::new();
    let mut failures: Vec<Value> = Vec::new();
    let mut failed_packages: HashSet<String> = HashSet::new();
    let fail = |failures: &mut Vec<Value>, failed: &mut HashSet<String>, record: &Value, server: &str, code: &str| {
        let id = record["id"].as_str().unwrap_or("").to_owned();
        if mode == "chat" {
            registry.diagnostics.lock().unwrap().insert(id.clone(), format!("MCP {server} 连接失败，请检查配置与运行环境"));
        }
        failed.insert(id.clone());
        failures.push(json!({"package": id, "server": server, "code": "worker_mcp.connection_failed", "cause_code": code}));
    };
    let mut pending = Vec::new();
    let scope_key = if mode == "chat" { "" } else { scope };
    for (record, server, config) in &selected {
        let identity = (key.clone(), record["id"].as_str().unwrap_or("").to_owned(), server.clone());
        let (ckey, chash) = circuit_key(record, server, config, scope_key);
        if runtime.circuit_check(&ckey, server).is_err() {
            runtime.health.lock().unwrap().insert(
                identity,
                Health { status: "failed", cause_code: "plugin.circuit_open".to_owned(), retry_after: Some(Instant::now() + RETRY_AFTER), observed_at: now() },
            );
            fail(&mut failures, &mut failed_packages, record, server, "plugin.circuit_open");
            continue;
        }
        let health = runtime.health.lock().unwrap().get(&identity).cloned();
        let connected = runtime.connected(&worker_key(record, server, scope_key));
        match health {
            Some(h) if h.status == "connected" && connected => {
                let cached = runtime.server_tools.lock().unwrap().get(&identity).cloned().unwrap_or_default();
                server_tools.insert(identity, cached);
            }
            Some(h) if h.status == "failed" && h.retry_after.is_some_and(|at| Instant::now() < at) => {
                fail(&mut failures, &mut failed_packages, record, server, &h.cause_code);
            }
            _ => {
                let (registry, engine, mode, scope) = (registry.clone(), engine.to_owned(), mode.to_owned(), scope.to_owned());
                let (record, server, config) = (record.clone(), server.clone(), config.clone());
                let task = tokio::spawn(async move { discover(&registry, &engine, &mode, &scope, &record, &server, &config).await });
                pending.push((identity, ckey, chash, task));
            }
        }
    }
    let deadline = tokio::time::Instant::now() + DISCOVERY_DEADLINE;
    for (identity, ckey, chash, mut task) in pending {
        let record = selected.iter().find(|(r, s, _)| r["id"] == identity.1.as_str() && *s == identity.2).map(|(r, _, _)| r.clone()).unwrap_or_default();
        let outcome = tokio::time::timeout_at(deadline, &mut task).await;
        let result = match outcome {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(PluginError::new("worker_mcp.connection_failed", "MCP 服务启动失败")),
            Err(_) => {
                task.abort();
                Err(PluginError::new("worker_mcp.startup_timeout", "MCP 启动超时"))
            }
        };
        match result {
            Ok(tools) => {
                runtime.circuit_record(&ckey, &identity.1, &identity.2, scope_key, &chash, true, None);
                runtime.server_tools.lock().unwrap().insert(identity.clone(), tools.clone());
                runtime.health.lock().unwrap().insert(identity.clone(), Health { status: "connected", cause_code: String::new(), retry_after: None, observed_at: now() });
                server_tools.insert(identity, tools);
            }
            Err(error) => {
                tracing::warn!(package = %identity.1, server = %identity.2, code = %error.code, "Package MCP server unavailable");
                runtime.circuit_record(&ckey, &identity.1, &identity.2, scope_key, &chash, false, Some(&error.code));
                runtime.health.lock().unwrap().insert(
                    identity.clone(),
                    Health { status: "failed", cause_code: error.code.clone(), retry_after: Some(Instant::now() + RETRY_AFTER), observed_at: now() },
                );
                fail(&mut failures, &mut failed_packages, &record, &identity.2, &error.code);
            }
        }
    }
    let tools: Vec<Value> = selected
        .iter()
        .flat_map(|(record, server, _)| {
            server_tools.get(&(key.clone(), record["id"].as_str().unwrap_or("").to_owned(), server.clone())).cloned().unwrap_or_default()
        })
        .collect();
    runtime.tools.lock().unwrap().insert(key.clone(), tools.clone());
    runtime.errors.lock().unwrap().insert(key, failures);
    if mode == "chat" {
        let mut diagnostics = registry.diagnostics.lock().unwrap();
        for (record, _, _) in &selected {
            let id = record["id"].as_str().unwrap_or("");
            if !failed_packages.contains(id) {
                diagnostics.remove(id);
            }
        }
        let names: HashSet<String> = tools
            .iter()
            .flat_map(|tool| {
                let (name, inner, server) = (tool["name"].as_str().unwrap_or(""), tool["_tool"].as_str().unwrap_or(""), tool["_server"].as_str().unwrap_or(""));
                [name.to_owned(), inner.to_owned(), format!("mcp__{server}__{inner}"), format!("{server}.{inner}")]
            })
            .collect();
        registry.verified.lock().unwrap().insert(engine.to_owned(), names);
    }
    Ok(tools)
}

/// Call one catalog tool after checking the package still has the version
/// and scope the catalog was built from.
pub async fn invoke(registry: &Arc<Registry>, engine: &str, name: &str, arguments: Value, mode: &str, scope: &str) -> Result<Value> {
    let tools = prepare_tools(registry, engine, mode, scope).await?;
    let Some(tool) = tools.into_iter().find(|t| t["name"] == name) else {
        return Err(PluginError::new("chat.plugin.unavailable", "当前 Agent 未启用该工具，或插件已停用"));
    };
    let record = registry.get(tool["_package"].as_str().unwrap_or(""))?.unwrap_or(Value::Null);
    let current = record.is_object()
        && record["enabled"] != false
        && super::modes_of(&record).iter().any(|m| m == mode)
        && super::strings(&record["engines"]).iter().any(|e| e == engine)
        && snapshot_id(&record) == tool["_snapshot_id"];
    if !current {
        return Err(PluginError::new("chat.plugin.unavailable", "插件版本或使用范围已变化，请重新读取工具目录"));
    }
    let server = tool["_server"].as_str().unwrap_or("");
    let scope_key = if mode == "chat" { "" } else { scope };
    let cfg = &record["mcp"][server];
    let (ckey, chash) = circuit_key(&record, server, cfg, scope_key);
    let pkg_id = record["id"].as_str().unwrap_or("");
    let client = match registry.runtime.worker(registry, &record, server, cfg, scope_key).await {
        Ok(client) => client,
        Err(error) => {
            if error.code != "plugin.circuit_open" { registry.runtime.circuit_record(&ckey, pkg_id, server, scope_key, &chash, false, Some(&error.code)); }
            return Err(error);
        }
    };
    let res = match tool["_method"].as_str().and_then(rpc_method) {
        Some(method) => client.request(method, arguments).await,
        None => client.request("tools/call", json!({"name": tool["_tool"], "arguments": arguments})).await,
    };
    match &res {
        Ok(_) => registry.runtime.circuit_record(&ckey, pkg_id, server, scope_key, &chash, true, None),
        Err(err) => registry.runtime.circuit_record(&ckey, pkg_id, server, scope_key, &chash, false, Some(&err.code)),
    }
    res
}

/// Connect to every server of a package and count its tools.
pub async fn check(registry: &Arc<Registry>, record: &Value) -> Value {
    let mut count = 0usize;
    let mut diagnostics: Vec<String> = Vec::new();
    for (name, config) in record["mcp"].as_object().into_iter().flatten() {
        if config["disabled"] == true {
            continue;
        }
        let listed = async {
            let client = registry.runtime.worker(registry, record, name, config, "").await?;
            if client.supports("tools") {
                let result = client.request("tools/list", json!({})).await?;
                Ok::<_, PluginError>(result["tools"].as_array().map_or(0, Vec::len))
            } else {
                Ok(0)
            }
        }
        .await;
        match listed {
            Ok(found) => count += found,
            Err(_) => diagnostics.push(format!("{name} 连接失败，请检查配置与运行环境")),
        }
    }
    registry.invalidate().await;
    if !diagnostics.is_empty() {
        registry.diagnostics.lock().unwrap().insert(record["id"].as_str().unwrap_or("").to_owned(), diagnostics.join("；"));
    }
    json!({"tool_count": count, "diagnostics": diagnostics})
}

/// Observed connections. A configured server is not a live connection.
pub fn health(registry: &Registry, engine: &str, mode: &str, scope: &str) -> Result<Vec<Value>> {
    let key = format!("{mode}:{engine}:{scope}:{}", registry.revision(engine, mode)?);
    let runtime = &registry.runtime;
    let mut rows = Vec::new();
    for record in registry.enabled(engine, mode)? {
        for (server, config) in record["mcp"].as_object().into_iter().flatten() {
            if config["disabled"] == true {
                continue;
            }
            let id = record["id"].as_str().unwrap_or("").to_owned();
            let health = runtime.health.lock().unwrap().get(&(key.clone(), id.clone(), server.clone())).cloned();
            let mut status = health.as_ref().map_or("not_checked", |h| h.status);
            if status == "connected" && !runtime.connected(&worker_key(&record, server, if mode == "chat" { "" } else { scope })) {
                status = "disconnected";
            }
            rows.push(json!({"package_id": id, "server": server, "status": status, "observed_at": health.map(|h| h.observed_at)}));
        }
    }
    Ok(rows)
}
