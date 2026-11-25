//! Capability management: which platform capabilities a chat session may use,
//! the tool catalog the control gateway serves, and the per-thread bindings
//! and grants sessions are issued under.
//!
//! The enable policy is a small JSON file in the state directory, read on
//! every request so an operator edit takes effect without a restart. A
//! corrupt file disables everything and is reported as `policy_error`
//! instead of being silently replaced.

use crate::{AppState, agent_runtime::descriptors, store::Store};
use axum::{
    Json, Router,
    extract::{Path, State},
    http::StatusCode,
    routing::{get, post, put},
};
use serde_json::{Map, Value, json};
use std::{collections::HashMap, io::Write, sync::{Arc, Mutex}};

type Failure = (StatusCode, Json<Value>);
type Api = Result<Json<Value>, Failure>;

pub const BINDINGS: &str = "capability_bindings";
pub const GRANTS: &str = "capability_grants";
const POLICY_FILE: &str = "capability_management.json";
pub const CONTROL_SERVER: &str = "muteki-control";
pub const COLLAB_SERVER: &str = "muteki-collab";
pub const BROWSER_SERVER: &str = "muteki-browser";
pub const COMPUTER_SERVER: &str = "muteki-computer";
pub const COMPUTER_RESOURCE: &str = "computer-use";
pub const PLATFORM_SERVER: &str = "muteki-platform";
pub const PACKAGES_SERVER: &str = "muteki-packages";
const GRANT_TTL_SECONDS: i64 = 24 * 60 * 60;
static POLICY_WRITES: Mutex<()> = Mutex::new(());

fn failure(status: StatusCode, detail: Value) -> Failure {
    (status, Json(json!({"detail": detail})))
}

fn now() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

fn id(prefix: &str) -> String {
    format!("{prefix}-{}", uuid::Uuid::new_v4().simple())
}


/// Capabilities enabled unless the operator turned them off.
fn defaults() -> Value {
    json!({
        "mcp": {
            CONTROL_SERVER: true,
            COLLAB_SERVER: true,
            BROWSER_SERVER: true,
            COMPUTER_RESOURCE: true,
            PLATFORM_SERVER: true,
        },
        "skills": {
            "muteki-blackboard": true,
            "agent-browser": true,
            "show-skills-in-slash": true,
        }
    })
}

fn all_disabled() -> Value {
    let mut value = defaults();
    for resources in value.as_object_mut().unwrap().values_mut() {
        for enabled in resources.as_object_mut().unwrap().values_mut() {
            *enabled = json!(false);
        }
    }
    value
}

fn policy_path(s: &AppState) -> std::path::PathBuf {
    crate::native_chat::state_dir(s).join(POLICY_FILE)
}

/// The saved policy, or the defaults when no file exists. A file that is not
/// a JSON object leaves every capability disabled and carries the reason.
fn policy(s: &AppState) -> (Value, String) {
    let path = policy_path(s);
    match std::fs::read(&path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => (defaults(), String::new()),
        Err(error) => (json!({}), format!("无法读取能力策略文件 {}：{error}", path.display())),
        Ok(bytes) => match serde_json::from_slice::<Value>(&bytes) {
            Ok(Value::Object(saved)) => {
                let mut effective = defaults();
                for (kind, resources) in effective.as_object_mut().unwrap() {
                    for (resource, value) in resources.as_object_mut().unwrap() {
                        if let Some(enabled) = saved.get(kind).and_then(|v| v.get(resource)).and_then(Value::as_bool) {
                            *value = json!(enabled);
                        }
                    }
                }
                (effective, String::new())
            }
            Ok(_) | Err(_) => (json!({}), format!("能力策略文件 {} 不是 JSON 对象，已暂停全部可选能力", path.display())),
        },
    }
}

/// Whether `kind`/`id` is enabled. An unknown resource is not enabled.
pub fn enabled(s: &AppState, kind: &str, resource: &str) -> bool {
    let (saved, error) = policy(s);
    if !error.is_empty() {
        return false;
    }
    match saved[kind].get(resource) {
        Some(value) => value == &json!(true),
        None => defaults()[kind][resource] == json!(true),
    }
}

pub(crate) fn set_enabled(s: &AppState, kind: &str, resource: &str, value: bool) -> Result<(), Failure> {
    if defaults()[kind].get(resource).is_none() {
        return Err(failure(StatusCode::NOT_FOUND, json!(format!("未知能力资源 {kind}/{resource}"))));
    }
    let path = policy_path(s);
    let unavailable = |error: std::io::Error| {
        failure(StatusCode::SERVICE_UNAVAILABLE, json!(format!("无法写入能力策略文件：{error}")))
    };
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(unavailable)?;
    }
    let _guard = POLICY_WRITES.lock().map_err(|_| failure(
        StatusCode::SERVICE_UNAVAILABLE, json!("能力策略写入锁不可用"),
    ))?;
    let mut lock_options = std::fs::OpenOptions::new();
    lock_options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        lock_options.mode(0o600);
    }
    let lock_file = lock_options.open(path.with_extension("json.lock")).map_err(unavailable)?;
    fs2::FileExt::lock_exclusive(&lock_file).map_err(unavailable)?;
    let (saved, error) = policy(s);
    let mut merged = if error.is_empty() { saved } else { all_disabled() };
    merged[kind][resource] = json!(value);
    let parent = path.parent().ok_or_else(|| failure(
        StatusCode::SERVICE_UNAVAILABLE, json!("能力策略目录不可用"),
    ))?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent).map_err(unavailable)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        temporary.as_file().set_permissions(std::fs::Permissions::from_mode(0o600)).map_err(unavailable)?;
    }
    temporary.write_all(merged.to_string().as_bytes()).map_err(unavailable)?;
    temporary.as_file().sync_all().map_err(unavailable)?;
    temporary.persist(&path).map_err(|error| unavailable(error.error))?;
    #[cfg(unix)]
    std::fs::File::open(parent).and_then(|directory| directory.sync_all()).map_err(unavailable)?;
    Ok(())
}


pub struct ToolSpec {
    pub name: &'static str,
    description: &'static str,
    target_kind: &'static str,
    operation: &'static str,
    aggregate: &'static str,
}

impl ToolSpec {
    pub fn target_kind(&self) -> &'static str { self.target_kind }
    pub fn operation(&self) -> &'static str { self.operation }
}

/// A single data catalog owns source schemas, mappings, caller scopes and modes.
pub(crate) fn source_catalog() -> &'static Value {
    static CATALOG: std::sync::OnceLock<Value> = std::sync::OnceLock::new();
    CATALOG.get_or_init(||serde_json::from_str(include_str!("capability_catalog_source.json"))
        .expect("bundled capability catalog is valid JSON"))
}
pub(crate) fn source_tool(name: &str) -> Option<&'static Value> {
    source_catalog()["tools"].get(name)
}
pub static TOOLS: std::sync::LazyLock<Vec<ToolSpec>> = std::sync::LazyLock::new(|| {
    let mut tools: Vec<ToolSpec> = source_catalog()["tools"].as_object().unwrap().values().map(|row| ToolSpec {
        name:row["name"].as_str().unwrap(), description:row["description"].as_str().unwrap_or(""),
        target_kind:row["target_kind"].as_str().unwrap(),
        operation:row["command_type"].as_str().or_else(||row["query_type"].as_str()).unwrap_or(""),
        aggregate:row["aggregate_type"].as_str().unwrap_or(""),
    }).collect();
    for row in crate::control_tools::catalog() {
        let name = row["name"].as_str().unwrap();
        let description = row["description"].as_str().unwrap_or("");
        let target_kind = row["target_kind"].as_str().unwrap_or("query");
        let operation = row["operation"].as_str().unwrap_or("chat.control");
        let aggregate = row["aggregate"].as_str().unwrap_or("thread");
        if let Some(existing) = tools.iter_mut().find(|spec| spec.name == name) {
            existing.description = description;
            existing.target_kind = target_kind;
            existing.operation = operation;
            existing.aggregate = aggregate;
        } else {
            tools.push(ToolSpec {
                name,
                description,
                target_kind,
                operation,
                aggregate,
            });
        }
    }
    tools.extend([
        ToolSpec { name:"chat_computer_js",description:"通过当前聊天专属 JavaScript 会话操控本机 Mac；按不透明引用分页读取完整结果，不重复电脑动作。",target_kind:"query",operation:"chat.computer.js",aggregate:"thread" },
        ToolSpec { name:"chat_computer_js_reset",description:"重置当前聊天的本机电脑 JavaScript 会话。",target_kind:"query",operation:"chat.computer.js_reset",aggregate:"thread" },
    ]);
    tools.extend(crate::browser::catalog().iter().map(|row| ToolSpec {
        name: row["name"].as_str().unwrap(), description: row["description"].as_str().unwrap(),
        target_kind: "query", operation: "chat.browser", aggregate: "thread",
    }));
    tools
});
pub(crate) fn src_query_type(name: &str) -> Option<&'static str> {
    spec(name).filter(|tool|tool.target_kind == "query" && tool.operation.starts_with("src.")).map(|tool|tool.operation)
}
pub(crate) fn src_command_type(name: &str) -> Option<&'static str> {
    spec(name).filter(|tool|tool.target_kind == "command" && tool.operation.starts_with("src.")).map(|tool|tool.operation)
}

/// The tool's published schema is also enforced at the shared invocation boundary.
pub fn schema(name: &str) -> Value {
    if let Some(tool) = crate::control_tools::catalog().iter().find(|tool| tool["name"] == name) {
        return tool["input_schema"].clone();
    }
    if let Some(tool) = crate::browser::catalog().iter().find(|tool| tool["name"] == name) {
        return tool["input_schema"].clone();
    }
    if name == "muteki_list_threads"
        && let Some(tool) = source_tool(name)
    {
        let mut published = tool["input_schema"].clone();
        if let Some(props) = published["properties"].as_object_mut() {
            props.insert("status".into(), json!({"type":"string","description":"可选：按运行状态筛选（idle / running / awaiting_approval / awaiting_user_input）"}));
            props.insert("archived".into(), json!({"type":"boolean","description":"可选：筛选归档状态"}));
            props.insert("settled".into(), json!({"type":"boolean","description":"可选：筛选结案（settled）状态"}));
            props.insert("pinned".into(), json!({"type":"boolean","description":"可选：筛选置顶会话"}));
            props.insert("follow_up".into(), json!({"type":"boolean","description":"可选：筛选待跟进（follow_up）会话"}));
            props.insert("subagent".into(), json!({"type":"boolean","description":"可选：筛选子代理（true）或主会话（false）"}));
        }
        return published;
    }
    if let Some(tool) = source_tool(name) {
        let published = &tool["input_schema"];
        if let Some(operation) = tool["query_type"].as_str().or_else(|| tool["command_type"].as_str())
            .filter(|operation| operation.starts_with("src.")) {
            static SRC_CONTRACTS: std::sync::OnceLock<Value> = std::sync::OnceLock::new();
            let contracts = SRC_CONTRACTS.get_or_init(||
                serde_json::from_str(include_str!("runs/src_api/operation_contracts.json"))
                    .expect("bundled SRC argument contracts are valid JSON"));
            if let Some(contract) = contracts.get(operation) {
                let mut schema = contract.clone();
                if let Some(object) = schema.as_object_mut() {
                    let aliases = object.remove("binary_aliases").unwrap_or_else(|| json!({}));
                    let auto_payload = object.remove("auto_payload") == Some(json!(true));
                    object.remove("source");
                    if auto_payload {
                        object.insert("additionalProperties".into(), json!(true));
                        if let Some(payload) = object
                            .get_mut("properties")
                            .and_then(|properties| properties.get_mut("payload"))
                            .and_then(Value::as_object_mut)
                            .filter(|payload| !payload.contains_key("description"))
                        {
                            payload.insert("description".into(),
                                json!("领域业务载荷；等价业务字段也可直接作为顶层参数传入，服务端校验归入 payload"));
                        }
                    }
                    let required = object.get("required").and_then(Value::as_array).cloned().unwrap_or_default();
                    let mut alias_clauses = Vec::new();
                    for canonical in &required {
                        let Some(canonical) = canonical.as_str() else { continue };
                        let alias_names: Vec<_> = aliases.as_object().into_iter().flat_map(|rows| rows.iter())
                            .filter_map(|(alias, target)| (target.as_str() == Some(canonical)).then_some(alias.clone()))
                            .collect();
                        if !alias_names.is_empty() {
                            let mut choices = vec![json!({"required":[canonical]})];
                            choices.extend(alias_names.iter().map(|alias| json!({"required":[alias]})));
                            alias_clauses.push(json!({"anyOf":choices}));
                        }
                    }
                    if let Some(rows) = object.get_mut("required").and_then(Value::as_array_mut) {
                        rows.retain(|name| name.as_str().is_none_or(|name| {
                            !(auto_payload && name == "payload") &&
                            !aliases.as_object().is_some_and(|aliases| aliases.values().any(|target| target == name))
                        }));
                    }
                    if !alias_clauses.is_empty() {
                        object.entry("allOf").or_insert_with(|| json!([])).as_array_mut()
                            .expect("SRC allOf is an array").extend(alias_clauses);
                    }
                    if tool["command_type"].is_string() {
                        object.entry("properties").or_insert_with(|| json!({}))["idempotency_key"] =
                            published["properties"]["idempotency_key"].as_object()
                                .map(|_| published["properties"]["idempotency_key"].clone())
                                .unwrap_or_else(|| json!({"type":"string","description":"仅在完整重试同一请求时复用同一键去重；内容不同的请求必须使用新键或省略，否则返回 409 冲突"}));
                        object.get_mut("properties").expect("SRC command properties exist")["actor_id"] =
                            json!({"type":"string","description":"可选；声明的执行者必须与可信会话执行者一致"});
                    }
                }
                return schema;
            }
        }
        return published.clone();
    }
    match name {
        "chat_computer_js" => json!({"type":"object","properties":{
            "code":{"type":"string"},"completion_summary":{"type":"string","description":"A short headline in three to six words, starting with a past-tense verb; no code or detailed explanation."},"title":{"type":"string"},"timeout_ms":{"type":"integer","minimum":1},
            "read_result":{"type":"object","properties":{"ref":{"type":"string"},"page":{"type":"integer","minimum":0}},
                "required":["ref","page"],"additionalProperties":false}},"additionalProperties":false}),
        "chat_computer_js_reset" => json!({"type":"object","properties":{},"additionalProperties":false}),
        _ => json!({"type":"object","additionalProperties":false}),
    }
}

pub(crate) fn validate_arguments(name: &str, arguments: &Value) -> Result<(),Value> {
    type Validators = HashMap<String,Arc<jsonschema::Validator>>;
    static VALIDATORS: std::sync::OnceLock<Mutex<Validators>> = std::sync::OnceLock::new();
    let mut validators = VALIDATORS.get_or_init(||Mutex::new(HashMap::new())).lock()
        .map_err(|_|json!({"code":"capability.schema_unavailable","category":"internal","message":"Tool schema cache is unavailable"}))?;
    let validator = match validators.get(name) {
        Some(validator) => validator.clone(),
        None => {
            let schema = schema(name);
            let validator = Arc::new(jsonschema::draft202012::options().build(&schema)
                .map_err(|error|json!({"code":"capability.schema_invalid","category":"internal","message":error.to_string()}))?);
            validators.insert(name.to_owned(),validator.clone());
            validator
        },
    };
    drop(validators);
    let errors: Vec<Value> = validator.iter_errors(arguments).map(|error|json!({"path":error.instance_path.to_string(),"message":error.to_string()})).collect();
    if errors.is_empty() { Ok(()) } else { Err(json!({"schema_version":1,"code":"capability.arguments_invalid","category":"validation",
        "message":"Tool arguments do not match the published schema","recovery_hint":"Use the current tool schema to correct the request.",
        "correlation_id":null,"retryable":false,"detail":{"validation_errors":errors}})) }
}

pub(crate) fn mode_tools(mode: &str) -> Vec<String> {
    let mut tools: Vec<String> = source_catalog()["modes"][mode].as_array().into_iter().flatten()
        .filter_map(Value::as_str).filter(|name|spec(name).is_some()).map(str::to_owned).collect();
    if mode == "conversation" {
        tools.retain(|name| !is_collab_tool(name));
        for name in crate::control_tools::all_conversation_control_names() {
            if !tools.iter().any(|existing| existing == name) {
                tools.push(name.to_owned());
            }
        }
        tools.extend(crate::browser::NAMES.iter().map(|name| (*name).to_owned()));
        tools.extend(["chat_computer_js".to_owned(),"chat_computer_js_reset".to_owned()]);
    } else if mode == "src" {
        for name in crate::browser::NAMES
            .iter()
            .copied()
            .chain(["chat_computer_js", "chat_computer_js_reset"])
        {
            if !tools.iter().any(|existing| existing == name) {
                tools.push(name.to_owned());
            }
        }
    }
    tools
}

pub fn spec(name: &str) -> Option<&'static ToolSpec> {
    TOOLS.iter().find(|tool| tool.name == name)
}

pub fn tool_names() -> Vec<String> { mode_tools("conversation") }

pub fn is_collab_tool(name: &str) -> bool {
    crate::control_tools::handles(name)
        || matches!(
            name,
            "muteki_list_projects"
                | "muteki_create_project"
                | "muteki_update_project"
                | "muteki_list_threads"
                | "muteki_get_thread"
                | "muteki_read_thread_messages"
                | "muteki_search_threads"
                | "muteki_create_thread"
                | "muteki_rename_thread"
                | "muteki_archive_thread"
                | "muteki_unarchive_thread"
                | "muteki_get_command_receipt"
                | "muteki_list_providers"
                | "muteki_start_thread"
                | "muteki_send_message"
                | "muteki_get_subagents"
                | "muteki_cancel_thread"
                | "muteki_request_grant"
                | "muteki_notify_when"
                | "muteki_cancel_notify"
        )
}

pub fn is_platform_tool(name: &str) -> bool {
    !is_collab_tool(name)
        && !crate::browser::handles(name)
        && !crate::chat_computer::handles(name)
        && name != crate::chat_computer::STATUS_TOOL
        && spec(name).is_some()
}

fn src_platform_subdomain(op: &str) -> &'static str {
    if op.starts_with("src.policy.")
        || op.starts_with("src.task.authorization.")
        || op.starts_with("src.research.authorization.")
        || matches!(op, "src.task.pause" | "src.task.resume")
        || op.starts_with("src.task.scope.")
        || op.starts_with("src.grant.")
    {
        "src-auth"
    } else if op.starts_with("src.asset.")
        || op.starts_with("src.workbench.asset.")
        || op.starts_with("src.coverage.")
        || op.starts_with("src.surface.")
        || op.starts_with("src.inventory.")
    {
        "src-assets"
    } else if op.starts_with("src.finding.")
        || op.starts_with("src.workbench.finding.")
        || op.starts_with("src.workbench.duplicate.")
        || op.starts_with("src.retest.")
        || op.starts_with("src.object.")
        || op.starts_with("src.differential.")
    {
        "src-findings"
    } else if op.starts_with("src.report.")
        || op.starts_with("src.workbench.report.")
        || op.starts_with("src.evidence.")
        || op.starts_with("src.workbench.evidence.")
        || op.starts_with("src.artifact.")
    {
        "src-reports"
    } else if op.starts_with("src.exchange.")
        || op.starts_with("src.traffic.")
        || op.starts_with("src.stream.")
        || op.starts_with("src.oast.")
        || op.starts_with("src.proxy_bridge.")
        || op.starts_with("src.proxypool.")
        || op.starts_with("src.browser.")
        || op.starts_with("src.mutator.")
        || op.starts_with("src.ingestion.")
        || op.starts_with("src.inspection.")
    {
        "src-traffic"
    } else if op.starts_with("src.connector.") || op.starts_with("src.intel.") {
        "src-connectors"
    } else if op.starts_with("src.monitor.") {
        "src-monitors"
    } else if op.starts_with("src.identity.")
        || op.starts_with("src.login_recipe.")
        || op.starts_with("src.credential.")
    {
        "src-identity"
    } else if op.starts_with("src.research.")
        || op.starts_with("src.knowledge.")
        || op.starts_with("src.note.")
        || op.starts_with("src.project.memory.")
        || op.starts_with("src.project_memory.")
        || op.starts_with("src.javascript.")
        || op.starts_with("src.sourcemap.")
        || op.starts_with("src.graphql_grpc.")
    {
        "src-research"
    } else {
        "src-workbench"
    }
}

/// Sub-domain classification inside `muteki-platform` for progressive disclosure discovery.
pub fn platform_domain(name: &str) -> &'static str {
    if let Some(op) = src_query_type(name).or_else(|| src_command_type(name)) {
        return src_platform_subdomain(op);
    }
    if matches!(
        name,
        "muteki_list_credentials"
            | "muteki_upsert_credential_account"
            | "muteki_delete_credential_account"
            | "muteki_upsert_model_endpoint"
            | "muteki_delete_model_endpoint"
            | "muteki_refresh_credential_models"
            | "muteki_test_credential_account"
            | "muteki_get_local_pi"
            | "muteki_update_local_pi"
            | "muteki_get_usage_and_quota"
            | "muteki_refresh_quota"
            | "muteki_get_pricing"
            | "muteki_update_pricing"
    ) {
        "models"
    } else if matches!(
        name,
        "muteki_list_runtime_instances"
            | "muteki_list_runtime_descriptors"
            | "muteki_manage_runtime_instance"
    ) {
        "runtimes"
    } else if matches!(
        name,
        "muteki_get_capabilities_overview"
            | "muteki_update_capability"
            | "muteki_list_chat_plugins"
            | "muteki_manage_chat_plugin"
            | "muteki_list_platform_extensions"
            | "muteki_manage_extension"
    ) {
        "extensions"
    } else if matches!(
        name,
        "muteki_get_ui_settings"
            | "muteki_update_ui_settings"
            | "muteki_get_control_state"
            | "muteki_scan_provider_history"
            | "muteki_import_provider_history"
    ) {
        "settings"
    } else if matches!(
        name,
        "muteki_list_workspaces"
            | "muteki_manage_workspace"
            | "muteki_manage_folder"
            | "muteki_get_thread_memory"
            | "muteki_manage_thread_memory"
            | "muteki_list_conversation_shares"
            | "muteki_manage_conversation_share"
            | "muteki_get_turn_process"
    ) {
        "workspace"
    } else if matches!(
        name,
        "muteki_get_operations_overview"
            | "muteki_manage_platform_effect"
            | "muteki_maintenance"
            | "muteki_manage_domain_module"
            | "muteki_get_runner_resources"
            | "muteki_update_runner_resources"
            | "muteki_get_system_status"
            | "muteki_check_system_update"
    ) {
        "operations"
    } else if name == "muteki_dispatch_pentest" {
        "pentest"
    } else if name.contains("competition")
        || name.contains("challenge_instance")
        || matches!(
            name,
            "muteki_list_connections"
                | "muteki_select_challenge"
                | "muteki_queue_challenge"
                | "muteki_skip_challenge"
                | "muteki_start_scheduler"
                | "muteki_pause_scheduler"
                | "muteki_resume_scheduler"
        )
    {
        "competitions"
    } else if name.contains("worker")
        || name.contains("credential")
        || name.contains("host_account")
        || name.contains("openvpn")
        || matches!(
            name,
            "muteki_test_llm_endpoint"
                | "muteki_get_run_retention"
                | "muteki_update_run_retention"
        )
    {
        "workers"
    } else {
        "runs"
    }
}

pub fn collab_enabled(s: &AppState) -> bool {
    enabled(s, "mcp", COLLAB_SERVER) && enabled(s, "mcp", CONTROL_SERVER)
}

pub fn collab_enabled_for_thread(s: &AppState, thread_id: &str) -> bool {
    collab_enabled(s) || thread_mode(&s.store, thread_id) == "src"
}

pub fn browser_enabled(s: &AppState) -> bool {
    enabled(s, "mcp", BROWSER_SERVER)
}

pub fn computer_enabled(s: &AppState) -> bool {
    enabled(s, "mcp", COMPUTER_RESOURCE)
}

pub fn platform_always_on(s: &AppState) -> bool {
    enabled(s, "mcp", PLATFORM_SERVER)
}

pub fn thread_platform_awake(store: &Store, thread_id: &str) -> bool {
    if thread_id.is_empty() || thread_id.starts_with("prewarm:") {
        return false;
    }
    store
        .get("control_threads", thread_id)
        .ok()
        .flatten()
        .is_some_and(|row| row["platform_awake"] == true)
}

pub fn wake_thread_platform(store: &Store, thread_id: &str) -> Result<bool, crate::chat::Failure> {
    if thread_id.is_empty() || thread_id.starts_with("prewarm:") {
        return Ok(false);
    }
    let unavailable = |error| crate::chat::fail_code(
        StatusCode::INTERNAL_SERVER_ERROR,
        "capability.platform_wake.storage_failed",
        &format!("平台唤醒状态存储失败：{error}"),
    );
    let mut row = store.get("control_threads", thread_id)
        .map_err(unavailable)?.unwrap_or_else(|| json!({}));
    if row["platform_awake"] == true {
        return Ok(false);
    }
    row["platform_awake"] = json!(true);
    store.put("control_threads", thread_id, &row).map_err(unavailable)?;
    Ok(true)
}

pub fn platform_enabled_for_thread(s: &AppState, thread_id: &str) -> bool {
    platform_always_on(s) || thread_platform_awake(&s.store, thread_id)
}

fn src_task_principal(store: &Store, thread_id: &str) -> Result<Option<String>, Failure> {
    match crate::runs::src_api::resolve_authorized_src_caller_from_store(store, thread_id) {
        Ok(Some(caller)) => return Ok(Some(caller.principal_id)),
        Ok(None) => {}
        Err(error) if error.status.is_server_error() => {
            return Err(failure(error.status, json!(error.message)));
        }
        Err(_) => return Ok(None),
    }
    if store.get("conversation_threads", thread_id).map_err(storage)?.is_some() {
        return Ok(None);
    }
    let mut matches = store.list("src_tasks").map_err(storage)?.into_iter()
        .filter(|task| task["lifecycle"]["state"].as_str() != Some("destroyed") && task["binding"]["thread_id"] == thread_id);
    let first = matches.next();
    if matches.next().is_some() { return Ok(None); }
    Ok(first.and_then(|task| task["binding"]["principal_id"].as_str().map(str::to_owned)))
}

pub(crate) fn thread_mode(store: &Store, thread_id: &str) -> String {
    if src_task_principal(store,thread_id).ok().flatten().is_some() { return "src".to_owned(); }
    store.get("conversation_threads",thread_id).ok().flatten()
        .and_then(|row|row["thread"]["mode"].as_str().map(str::to_owned))
        .filter(|mode| source_catalog()["modes"].get(mode).is_some())
        .unwrap_or_else(||"conversation".to_owned())
}

pub(crate) fn is_src_inline_worker_only_tool(name: &str) -> bool {
    matches!(
        name,
        "muteki_exchange_record"
            | "muteki_exchange_draft_save"
            | "muteki_exchange_draft_execute"
            | "muteki_src_differential_probe"
            | "muteki_src_auto_reproduce"
            | "muteki_report_reproduction_auto"
            | "muteki_src_collect_javascript"
            | "muteki_research_sourcemap_unpack"
            | "muteki_mutator_test"
            | "muteki_connector_query"
            | "muteki_connector_repository"
            | "muteki_identity_verify"
            | "muteki_identity_recover"
            | "muteki_identity_session_probe"
            | "muteki_object_lease_acquire"
            | "muteki_object_lease_release"
            | "muteki_object_experiment_begin"
            | "muteki_object_experiment_after"
            | "muteki_object_experiment_restore"
            | "muteki_oast_allocate"
            | "muteki_oast_poll"
            | "muteki_oast_expire"
            | "muteki_oast_local_start"
            | "muteki_oast_token_mint"
            | "muteki_exchange_race_probe"
            | "muteki_research_graphql_grpc_ingest"
            | "muteki_identity_session_import"
    ) || src_command_type(name).is_some_and(|cmd| {
            cmd.starts_with("src.browser.") || cmd.starts_with("src.stream.")
        })
        || crate::browser::handles(name)
        || crate::chat_computer::handles(name)
}

fn is_src_readonly_analysis_tool(name: &str) -> bool {
    if matches!(
        name,
        "muteki_workbench_suggestion_propose"
            | "muteki_src_todo_save"
            | "muteki_outline"
            | "muteki_read"
            | crate::control_tools::REPORT_TOOL
    ) {
        return true;
    }
    src_query_type(name).is_some_and(|qt| {
        !qt.starts_with("src.browser.")
            && !qt.starts_with("src.run.")
            && !qt.starts_with("src.policy.")
            && qt != "src.credential.list"
            && qt != "src.diagnostics.mcp_reset"
    })
}

fn delegated_src_tool_names(delegation: &Value) -> Vec<String> {
    let profile = delegation["profile"].as_str().unwrap_or("readonly_analysis");
    let custom: Vec<String> = delegation["allowed_tools"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_owned)
        .collect();
    let base = mode_tools("src");
    let mut tools = Vec::new();
    let mut push_unique = |name: String| {
        if !tools.iter().any(|existing| existing == &name) {
            tools.push(name);
        }
    };
    if profile == "scoped_execution" {
        for name in base {
            if matches!(
                name.as_str(),
                "muteki_start_thread"
                    | "muteki_send_message"
                    | "muteki_control_thread"
                    | "muteki_cancel_thread"
                    | "muteki_update_threads"
                    | "muteki_src_project_memory_save"
                    | "muteki_ensure_src_run"
                    | "muteki_start_src_run"
                    | "muteki_src_run_intervene"
                    | "muteki_src_coordination_decide"
                    | "muteki_ack_src_coordination_notice"
                    | "muteki_update_src_policy"
                    | "muteki_renew_src_authorization"
                    | "muteki_update_src_coordination_policy"
                    | "muteki_pause_src_task"
                    | "muteki_resume_src_task"
                    | "muteki_notify_when"
                    | "muteki_cancel_notify"
            ) {
                continue;
            }
            if !custom.is_empty() && !custom.iter().any(|allowed| allowed == &name) {
                continue;
            }
            push_unique(name);
        }
    } else {
        for name in base {
            if !is_src_readonly_analysis_tool(&name) {
                continue;
            }
            if !custom.is_empty() && !custom.iter().any(|allowed| allowed == &name) {
                continue;
            }
            push_unique(name);
        }
        for name in ["muteki_workbench_suggestion_propose", "muteki_src_todo_save"] {
            if custom.is_empty() || custom.iter().any(|allowed| allowed == name) {
                push_unique(name.to_owned());
            }
        }
    }
    for required in [crate::control_tools::REPORT_TOOL, "muteki_outline", "muteki_read"] {
        push_unique(required.to_owned());
    }
    tools
}

fn thread_tool_names(store: &Store, thread_id: &str) -> Vec<String> {
    let mut is_src_coordinator = false;
    if !thread_id.is_empty() && !thread_id.starts_with("prewarm:") {
        match crate::runs::src_api::resolve_authorized_src_caller_from_store(store, thread_id) {
            Ok(Some(caller)) => {
                if let Some(delegation) = caller.delegation {
                    return delegated_src_tool_names(&delegation);
                }
                is_src_coordinator = true;
            }
            Err(_) => return Vec::new(),
            Ok(None) => {}
        }
    }
    let mode = thread_mode(store, thread_id);
    if !matches!(mode.as_str(), "conversation" | "src")
        || thread_id.is_empty()
        || thread_id.starts_with("prewarm:")
    {
        return mode_tools(&mode);
    }
    let mut tools = mode_tools(&mode);
    if is_src_coordinator {
        tools.retain(|name| !is_src_inline_worker_only_tool(name));
    }
    if store
        .get("conversation_threads", thread_id)
        .ok()
        .flatten()
        .is_some_and(|doc| doc["thread"]["state"]["lineage"]["origin"] == "app_owned")
        && !tools.iter().any(|name| name == crate::control_tools::REPORT_TOOL)
    {
        tools.push(crate::control_tools::REPORT_TOOL.to_owned());
    }
    tools
}

fn thread_principal(store: &Store, thread_id: &str) -> Result<String, Failure> {
    Ok(src_task_principal(store, thread_id)?.unwrap_or_else(|| "system".to_owned()))
}

/// The catalog rows the capability page lists, and the descriptions the
/// gateway publishes.
pub fn catalog() -> Vec<Value> {
    TOOLS
        .iter()
        .map(|tool| {
            json!({
                "name": tool.name, "description": tool.description, "target_kind": tool.target_kind,
                "command_type": if tool.target_kind == "command" { json!(tool.operation) } else { Value::Null },
                "query_type": if tool.target_kind == "query" { json!(tool.operation) } else { Value::Null },
                "aggregate_type": tool.aggregate, "source": "builtin", "default_modes":
                    json!(["conversation","single_task","competition","management","src"].into_iter()
                        .filter(|mode|mode_tools(mode).iter().any(|name|name == tool.name)).collect::<Vec<_>>()),
                "input_schema": schema(tool.name),
            })
        })
        .collect()
}


fn binding_key(thread_id: &str, principal_id: &str) -> String {
    format!("{thread_id}\u{0}{principal_id}")
}

fn storage(error: impl std::fmt::Display) -> Failure {
    failure(StatusCode::INTERNAL_SERVER_ERROR, json!(format!("能力绑定存储不可用：{error}")))
}

fn versions(store: &Store, thread_id: &str, principal_id: &str) -> Result<Vec<Value>, Failure> {
    let row = store.get(BINDINGS, &binding_key(thread_id, principal_id)).map_err(storage)?;
    Ok(row.map(|row| row["versions"].as_array().cloned().unwrap_or_default()).unwrap_or_default())
}


fn active(rows: &[Value]) -> Option<Value> {
    rows.iter().filter(|row| row["revoked_at"].is_null()).next_back().cloned()
}

#[allow(dead_code)]
fn follows_mode_template(binding: &Value, rows: &[Value], defaults: &[String]) -> bool {
    if binding["selection_source"] == "mode_template" { return true; }
    if !binding["selection_source"].is_null() || rows.len() != 1 || binding["binding_version"] != 1 { return false; }
    let mut saved: Vec<&str> = binding["tool_set"].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
    let mut current: Vec<&str> = defaults.iter().map(String::as_str).collect();
    saved.sort_unstable(); current.sort_unstable();
    let added = |name: &str| crate::browser::handles(name) || crate::control_tools::handles(name);
    saved == current
        || saved.into_iter().filter(|name| !added(name)).collect::<Vec<_>>()
            == current.into_iter().filter(|name| !added(name)).collect::<Vec<_>>()
}

/// The tools a session for this thread is issued directly from the live thread mode
/// and delegation profile (never locked behind stale SQLite capability bindings).
pub fn effective_tool_set(store: &Store, thread_id: &str) -> Vec<String> {
    thread_tool_names(store, thread_id)
}

/// Stable revision of a thread's bound tool set. Auto-creating the default
/// version-1 binding in `issue_grant` preserves the template revision so the
/// token issued immediately before `issue_grant` stays valid.
pub fn thread_binding_revision(store: &Store, thread_id: &str) -> String {
    if thread_id.is_empty() || thread_id.starts_with("prewarm:") {
        return json!([0, tool_names()]).to_string();
    }
    let principal = thread_principal(store, thread_id).unwrap_or_else(|_| "system".to_owned());
    let rows = versions(store, thread_id, &principal).unwrap_or_default();
    let default_tools = thread_tool_names(store, thread_id);
    let tools = effective_tool_set(store, thread_id);
    let version = active(&rows)
        .filter(|_| tools != default_tools || rows.len() > 1)
        .and_then(|b| b["binding_version"].as_u64())
        .unwrap_or(0);
    json!([version, tools]).to_string()
}

/// Combined capability revision for a thread and adapter: includes MCP policy
/// switches, thread tool binding revision, and managed plugin session revision.
pub fn session_capability_revision(s: &AppState, thread_id: &str, adapter_id: &str) -> String {
    let engine = descriptors::for_adapter(adapter_id)
        .map(|(e, _)| e.engine().to_owned())
        .unwrap_or_else(|| adapter_id.to_owned());
    let plugin_rev = crate::plugins::registry(s)
        .ok()
        .and_then(|r| r.session_revision(&engine).ok())
        .unwrap_or_default();
    let collab_on = collab_enabled_for_thread(s, thread_id);
    let browser_on = browser_enabled(s);
    let computer_on = computer_enabled(s);
    let platform_on = platform_enabled_for_thread(s, thread_id);
    let slash_skills = enabled(s, "skills", "show-skills-in-slash");
    let binding_rev = thread_binding_revision(&s.store, thread_id);
    let orch_rev = if collab_on {
        crate::control::effective_orchestration(s, thread_id)
            .map(|resolved| {
                format!(
                    "{}:{:?}:{}",
                    resolved.mode,
                    resolved.source,
                    resolved.profile.digest()
                )
            })
            .unwrap_or_default()
    } else {
        String::new()
    };
    json!([
        collab_on,
        browser_on,
        computer_on,
        platform_on,
        slash_skills,
        binding_rev,
        plugin_rev,
        orch_rev
    ])
    .to_string()
}

fn resource_scopes(mode: &str, thread_id: &str, tools: &[String]) -> Vec<String> {
    let own_thread = if thread_id.is_empty(){"thread:*".to_owned()}else{format!("thread:{thread_id}")};
    let mut scopes: Vec<String> = match mode {
        "conversation" => ["thread:*","task:*","project:*","run:*","competition:*","connection:*","worker_settings:*","runtime:*","profile:*","credential:*"].map(str::to_owned).to_vec(),
        "management" => vec!["*:*".to_owned()],
        "competition" => vec![own_thread,"task:*".into(),"project:*".into(),"run:*".into(),"competition:*".into(),"connection:*".into()],
        _ => vec![own_thread,"task:*".into(),"project:*".into(),"run:*".into()],
    };
    for name in tools {
        if source_tool(name).is_some_and(|tool|tool["aggregate_from_caller_thread"] == true) { continue; }
        if let Some(tool) = spec(name).filter(|tool|!tool.aggregate.is_empty()) {
            let scope = format!("{}:*",tool.aggregate);
            if !scopes.contains(&scope) && !scopes.iter().any(|scope|scope == "*:*") { scopes.push(scope); }
        }
    }
    scopes
}

fn binding_record(thread_id: &str, principal_id: &str, version: u64, tools: &[String], mode: &str) -> Value {
    let commands: Vec<&str> =
        tools.iter().filter_map(|name| spec(name)).filter(|t| t.target_kind == "command").map(|t| t.operation).collect();
    let queries: Vec<&str> =
        tools.iter().filter_map(|name| spec(name)).filter(|t| t.target_kind == "query").map(|t| t.operation).collect();
    json!({
        "binding_id": id("bind"), "binding_version": version, "thread_id": thread_id,
        "principal_id": principal_id, "mode": mode, "tool_set": tools,
        "allowed_commands": commands, "allowed_queries": queries,
        "resource_scopes": resource_scopes(mode,thread_id,tools), "policy_version": 1,
        "created_at": now(), "revoked_at": Value::Null,
    })
}

/// A grant is the gateway credential one engine session holds. The process
/// that issued it owns the session, so a grant from an earlier epoch can no
/// longer be presented and is reported as expired.
fn grant_state(grant: &Value, epoch: &str) -> &'static str {
    if !grant["revoked_at"].is_null() {
        "revoked"
    } else if grant["epoch"] != epoch {
        "expired"
    } else if grant["expires_at"].as_str().and_then(|at| chrono::DateTime::parse_from_rfc3339(at).ok())
        .is_none_or(|at| at <= chrono::Utc::now()) {
        "expired"
    } else {
        "active"
    }
}

fn grant_view(grant: &Value, epoch: &str) -> Value {
    let mut view = grant.clone();
    if let Some(object) = view.as_object_mut() {
        object.remove("epoch");
        object.remove("token_digest");
    }
    view["state"] = json!(grant_state(grant, epoch));
    view
}

/// Return the live session grant and tool binding without SQLite lease/TTL expiry blocks.
pub fn issue_grant(s: &AppState, thread_id: &str, session: &Value, digest: &str) -> Result<Value, Failure> {
    let session_id = session["agent_session_id"]
        .as_str()
        .filter(|value| !value.is_empty())
        .unwrap_or("direct");
    let principal = thread_principal(&s.store, thread_id).unwrap_or_else(|_| "system".to_owned());
    let mode = thread_mode(&s.store, thread_id);
    let binding = binding_record(thread_id, &principal, 1, &effective_tool_set(&s.store, thread_id), &mode);
    let grant = json!({
        "grant_id": id("grant"),
        "binding_id": binding["binding_id"],
        "binding_version": 1,
        "thread_id": thread_id,
        "agent_session_id": session_id,
        "runtime_instance_id": session["instance_id"],
        "injection_kind": "mcp",
        "audience": CONTROL_SERVER,
        "credential_reference": format!("gateway:{}", &digest[..digest.len().min(12)]),
        "credential_status": "referenced",
        "issued_at": now(),
        "expires_at": (chrono::Utc::now() + chrono::Duration::seconds(GRANT_TTL_SECONDS)).to_rfc3339(),
        "revoked_at": Value::Null,
        "last_touched_at": now(),
        "epoch": s.chat.epoch(),
        "token_digest": digest,
        "adapter_id": session["adapter_id"],
    });
    s.capability_gateway.set_binding_revision(thread_id, thread_binding_revision(&s.store, thread_id));
    Ok(grant)
}

#[cfg(test)]
fn session_grant(s: &AppState, thread_id: &str, digest: &str, audience: &str, renew: bool) -> Result<(Value,Value),Failure> {
    let denied = |code: &str, message: &str| crate::operations_api::fail(StatusCode::FORBIDDEN,code,message);
    loop {
        let grant = s.store.list(GRANTS).map_err(storage)?.into_iter().find(|grant|
            grant["token_digest"] == digest && grant["thread_id"] == thread_id
                && grant["epoch"] == s.chat.epoch()
                && grant["revoked_at"].is_null())
            .ok_or_else(||denied("capability.grant.required","Current session has no matching active grant"))?;
        let session_id = grant["agent_session_id"].as_str().unwrap_or("");
        let session = s.store.get(SESSIONS,session_id).map_err(storage)?
            .ok_or_else(||denied("capability.session.not_found","Grant agent session was not found"))?;
        if session["open"] != true || session["epoch"] != s.chat.epoch() || session["thread_id"] != thread_id {
            return Err(denied("capability.session.closed","Grant session is closed or belongs to another Thread/service instance"));
        }
        if grant["audience"] != audience { return Err(denied("capability.grant.audience_mismatch","Grant audience does not match this endpoint")); }
        if !renew && grant_state(&grant,s.chat.epoch()) != "active" {
            return Err(denied("capability.grant.expired","Grant lease expired; a trusted session interaction must renew it"));
        }
        let principal = thread_principal(&s.store,thread_id)?;
        let key = binding_key(thread_id,&principal);
        let bindings = s.store.get(BINDINGS,&key).map_err(storage)?
            .ok_or_else(||denied("capability.binding.not_found","Grant binding no longer exists"))?;
        let binding = bindings["versions"].as_array().into_iter().flatten()
            .filter(|binding|binding["binding_id"] == grant["binding_id"])
            .max_by_key(|binding|binding["binding_version"].as_u64().unwrap_or(0))
            .ok_or_else(||denied("capability.binding.not_found","Grant binding no longer exists"))?.clone();
        if !binding["revoked_at"].is_null() || binding["binding_version"] != grant["binding_version"]
            || binding["thread_id"] != thread_id || binding["principal_id"] != principal {
            return Err(denied("capability.binding.stale","Grant references a revoked or replaced binding revision"));
        }
        let mut next = grant.clone();
        next["last_touched_at"] = json!(now());
        if renew { next["expires_at"] = json!((chrono::Utc::now()+chrono::Duration::seconds(GRANT_TTL_SECONDS)).to_rfc3339()); }
        let grant_id = grant["grant_id"].as_str().unwrap_or("");
        if s.store.put_many_if(&[(SESSIONS,session_id,Some(&session)),(BINDINGS,key.as_str(),Some(&bindings)),(GRANTS,grant_id,Some(&grant))],
            &[(GRANTS,grant_id,&next)]).map_err(storage)? { return Ok((next,binding)); }
    }
}

pub(crate) fn authorize_gateway_grant(s: &AppState, thread_id: &str, digest: &str) -> Result<(Value,Value),Failure> {
    let mode = thread_mode(&s.store, thread_id);
    let principal = thread_principal(&s.store, thread_id).unwrap_or_else(|_| "system".to_owned());
    let tools = effective_tool_set(&s.store, thread_id);
    let binding = binding_record(thread_id, &principal, 1, &tools, &mode);
    let grant = json!({
        "grant_id": format!("grant-{}", thread_id),
        "binding_id": binding["binding_id"],
        "binding_version": 1,
        "thread_id": thread_id,
        "token_digest": digest,
        "state": "active",
    });
    Ok((grant, binding))
}

pub(crate) fn renew_session_grant(_s: &AppState, _thread_id: &str) -> Result<(),Failure> {
    Ok(())
}

pub fn revoke_session_grants(s: &AppState, agent_session_id: &str) -> Result<usize, Failure> {
    s.store.revoke_capability_sessions(None,Some(agent_session_id),&now()).map_err(storage)
}

pub(crate) fn revoke_thread_grants(s: &AppState, thread_id: &str) -> Result<usize,Failure> {
    s.store.revoke_capability_sessions(Some(thread_id),None,&now()).map_err(storage)
}


fn adapter_rows(s: &AppState) -> Vec<Value> {
    let views = crate::runtime_api::list_views(s, "", true, "").unwrap_or_default();
    let mut rows = Vec::new();
    for engine in descriptors::all() {
        let name = engine.engine();
        let instances: Vec<&Value> = views.iter().filter(|view| view["engine"] == name).collect();
        let preferred = &engine.identity.default_adapter_id;
        let selected = instances
            .iter()
            .find(|view| view["adapter_id"] == preferred.as_str() && view["health"]["healthy"] == true)
            .or_else(|| instances.iter().find(|view| view["health"]["healthy"] == true))
            .or_else(|| instances.iter().find(|view| view["configured"] == true))
            .or_else(|| instances.first())
            .copied()
            .cloned()
            .unwrap_or(Value::Null);
        let adapter_id = selected["adapter_id"].as_str().unwrap_or(preferred).to_owned();
        let transport = descriptors::for_adapter(&adapter_id)
            .map(|(_, adapter)| adapter.transport_kind.clone())
            .unwrap_or_default();
        let health = &selected["health"];
        let injection = |kind: &str, supported: bool| json!({"kind": kind, "supported": supported, "source": "descriptor"});
        let latest = s
            .store
            .list(GRANTS)
            .unwrap_or_default()
            .into_iter()
            .filter(|grant| {
                grant["adapter_id"].as_str().and_then(descriptors::for_adapter).is_some_and(|(e, _)| e.engine() == name)
            })
            .max_by(|a, b| a["issued_at"].as_str().cmp(&b["issued_at"].as_str()))
            .map(|grant| {
                let mut view = grant_view(&grant, s.chat.epoch());
                view["adapter_id"] = grant["adapter_id"].clone();
                view["runtime_instance_id"] = grant["runtime_instance_id"].clone();
                view
            });
        rows.push(json!({
            "engine": name, "adapter_id": adapter_id,
            "instance_id": selected["instance_id"].as_str().unwrap_or("default"),
            "configured": selected["configured"] == true, "enabled": selected["enabled"] == true,
            "transport_kind": transport,
            "health_state": match health["healthy"] {
                Value::Bool(true) => "healthy", Value::Bool(false) => "unhealthy", _ => "unprobed",
            },
            "health_detail": health["detail"].as_str().unwrap_or(""),
            "probed_at": health["probed_at"], "runtime_version": health["runtime_version"].as_str().unwrap_or(""),
            "capabilities": health["capabilities"],
            "injection_support": [
                injection("mcp", crate::agent_runtime::pool::attaches_mcp(&adapter_id)),
                injection("native_tool", false), injection("acp_mcp_config", false),
                injection("agent_plugin", false), injection("http_jsonrpc", false),
            ],
            "last_session_injection": latest,
            "source": ["builtin"],
            "instances": instances.iter().map(|view| json!({
                "adapter_id": view["adapter_id"], "instance_id": view["instance_id"],
                "configured": view["configured"] == true, "enabled": view["enabled"] == true,
                "transport_kind": descriptors::for_adapter(view["adapter_id"].as_str().unwrap_or(""))
                    .map(|(_, adapter)| adapter.transport_kind.clone()).unwrap_or_default(),
                "health_state": match view["health"]["healthy"] {
                    Value::Bool(true) => "healthy", Value::Bool(false) => "unhealthy", _ => "unprobed",
                },
            })).collect::<Vec<_>>(),
        }));
    }
    rows
}

fn thread_rows(s: &AppState) -> Result<Vec<Value>, Failure> {
    let mut grants_by_binding: HashMap<String, Vec<Value>> = HashMap::new();
    for grant in s.store.list(GRANTS).map_err(storage)? {
        grants_by_binding.entry(grant["binding_id"].as_str().unwrap_or_default().to_owned()).or_default().push(grant);
    }
    let mut rows = Vec::new();
    let mut summaries = s.store.conversation_summaries().map_err(storage)?;
    summaries.sort_by(|a, b| b["thread"]["updated_at"].as_str().cmp(&a["thread"]["updated_at"].as_str()));
    for summary in summaries {
        let thread = &summary["thread"];
        if crate::chat::is_ctf_thread(thread) || crate::chat::is_side_chat(thread) {
            continue;
        }
        let thread_id = thread["thread_id"].as_str().unwrap_or_default();
        let principal = thread_principal(&s.store, thread_id)?;
        let saved = versions(&s.store, thread_id, &principal)?;
        let current = active(&saved);
        let mut grants: Vec<Value> = saved
            .iter()
            .filter_map(|binding| grants_by_binding.get(binding["binding_id"].as_str().unwrap_or_default()))
            .flatten()
            .map(|grant| grant_view(grant, s.chat.epoch()))
            .collect();
        grants.sort_by(|a, b| b["issued_at"].as_str().cmp(&a["issued_at"].as_str()));
        rows.push(json!({
            "thread": {
                "thread_id": thread_id, "title": thread["title"].as_str().filter(|t| !t.is_empty()).unwrap_or("未命名 Thread"),
                "mode": thread["mode"].as_str().unwrap_or("conversation"), "updated_at": thread["updated_at"],
            },
            "principal_id": principal,
            "active_binding": current.clone(),
            "initial_tool_set": current
                .as_ref()
                .map(|binding| binding["tool_set"].clone())
                .unwrap_or_else(|| json!(thread_tool_names(&s.store, thread_id))),
            "selection_source": if current.is_some() { "binding" } else { "mode_template" },
            "versions": saved.iter().rev().cloned().collect::<Vec<_>>(),
            "grants": grants,
        }));
    }
    Ok(rows)
}

pub(crate) async fn overview(State(s): State<Arc<AppState>>) -> Api {
    let (_, policy_error) = policy(&s);
    let tools = catalog();
    let collab_on = collab_enabled(&s);
    let browser_on = browser_enabled(&s);
    let computer_on = computer_enabled(&s);
    let platform_on = platform_always_on(&s);
    let computer = crate::chat_computer::status();
    let epoch = s.chat.epoch().to_owned();
    let grants = s.store.list(GRANTS).map_err(storage)?;
    let active_grants = grants.iter().filter(|grant| grant_state(grant, &epoch) == "active").count();
    let conv_tools = mode_tools("conversation");
    let collab_count = conv_tools.iter().filter(|name| is_collab_tool(name)).count();
    let browser_count = crate::browser::NAMES.len();
    let platform_backing_count = conv_tools.iter().filter(|name| is_platform_tool(name)).count();
    let skill_item = |resource: &str, name: &str, scope: &str, detail: &str, implemented: bool, lifecycle: &str| {
        let is_on = enabled(&s, "skills", resource);
        json!({
            "id": resource, "name": name, "enabled": is_on, "mutable": true,
            "health": if !is_on { "disabled" } else if implemented { "ready" } else { "unavailable" },
            "source": detail, "scope": scope,
            "engines": descriptors::engine_ids().iter().map(|engine| ((*engine).to_owned(), json!([]))).collect::<Map<String, Value>>(),
            "legacy_copies": [], "lifecycle": lifecycle,
        })
    };
    let blackboard_on = enabled(&s, "skills", "muteki-blackboard");
    let any_mcp_ready = collab_on || browser_on || computer_on || platform_on;
    Ok(Json(json!({
        "policy_error": policy_error,
        "mcp": {
            "status": if any_mcp_ready { "ready" } else { "disabled" },
            "endpoint": "/api/capability", "implementation": "muteki_server::capability_gateway",
            "active_grants": active_grants,
            "servers": [{
                "id": COLLAB_SERVER, "name": "会话协作与多模型编排",
                "enabled": collab_on,
                "health": if collab_on { "ready" } else { "disabled" },
                "source": "builtin", "scope": "conversation", "endpoint": "/api/capability/collab",
                "protocol_version": crate::capability_gateway::PROTOCOL_VERSION,
                "tools": collab_count, "active_grants": active_grants, "lifecycle": "new_sessions",
            }, {
                "id": BROWSER_SERVER, "name": "内置预览与无头浏览器",
                "enabled": browser_on,
                "health": if browser_on { "ready" } else { "disabled" },
                "source": "builtin", "scope": "conversation", "endpoint": "/api/capability/browser",
                "protocol_version": crate::capability_gateway::PROTOCOL_VERSION,
                "tools": browser_count, "active_grants": active_grants, "lifecycle": "new_sessions",
            }, {
                "id": COMPUTER_RESOURCE, "name": "本机电脑操控 (Computer Use)",
                "enabled": computer_on,
                "health": if !computer_on { "disabled" } else if computer["available"] == true { "ready" } else { "unavailable" },
                "source": "installed-plugin", "scope": "mac_host", "endpoint": "/api/capability/computer",
                "protocol_version": crate::capability_gateway::PROTOCOL_VERSION,
                "tools": computer["tools"], "status": computer, "lifecycle": "new_sessions",
            }, {
                "id": PLATFORM_SERVER, "name": "安全平台桥接（渐进式披露）",
                "enabled": platform_on,
                "health": if platform_on { "ready" } else { "disabled" },
                "source": "builtin", "scope": "progressive_disclosure", "endpoint": "/api/capability/platform",
                "protocol_version": crate::capability_gateway::PROTOCOL_VERSION,
                "tools": 2, "backing_tools": platform_backing_count, "progressive_disclosure": true,
                "active_grants": active_grants, "lifecycle": "new_sessions",
            }],
            "extension_sources": [],
        },
        "skills": {
            "status": if blackboard_on { "ready" } else { "disabled" }, "id": COLLAB_SERVER, "implementation": "mcp/2026-07-28",
            "active_grants": 0,
            "items": [
                skill_item("muteki-blackboard", "Muteki Blackboard", "worker_workspace", "builtin", true, "new_workers"),
                skill_item("agent-browser", "agent-browser", "pentest_worker_workspace", "builtin", true, "new_workers"),
                skill_item("show-skills-in-slash", "斜杠菜单包含 Skills", "composer", "builtin", true, "immediate"),
            ],
            "extension_sources": [],
        },
        "tools": tools,
        "adapters": adapter_rows(&s),
        "extensions": [],
        "threads": thread_rows(&s)?,
        "mode_templates": {"conversation": tool_names()},
    })))
}

pub(crate) async fn update_resource(State(s): State<Arc<AppState>>, Path((kind, resource)): Path<(String, String)>, body: Option<Json<Value>>) -> Api {
    let Some(value) = body.and_then(|Json(body)| body["enabled"].as_bool()) else {
        return Err(failure(StatusCode::UNPROCESSABLE_ENTITY, json!("enabled 必须是布尔值")));
    };
    set_enabled(&s, &kind, &resource, value)?;
    if kind == "mcp" && (resource == COLLAB_SERVER || resource == CONTROL_SERVER) {
        let other = if resource == COLLAB_SERVER { CONTROL_SERVER } else { COLLAB_SERVER };
        let _ = set_enabled(&s, "mcp", other, value);
    }
    if kind == "mcp" || (kind == "skills" && resource == "show-skills-in-slash") {
        crate::native_chat::invalidate_all_capabilities(&s).await;
    }
    Ok(Json(json!({
        "kind": kind, "id": resource, "enabled": value, "command_id": id("cmd"), "state": "completed",
        "applies_to": if kind == "mcp" { "new_sessions" } else if resource == "show-skills-in-slash" { "immediate" } else { "new_workers" },
    })))
}

fn thread_exists(s: &AppState, thread_id: &str) -> Result<(), Failure> {
    match s.store.get("conversation_threads", thread_id).map_err(storage)? {
        Some(_) => Ok(()),
        None => Err(failure(StatusCode::NOT_FOUND, json!("Thread 不存在"))),
    }
}

pub(crate) async fn replace_binding(State(s): State<Arc<AppState>>, Path(thread_id): Path<String>, body: Option<Json<Value>>) -> Api {
    let Json(body) = body.unwrap_or(Json(json!({})));
    thread_exists(&s, &thread_id)?;
    let Some(requested) = body["tool_set"].as_array() else {
        return Err(failure(StatusCode::UNPROCESSABLE_ENTITY, json!("tool_set 必须是数组")));
    };
    let principal = body["principal_id"].as_str().map(str::trim).filter(|p| !p.is_empty())
        .map(str::to_owned).unwrap_or(thread_principal(&s.store, &thread_id)?);
    let mut selected: Vec<String> = Vec::new();
    for name in requested.iter().filter_map(Value::as_str) {
        if spec(name).is_none() {
            return Err(failure(StatusCode::UNPROCESSABLE_ENTITY, json!(format!("未知能力工具 {name}"))));
        }
        if !selected.iter().any(|kept| kept == name) {
            selected.push(name.to_owned());
        }
    }
    let key = binding_key(&thread_id,&principal);
    let (binding,changed,revoked) = loop {
        let previous = s.store.get(BINDINGS,&key).map_err(storage)?;
        let mut rows = previous.as_ref().and_then(|row|row["versions"].as_array().cloned()).unwrap_or_default();
        let current = active(&rows);
        if let Some(binding) = current.as_ref().filter(|binding|binding["tool_set"] == json!(selected)) {
            break (binding.clone(),false,0);
        }
        let version = rows.iter().filter_map(|row|row["binding_version"].as_u64()).max().unwrap_or(0)+1;
        let mode = thread_mode(&s.store,&thread_id);
        let mut created = binding_record(&thread_id,&principal,version,&selected,&mode);
        created["selection_source"] = json!("binding");
        let revoke: Vec<String> = rows.iter().filter(|row| row["revoked_at"].is_null())
            .filter_map(|row|row["binding_id"].as_str().map(str::to_owned)).collect();
        if let Some(binding) = current.as_ref() { created["binding_id"] = binding["binding_id"].clone(); }
        let stamp = now();
        for row in rows.iter_mut().filter(|row|row["revoked_at"].is_null()) { row["revoked_at"] = json!(stamp); }
        rows.push(created.clone());
        let next = json!({"thread_id":thread_id,"principal_id":principal,"versions":rows});
        if let Some(revoked) = s.store.commit_capability_binding(&key,previous.as_ref(),&next,&revoke,&stamp).map_err(storage)? {
            break (created,true,revoked);
        }
    };
    let refresh = if changed { crate::native_chat::reload_thread_capabilities(&s,&thread_id).await }
        else { json!({"interrupted":false,"session_closed":false}) };
    Ok(Json(json!({"binding":binding,"changed":changed,"revoked_grants":revoked,"runtime_refresh":refresh,
        "applies_to":if changed{"next_turn"}else{"unchanged"}})))
}

pub(crate) async fn revoke_binding(State(s): State<Arc<AppState>>, Path(thread_id): Path<String>, body: Option<Json<Value>>) -> Api {
    let Json(body) = body.unwrap_or(Json(json!({})));
    thread_exists(&s, &thread_id)?;
    let principal = body["principal_id"].as_str().map(str::trim).filter(|p| !p.is_empty())
        .map(str::to_owned).unwrap_or(thread_principal(&s.store, &thread_id)?);
    let key = binding_key(&thread_id,&principal);
    let (binding,revoked) = loop {
        let previous = s.store.get(BINDINGS,&key).map_err(storage)?;
        let mut rows = previous.as_ref().and_then(|row|row["versions"].as_array().cloned()).unwrap_or_default();
        let Some(current) = active(&rows) else { return Err(failure(StatusCode::NOT_FOUND,json!("该 Thread 没有活动 Binding"))); };
        let stamp = now();
        let binding_id = current["binding_id"].as_str().unwrap_or("").to_owned();
        let mut view = current;
        view["revoked_at"] = json!(stamp);
        for row in rows.iter_mut().filter(|row|row["binding_id"] == binding_id && row["revoked_at"].is_null()) { row["revoked_at"] = json!(stamp); }
        let next = json!({"thread_id":thread_id,"principal_id":principal,"versions":rows});
        if let Some(revoked) = s.store.commit_capability_binding(&key,previous.as_ref(),&next,&[binding_id],&stamp).map_err(storage)? {
            break (view,revoked);
        }
    };
    let refresh = crate::native_chat::reload_thread_capabilities(&s,&thread_id).await;
    Ok(Json(json!({"binding":binding,"revoked_grants":revoked,"runtime_refresh":refresh})))
}

pub(crate) async fn archive_src_thread_binding(s: Arc<AppState>, thread_id: &str) -> Result<(), Failure> {
    let principal = thread_principal(&s.store, thread_id)?;
    let key = binding_key(thread_id, &principal);
    loop {
        let current = s.store.get(BINDINGS, &key).map_err(storage)?;
        let mut rows = current.as_ref()
            .and_then(|row| row["versions"].as_array().cloned())
            .unwrap_or_default();
        let ids: Vec<String> = rows.iter().filter(|row| row["revoked_at"].is_null())
            .filter_map(|row| row["binding_id"].as_str().map(str::to_owned)).collect();
        if ids.is_empty() {
            return Ok(());
        }
        let stamp = now();
        for row in rows.iter_mut().filter(|row| row["revoked_at"].is_null()) {
            row["revoked_at"] = json!(stamp);
        }
        let next = json!({"thread_id":thread_id,"principal_id":principal,"versions":rows});
        if s.store.commit_capability_binding(&key,current.as_ref(),&next,&ids,&stamp).map_err(storage)?.is_some() {
            let _ = crate::native_chat::reload_thread_capabilities(&s, thread_id).await;
            return Ok(());
        }
    }
}

pub(crate) async fn restore_src_thread_binding(s: Arc<AppState>, thread_id: &str) -> Result<(), Failure> {
    let principal = thread_principal(&s.store, thread_id)?;
    let key = binding_key(thread_id, &principal);
    loop {
        let current = s.store.get(BINDINGS, &key).map_err(storage)?;
        let mut rows = current.as_ref()
            .and_then(|row| row["versions"].as_array().cloned())
            .unwrap_or_default();
        if active(&rows).is_some() {
            return Ok(());
        }
        let tools: Vec<String> = rows.last().and_then(|row| row["tool_set"].as_array())
            .map(|tools| tools.iter().filter_map(Value::as_str).map(str::to_owned).collect())
            .unwrap_or_else(|| thread_tool_names(&s.store, thread_id));
        let version = rows.iter().filter_map(|row|row["binding_version"].as_u64()).max().unwrap_or(0)+1;
        let mode = thread_mode(&s.store,thread_id);
        let mut restored = binding_record(thread_id,&principal,version,&tools,&mode);
        if let Some(previous) = rows.last() { restored["binding_id"] = previous["binding_id"].clone(); }
        rows.push(restored);
        let next = json!({"thread_id":thread_id,"principal_id":principal,"versions":rows});
        if s.store.put_many_if(&[(BINDINGS, &key, current.as_ref())],
            &[(BINDINGS, &key, &next)]).map_err(storage)? {
            let _ = crate::native_chat::reload_thread_capabilities(&s, thread_id).await;
            return Ok(());
        }
    }
}

pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/capability-management", get(overview))
        .route("/api/capability-management/resources/{kind}/{resource_id}", put(update_resource))
        .route("/api/capability-management/threads/{thread_id}/binding", put(replace_binding))
        .route("/api/capability-management/threads/{thread_id}/revoke", post(revoke_binding))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state() -> (tempfile::TempDir, Arc<AppState>) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("muteki.sqlite")).unwrap();
        store.put("system", "state_dir", &json!(dir.path())).unwrap();
        (dir, Arc::new(AppState::new(store)))
    }

    #[test]
    fn policy_defaults_apply_until_a_resource_is_turned_off() {
        let (_dir, s) = state();
        assert!(enabled(&s, "skills", "agent-browser"));
        assert!(!enabled(&s, "skills", "unknown-skill"));
        set_enabled(&s, "skills", "agent-browser", false).unwrap();
        assert!(!enabled(&s, "skills", "agent-browser"));
        assert!(enabled(&s, "mcp", CONTROL_SERVER), "unrelated capabilities keep their default");
        assert_eq!(
            set_enabled(&s, "skills", "unknown-skill", true).unwrap_err().0,
            StatusCode::NOT_FOUND
        );
    }

    #[test]
    fn a_corrupt_policy_file_disables_capabilities_and_is_reported() {
        let (_dir, s) = state();
        std::fs::write(policy_path(&s), b"[]").unwrap();
        assert!(!enabled(&s, "mcp", CONTROL_SERVER));
        assert!(policy(&s).1.contains("不是 JSON 对象"));
    }

    #[tokio::test]
    async fn narrowing_a_binding_revokes_the_grants_issued_under_it() {
        let (_dir, s) = state();
        s.store
            .put("conversation_threads", "t1", &json!({"thread":{"thread_id":"t1","title":"T","mode":"conversation","updated_at":now()},"messages":[]}))
            .unwrap();
        let grant = issue_grant(&s, "t1", &json!({"agent_session_id":"sess","instance_id":"default","adapter_id":"codex.app_server"}), "abcdef123456").unwrap();
        assert_eq!(grant_state(&grant, s.chat.epoch()), "active");
        let body = json!({"tool_set": ["muteki_list_threads", "muteki_list_threads"]});
        let Json(result) = replace_binding(State(s.clone()), Path("t1".into()), Some(Json(body.clone()))).await.unwrap();
        assert_eq!(result["changed"], true);
        assert_eq!(result["revoked_grants"], 1);
        assert_eq!(result["binding"]["tool_set"], json!(["muteki_list_threads"]));
        assert_eq!(result["binding"]["binding_version"], 2);
        assert_eq!(effective_tool_set(&s.store, "t1"), vec!["muteki_list_threads"]);
        let Json(repeat) = replace_binding(State(s.clone()), Path("t1".into()), Some(Json(body))).await.unwrap();
        assert_eq!(repeat["changed"], false);
        assert_eq!(repeat["applies_to"], "unchanged");
        let Json(revoked) = revoke_binding(State(s.clone()), Path("t1".into()), None).await.unwrap();
        assert!(!revoked["binding"]["revoked_at"].is_null());
        assert_eq!(
            revoke_binding(State(s.clone()), Path("t1".into()), None).await.unwrap_err().0,
            StatusCode::NOT_FOUND
        );
        assert_eq!(effective_tool_set(&s.store, "t1"), tool_names(), "no binding falls back to the mode template");
    }

    #[test]
    fn session_grants_last_twenty_four_hours_and_only_trusted_renewal_restores_expiry() {
        let (_dir, s) = state();
        s.store
            .put(
                "conversation_threads",
                "t1",
                &json!({"thread":{"thread_id":"t1","mode":"conversation"}}),
            )
            .unwrap();
        let grant = issue_grant(
            &s,
            "t1",
            &json!({"agent_session_id":"sess","instance_id":"default","adapter_id":"cursor.acp"}),
            "test-digest",
        )
        .unwrap();
        let issued =
            chrono::DateTime::parse_from_rfc3339(grant["issued_at"].as_str().unwrap()).unwrap();
        let expires =
            chrono::DateTime::parse_from_rfc3339(grant["expires_at"].as_str().unwrap()).unwrap();
        assert_eq!((expires - issued).num_seconds(), 24 * 60 * 60);

        let mut expired = grant.clone();
        expired["expires_at"] =
            json!((chrono::Utc::now() - chrono::Duration::seconds(1)).to_rfc3339());
        s.store
            .put(GRANTS, grant["grant_id"].as_str().unwrap(), &expired)
            .unwrap();
        let denied = authorize_gateway_grant(&s, "t1", "test-digest").unwrap_err();
        assert_eq!(denied.1.0["error"]["code"], "capability.grant.expired");
        let before = chrono::Utc::now();
        let (renewed, _) = session_grant(&s, "t1", "test-digest", CONTROL_SERVER, true).unwrap();
        let expires =
            chrono::DateTime::parse_from_rfc3339(renewed["expires_at"].as_str().unwrap()).unwrap();
        assert_eq!(
            (expires.with_timezone(&chrono::Utc) - before).num_seconds(),
            24 * 60 * 60
        );
        assert_eq!(renewed["grant_id"], grant["grant_id"]);
        assert_eq!(renewed["issued_at"], grant["issued_at"]);
        assert_eq!(renewed["binding_version"], grant["binding_version"]);
        assert!(authorize_gateway_grant(&s, "t1", "test-digest").is_ok());

        revoke_session_grants(&s, "sess").unwrap();
        assert!(session_grant(&s, "t1", "test-digest", CONTROL_SERVER, true).is_err());
        assert!(authorize_gateway_grant(&s, "t1", "test-digest").is_err());
    }

    #[tokio::test]
    async fn unknown_tools_and_threads_are_refused() {
        let (_dir, s) = state();
        assert_eq!(
            replace_binding(State(s.clone()), Path("missing".into()), Some(Json(json!({"tool_set":[]})))).await.unwrap_err().0,
            StatusCode::NOT_FOUND
        );
        s.store.put("conversation_threads", "t1", &json!({"thread":{"thread_id":"t1"}})).unwrap();
        assert_eq!(
            replace_binding(State(s.clone()), Path("t1".into()), Some(Json(json!({"tool_set":["nope"]})))).await.unwrap_err().0,
            StatusCode::UNPROCESSABLE_ENTITY
        );
        assert_eq!(
            replace_binding(State(s.clone()), Path("t1".into()), Some(Json(json!({})))).await.unwrap_err().0,
            StatusCode::UNPROCESSABLE_ENTITY
        );
    }
}
