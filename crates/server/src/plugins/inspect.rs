//! Package inspection: manifests, Skills, MCP declarations and native
//! components of an Agent extension package, and per-engine compatibility.
//!
//! Everything here reads one normalized package directory. The only write is
//! the expansion of inline command templates into `.muteki-commands/`, which
//! happens before the package snapshot is published, so those files are included
//! that are installed.

use crate::agent_runtime::descriptors;
use regex::Regex;
use serde_json::{Map, Value, json};
use std::{
    collections::{BTreeSet, HashMap, HashSet},
    path::{Component, Path, PathBuf},
    sync::OnceLock,
};

pub const MODES: [&str; 3] = ["chat", "ctf", "pentest"];
const MAX_INSTRUCTION_BYTES: u64 = 256_000;
const MAX_SKILLS: usize = 200;
const MANIFESTS: [&str; 4] = [
    "plugin.json",
    ".codex-plugin/plugin.json",
    ".claude-plugin/plugin.json",
    ".cursor-plugin/plugin.json",
];
const NATIVE_SKILL_KEYS: [&str; 7] = [
    "context",
    "agent",
    "hooks",
    "allowed-tools",
    "disallowed-tools",
    "model",
    "disable-model-invocation",
];
pub const PATH_TOKENS: [&str; 4] = [
    "${PLUGIN_ROOT}",
    "${CLAUDE_PLUGIN_ROOT}",
    "${CODEX_PLUGIN_ROOT}",
    "${PLUGIN_DATA}",
];

/// A user-correctable or attributed package failure with a stable code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginError {
    pub code: String,
    pub message: String,
}

impl PluginError {
    pub fn new(code: &str, message: impl Into<String>) -> Self {
        Self { code: code.to_owned(), message: message.into() }
    }
    pub fn invalid(message: impl Into<String>) -> Self {
        Self::new("chat_plugin.invalid", message)
    }
}

impl std::fmt::Display for PluginError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

pub type Result<T> = std::result::Result<T, PluginError>;


pub fn engines() -> Vec<String> {
    descriptors::engine_ids().into_iter().map(str::to_owned).collect()
}

fn engines_where(predicate: impl Fn(&Value) -> bool) -> Vec<String> {
    descriptors::all()
        .iter()
        .filter(|descriptor| predicate(descriptor.public()))
        .map(|descriptor| descriptor.engine().to_owned())
        .collect()
}

fn component_text<'a>(descriptor: &'a Value, key: &str) -> &'a str {
    descriptor["components"][key].as_str().unwrap_or("none")
}

pub fn native_skill_engines() -> Vec<String> {
    engines_where(|d| d["components"]["native_skill_semantics"] == true)
}
pub fn worker_component_engines() -> Vec<String> {
    engines_where(|d| d["environment"]["worker_component_home_env"].as_str().is_some_and(|v| !v.is_empty()))
}
pub fn extension_engines() -> Vec<String> {
    engines_where(|d| d["components"]["extension_abi"] == true)
}
fn plugin_agent_engines() -> Vec<String> {
    engines_where(|d| component_text(d, "native_agents") == "plugin_package")
}
fn native_agent_engines() -> Vec<String> {
    engines_where(|d| component_text(d, "native_agents") != "none")
}
fn hook_engines() -> Vec<String> {
    engines_where(|d| component_text(d, "native_hooks") == "all")
}
fn command_hook_engines() -> Vec<String> {
    engines_where(|d| component_text(d, "native_hooks") != "none")
}

/// The engine has a verified private home, so Worker components may be
/// projected for it.
pub fn managed_environment(engine: &str) -> bool {
    descriptors::find(engine).is_some_and(|d| d.public()["environment"]["managed_home"] == true)
}

/// The engine's default session transport can attach Muteki's HTTP MCP
/// gateway, so package MCP tools reach the model.
pub fn gateway_tools(engine: &str) -> bool {
    let Some(descriptor) = descriptors::find(engine) else { return false };
    let adapter = descriptor.default_adapter();
    adapter.capability_gateway
        && crate::agent_runtime::pool::attaches_mcp(&adapter.adapter_id)
}


/// Normalize `.` and `..` without touching the filesystem.
pub fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// `root/relative`, refused when it leaves the package.
pub fn contained(root: &Path, relative: &str) -> Result<PathBuf> {
    let path = normalize(&root.join(relative));
    if !path.starts_with(normalize(root)) {
        return Err(PluginError::invalid("插件资源路径必须位于插件包内"));
    }
    Ok(path)
}

/// Every entry below `root`, sorted, without following links.
pub fn tree(root: &Path) -> Vec<PathBuf> {
    let mut found: Vec<PathBuf> = walkdir::WalkDir::new(root)
        .min_depth(1)
        .follow_links(false)
        .into_iter()
        .filter_map(|entry| entry.ok().map(|entry| entry.into_path()))
        .collect();
    found.sort();
    found
}

fn files_named(root: &Path, name: &str) -> Vec<PathBuf> {
    tree(root).into_iter().filter(|p| p.file_name().is_some_and(|n| n == name) && p.is_file()).collect()
}

fn markdown_under(location: &Path) -> Vec<PathBuf> {
    if location.is_dir() {
        tree(location)
            .into_iter()
            .filter(|p| p.is_file() && p.extension().is_some_and(|e| e == "md"))
            .collect()
    } else if location.is_file() {
        vec![location.to_path_buf()]
    } else {
        Vec::new()
    }
}

fn relative(root: &Path, path: &Path) -> String {
    path.strip_prefix(root).unwrap_or(path).to_string_lossy().replace('\\', "/")
}

fn stem(path: &Path) -> String {
    path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default()
}

pub fn which(name: &str) -> Option<PathBuf> {
    if name.contains('/') {
        let path = PathBuf::from(name);
        return is_executable(&path).then_some(path);
    }
    let paths = crate::shell_env::path_os()?;
    std::env::split_paths(&paths).map(|dir| dir.join(name)).find(|p| is_executable(p))
}

fn is_executable(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
    }
    #[cfg(not(unix))]
    {
        path.is_file()
    }
}


fn name_pattern() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^[a-zA-Z0-9][a-zA-Z0-9_.-]{0,79}$").unwrap())
}

pub fn valid_package_name(name: &str) -> bool {
    name_pattern().is_match(name)
}

fn component_name(name: &str) -> bool {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^[\w.-]{1,100}$").unwrap()).is_match(name)
}

fn mcp_server_name(name: &str) -> bool {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^[\w.-]{1,80}$").unwrap()).is_match(name)
}

fn read_json(path: &Path) -> Result<Value> {
    let text = std::fs::read_to_string(path)
        .map_err(|_| PluginError::invalid(format!("无法读取 {}", path.file_name().unwrap_or_default().to_string_lossy())))?;
    serde_json::from_str(&text)
        .map_err(|e| PluginError::invalid(format!("{} 不是有效 JSON：{e}", path.file_name().unwrap_or_default().to_string_lossy())))
}

pub fn frontmatter(text: &str) -> Result<(Map<String, Value>, String)> {
    if !(text.starts_with("---") && text.matches("---").count() >= 2) {
        return Ok((Map::new(), text.to_owned()));
    }
    let rest = &text[3..];
    let (raw, body) = rest.split_once("---").unwrap_or((rest, ""));
    let yaml: serde_yaml::Value =
        serde_yaml::from_str(raw).map_err(|e| PluginError::invalid(format!("front matter 不是有效 YAML：{e}")))?;
    let value = serde_json::to_value(yaml).unwrap_or(Value::Null);
    Ok((value.as_object().cloned().unwrap_or_default(), body.to_owned()))
}

fn instruction_file(path: &Path) -> Result<(Map<String, Value>, String)> {
    let size = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    if size > MAX_INSTRUCTION_BYTES {
        return Err(PluginError::invalid("插件指令超过 256 KB"));
    }
    let text = std::fs::read_to_string(path).map_err(|_| PluginError::invalid("插件指令不是 UTF-8 文本"))?;
    frontmatter(&text)
}

fn truthy(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Array(a)) => !a.is_empty(),
        Some(Value::Object(o)) => !o.is_empty(),
        Some(Value::Number(n)) => n.as_f64() != Some(0.0),
    }
}

fn text_of(value: Option<&Value>) -> Option<String> {
    match value {
        Some(Value::String(s)) if !s.is_empty() => Some(s.clone()),
        Some(Value::Number(n)) => Some(n.to_string()),
        Some(Value::Bool(b)) => Some(if *b { "True" } else { "False" }.to_owned()),
        _ => None,
    }
}

fn string_list(value: &Value) -> Option<Vec<String>> {
    value.as_array()?.iter().map(|v| v.as_str().map(str::to_owned)).collect()
}

fn string_map(value: &Value) -> bool {
    value.as_object().is_some_and(|m| m.values().all(Value::is_string))
}


/// Inspect a normalized package directory into a registry record (without
/// snapshot ID, root or lifecycle fields).
pub fn inspect(root: &Path) -> Result<Value> {
    let mut manifest = Map::new();
    for name in MANIFESTS {
        let path = contained(root, name)?;
        if path.is_file() {
            manifest = read_json(&path)?
                .as_object()
                .cloned()
                .ok_or_else(|| PluginError::invalid("插件清单必须是 JSON 对象"))?;
            break;
        }
    }
    let skill_files = files_named(root, "SKILL.md");
    if manifest.is_empty() && root.join("package.json").is_file() {
        let package = read_json(&root.join("package.json"))?;
        if let Some(object) = package.as_object()
            && extension_engines().iter().any(|engine| object.contains_key(engine))
        {
            manifest = object.clone();
            let raw = text_of(object.get("name"))
                .unwrap_or_else(|| root.file_name().unwrap_or_default().to_string_lossy().into_owned());
            manifest.insert("name".into(), json!(raw.trim_start_matches('@').replace('/', "-")));
        }
    }
    if manifest.is_empty() && skill_files.is_empty() {
        return Err(PluginError::new("chat_plugin.not_a_plugin", "未找到插件清单或 SKILL.md"));
    }
    let mut name = text_of(manifest.get("name")).unwrap_or_default().trim().to_owned();
    let mut skills: Vec<Value> = Vec::new();
    for path in skill_files.iter().take(MAX_SKILLS) {
        if std::fs::metadata(path).map(|m| m.len()).unwrap_or(0) > MAX_INSTRUCTION_BYTES {
            return Err(PluginError::invalid("SKILL.md 超过 256 KB"));
        }
        let text = std::fs::read_to_string(path).map_err(|_| PluginError::invalid("SKILL.md 不是 UTF-8 文本"))?;
        let (front, _) = frontmatter(&text)?;
        let skill_name = text_of(front.get("name")).unwrap_or_else(|| {
            path.parent().and_then(Path::file_name).unwrap_or_default().to_string_lossy().into_owned()
        });
        if !component_name(&skill_name) {
            return Err(PluginError::invalid("Skill 名称无效"));
        }
        if name.is_empty() {
            name = skill_name.clone();
        }
        let invocable = !matches!(
            front.get("user-invocable").or_else(|| front.get("user_invocable")),
            Some(Value::Bool(false))
        );
        skills.push(json!({
            "name": skill_name,
            "description": text_of(front.get("description")).unwrap_or_else(|| "Agent Skill".into()),
            "path": relative(root, path),
            "hooks": front.get("hooks").filter(|v| truthy(Some(v))).cloned().unwrap_or_else(|| json!({})),
            "requires_native": NATIVE_SKILL_KEYS.iter().any(|key| truthy(front.get(*key))) || text.contains("!`"),
            "invocable": invocable,
        }));
    }
    if !valid_package_name(&name) {
        return Err(PluginError::invalid("插件名称仅支持字母、数字、点、横线和下划线"));
    }
    let mut mcp = Map::new();
    let mut declarations: Vec<Value> = vec![json!("mcp.json"), json!(".mcp.json")];
    match manifest.get("mcpServers") {
        Some(Value::Array(items)) => declarations.extend(items.iter().cloned()),
        Some(value) if truthy(Some(value)) => declarations.push(value.clone()),
        _ => {}
    }
    for mut value in declarations {
        if let Value::String(reference) = &value {
            let path = contained(root, reference)?;
            if !path.is_file() {
                if reference != "mcp.json" && reference != ".mcp.json" {
                    return Err(PluginError::invalid("MCP 声明文件不存在"));
                }
                continue;
            }
            value = read_json(&path)?;
        }
        let Some(object) = value.as_object() else {
            return Err(PluginError::invalid("MCP 声明必须为对象或包内 JSON 路径"));
        };
        let servers = object.get("mcpServers").cloned().unwrap_or_else(|| value.clone());
        let Some(servers) = servers.as_object() else {
            return Err(PluginError::invalid("MCP 配置必须是最多 20 个服务的 mcpServers 对象"));
        };
        for (key, config) in servers {
            mcp.insert(key.clone(), config.clone());
        }
    }
    let mcp = Value::Object(mcp);
    validate_mcp(&mcp, Some(root))?;
    let mut components = inspect_components(root, &manifest)?;
    let native_skills = native_skill_engines();
    let items = components["components"].as_array().cloned().unwrap_or_default();
    let mut updated = Vec::with_capacity(items.len());
    for mut item in items {
        if item["kind"] == "commands" {
            let metadata = item["metadata"].as_object().cloned().unwrap_or_default();
            let path = item["path"].as_str().unwrap_or("").to_owned();
            let body = std::fs::read_to_string(contained(root, &path)?).unwrap_or_default();
            let requires_native = NATIVE_SKILL_KEYS.iter().any(|key| truthy(metadata.get(*key))) || body.contains("!`");
            skills.push(json!({
                "name": item["name"], "description": item["description"], "path": path,
                "invocable": true, "component_kind": "command",
                "hooks": metadata.get("hooks").filter(|v| truthy(Some(v))).cloned().unwrap_or_else(|| json!({})),
                "requires_native": requires_native,
            }));
            if requires_native {
                let mut sorted = native_skills.clone();
                sorted.sort();
                item["engines"] = json!(sorted);
            }
        }
        updated.push(item);
    }
    for skill in &skills {
        let hooks = &skill["hooks"];
        if truthy(Some(hooks)) {
            if !hooks.is_object() {
                return Err(PluginError::invalid("Skill hooks 必须为事件对象"));
            }
            let mut sorted = native_skills.clone();
            sorted.sort();
            updated.push(json!({
                "kind": "skill_hooks", "name": skill["name"], "config": {"hooks": hooks},
                "engines": sorted, "delivery": "native_hooks",
            }));
        }
    }
    components["components"] = json!(updated);
    if skills.is_empty() && mcp.as_object().is_some_and(Map::is_empty) && updated.is_empty() {
        return Err(PluginError::invalid("未找到可导入的 Skill 或 MCP；此插件可能依赖原客户端"));
    }
    let unsupported: Vec<&str> = ["hooks", "agents", "commands", "extensions"]
        .into_iter()
        .filter(|key| truthy(manifest.get(*key)) || root.join(key).is_dir())
        .collect();
    let description = text_of(manifest.get("description"))
        .or_else(|| skills.first().and_then(|s| s["description"].as_str().map(str::to_owned)))
        .unwrap_or_else(|| "MCP".into());
    let mut record = json!({
        "id": name, "name": name,
        "version": text_of(manifest.get("version")).unwrap_or_else(|| "local".into()),
        "description": description, "skills": skills, "mcp": mcp,
        "native_components": unsupported, "component_schema_version": 6,
    });
    for (key, value) in components.as_object().cloned().unwrap_or_default() {
        record[key] = value;
    }
    Ok(record)
}

fn inspect_components(root: &Path, manifest: &Map<String, Value>) -> Result<Value> {
    let all = engines();
    let native_meta = manifest
        .get("extensions")
        .and_then(Value::as_object)
        .and_then(|namespaces| namespaces.get("io.github.fishcodetech.muteki"))
        .cloned();
    let meta = match native_meta {
        Some(value) => value,
        None => manifest.get("muteki").filter(|v| truthy(Some(v))).cloned().unwrap_or_else(|| json!({})),
    };
    let Some(meta) = meta.as_object().cloned() else {
        return Err(PluginError::invalid("muteki 插件元数据必须为对象"));
    };
    let explicit_engines = match meta.get("engines") {
        None => None,
        Some(value) => Some(
            string_list(value)
                .filter(|list| list.iter().all(|engine| all.contains(engine)))
                .ok_or_else(|| PluginError::invalid("插件引擎声明无效"))?,
        ),
    };
    let declared_engines = explicit_engines.clone().unwrap_or_else(|| all.clone());
    let allowed_modes = match meta.get("modes") {
        None => MODES.iter().map(|m| (*m).to_owned()).collect(),
        Some(value) => string_list(value)
            .filter(|list| !list.is_empty() && list.iter().all(|mode| MODES.contains(&mode.as_str())))
            .ok_or_else(|| PluginError::invalid("插件使用场景声明无效"))?,
    };
    let mut components: Vec<Value> = Vec::new();
    for kind in ["commands", "agents"] {
        let declared = manifest.get(kind).cloned().unwrap_or_else(|| json!(kind));
        let mut paths: Vec<Value> = match &declared {
            Value::Object(entries) if kind == "commands" => {
                let mut expanded = Vec::new();
                for (name, entry) in entries {
                    let Some(entry) = entry.as_object().filter(|_| component_name(name)) else {
                        return Err(PluginError::invalid("命令模板声明无效"));
                    };
                    if entry.contains_key("content") == entry.contains_key("source") {
                        return Err(PluginError::invalid("命令模板必须且只能声明 content 或 source"));
                    }
                    let (mut config, body) = match entry.get("source") {
                        Some(source) => instruction_file(&contained(root, source.as_str().unwrap_or(""))?)?,
                        None => (Map::new(), text_of(entry.get("content")).unwrap_or_default()),
                    };
                    let description = entry
                        .get("description")
                        .cloned()
                        .or_else(|| config.get("description").cloned())
                        .unwrap_or_else(|| json!(name));
                    config.insert("name".into(), json!(name));
                    config.insert("description".into(), description);
                    let file = contained(root, &format!(".muteki-commands/{name}.md"))?;
                    if let Some(parent) = file.parent() {
                        std::fs::create_dir_all(parent).map_err(|_| PluginError::invalid("无法展开命令模板"))?;
                    }
                    let yaml = serde_yaml::to_string(&Value::Object(config)).unwrap_or_default();
                    std::fs::write(&file, format!("---\n{yaml}---\n{body}"))
                        .map_err(|_| PluginError::invalid("无法展开命令模板"))?;
                    expanded.push(json!(relative(root, &file)));
                }
                expanded
            }
            Value::Array(items) => items.clone(),
            other => vec![other.clone()],
        };
        paths.retain(Value::is_string);
        for path in paths {
            let location = contained(root, path.as_str().unwrap_or(""))?;
            for file in markdown_under(&location) {
                let (config, _) = instruction_file(&file)?;
                let name = text_of(config.get("name")).unwrap_or_else(|| stem(&file));
                if !component_name(&name) {
                    return Err(PluginError::invalid("插件组件名称无效"));
                }
                components.push(json!({
                    "kind": kind, "name": name, "path": relative(root, &file),
                    "description": text_of(config.get("description")).unwrap_or_else(|| name.clone()),
                    "metadata": config,
                    "engines": if kind == "commands" { all.clone() } else { plugin_agent_engines() },
                    "delivery": if kind == "commands" { "prompt_template" } else { "native_agent" },
                }));
            }
        }
    }
    for engine in native_agent_engines() {
        let Some(spec) = manifest.get(&engine).and_then(Value::as_object) else { continue };
        let Some(paths) = spec.get("agents") else { continue };
        let paths = string_list(paths).ok_or_else(|| PluginError::invalid(format!("{engine}.agents 必须为包内路径数组")))?;
        for path in paths {
            let files = markdown_under(&contained(root, &path)?);
            if files.is_empty() {
                return Err(PluginError::invalid(format!("{engine}.agents 入口不存在")));
            }
            for file in files {
                let (config, _) = instruction_file(&file)?;
                let name = text_of(config.get("name")).unwrap_or_else(|| stem(&file));
                if !component_name(&name) {
                    return Err(PluginError::invalid("插件组件名称无效"));
                }
                components.push(json!({
                    "kind": "agents", "name": name, "path": relative(root, &file),
                    "description": text_of(config.get("description")).unwrap_or_else(|| name.clone()),
                    "metadata": config, "engines": [engine], "delivery": "native_agent",
                }));
            }
        }
    }
    let hooks = manifest
        .get("hooks")
        .filter(|v| truthy(Some(v)))
        .cloned()
        .or_else(|| root.join("hooks/hooks.json").is_file().then(|| json!("hooks/hooks.json")));
    if let Some(hooks) = hooks {
        let mut merged: Map<String, Value> = Map::new();
        let values = match hooks {
            Value::Array(items) => items,
            other => vec![other],
        };
        for mut value in values {
            if let Value::String(reference) = &value {
                value = read_json(&contained(root, reference)?)?;
            }
            let Some(object) = value.as_object() else {
                return Err(PluginError::invalid("hooks 必须为 JSON 对象或包内路径"));
            };
            let events = object.get("hooks").cloned().unwrap_or_else(|| value.clone());
            for (event, entries) in events.as_object().cloned().unwrap_or_default() {
                let Some(entries) = entries.as_array() else {
                    return Err(PluginError::invalid("Hook 事件必须为数组"));
                };
                let slot = merged.entry(event).or_insert_with(|| json!([]));
                slot.as_array_mut().unwrap().extend(entries.iter().cloned());
            }
        }
        let commands_only = merged.values().flat_map(|v| v.as_array().cloned().unwrap_or_default()).all(|row| {
            row["hooks"].as_array().into_iter().flatten().all(|hook| hook["type"] == "command")
        });
        components.push(json!({
            "kind": "hooks", "name": "hooks", "config": {"hooks": merged},
            "engines": if commands_only { command_hook_engines() } else { hook_engines() },
            "delivery": "native_hooks",
        }));
    }
    let extension_abi = extension_engines();
    let mut extensions = match meta.get("extensions") {
        None | Some(Value::Null) => Map::new(),
        Some(Value::Object(map)) => map.clone(),
        Some(value) if !truthy(Some(value)) => Map::new(),
        Some(_) => return Err(PluginError::invalid("muteki.extensions 必须按引擎声明入口")),
    };
    for engine in &extension_abi {
        let Some(spec) = manifest.get(engine).and_then(Value::as_object) else { continue };
        let Some(entries) = spec.get("extensions").filter(|v| v.is_array()) else { continue };
        let mut dependencies = Map::new();
        for key in ["dependencies", "devDependencies"] {
            if let Some(map) = manifest.get(key).and_then(Value::as_object) {
                dependencies.extend(map.clone());
            }
        }
        let native_engine =
            if engine == "pi" && dependencies.contains_key("@oh-my-pi/pi-coding-agent") { "omp" } else { engine.as_str() };
        extensions.entry(native_engine.to_owned()).or_insert_with(|| entries.clone());
    }
    if extensions.is_empty() && root.join("extensions").is_dir() {
        let candidates: Vec<&String> = declared_engines.iter().filter(|e| extension_abi.contains(e)).collect();
        if candidates.len() == 1 {
            let mut entries: Vec<String> = std::fs::read_dir(root.join("extensions"))
                .into_iter()
                .flatten()
                .flatten()
                .map(|entry| entry.path())
                .filter(|p| p.extension().is_some_and(|e| e == "js" || e == "ts" || e == "mjs"))
                .map(|p| relative(root, &p))
                .collect();
            entries.sort();
            extensions.insert(candidates[0].clone(), json!(entries));
        } else {
            components.push(json!({
                "kind": "extensions", "name": "extensions", "engines": [],
                "reason": "请在插件 muteki.extensions 中声明对应引擎和入口；不会猜测执行 ABI",
            }));
        }
    }
    for (engine, paths) in &extensions {
        let paths = string_list(paths)
            .filter(|_| extension_abi.contains(engine))
            .ok_or_else(|| PluginError::invalid("可执行扩展支持 Pi、OMP、OpenCode 的原生入口"))?;
        for path in paths {
            let mut file = contained(root, &path)?;
            if file.is_dir() {
                if let Some(index) = ["index.ts", "index.js", "index.mjs"].iter().map(|n| file.join(n)).find(|p| p.is_file()) {
                    file = index;
                }
            }
            let suffix_ok = file.extension().is_some_and(|e| e == "js" || e == "ts" || e == "mjs");
            if !file.is_file() || !suffix_ok {
                return Err(PluginError::invalid("可执行扩展入口不存在"));
            }
            let name = if stem(&file) != "index" {
                stem(&file)
            } else {
                file.parent().and_then(Path::file_name).unwrap_or_default().to_string_lossy().into_owned()
            };
            components.push(json!({
                "kind": "extensions", "name": name, "path": relative(root, &file),
                "engines": [engine], "delivery": "native_extension",
            }));
        }
    }
    let requirements = meta.get("requires").cloned().unwrap_or_else(|| json!({}));
    let Some(requirement_map) = requirements.as_object() else {
        return Err(PluginError::invalid("requires 必须为对象"));
    };
    for key in ["executables", "env", "files", "tools"] {
        if let Some(value) = requirement_map.get(key)
            && string_list(value).is_none()
        {
            return Err(PluginError::invalid(format!("requires.{key} 必须为字符串数组")));
        }
    }
    let environment = meta.get("env").cloned().unwrap_or_else(|| json!({}));
    if !string_map(&environment) {
        return Err(PluginError::invalid("插件环境变量必须为字符串映射"));
    }
    let mut seen = BTreeSet::new();
    let allowed_modes: Vec<String> = allowed_modes.into_iter().filter(|m| seen.insert(m.clone())).collect();
    Ok(json!({
        "components": components, "requirements": requirements,
        "declared_engines": explicit_engines, "environment": environment,
        "allowed_modes": allowed_modes,
    }))
}

/// Validate an `mcpServers` object. With a package root, path templates must
/// name files inside it.
pub fn validate_mcp(servers: &Value, root: Option<&Path>) -> Result<()> {
    let Some(servers) = servers.as_object().filter(|s| s.len() <= 20) else {
        return Err(PluginError::invalid("MCP 配置必须是最多 20 个服务的 mcpServers 对象"));
    };
    for (name, config) in servers {
        let Some(cfg) = config.as_object().filter(|_| mcp_server_name(name)) else {
            return Err(PluginError::invalid("MCP 服务名称或配置无效"));
        };
        let url = cfg.get("url").and_then(Value::as_str).unwrap_or("");
        if !truthy(cfg.get("command")) && !(url.starts_with("https://") || url.starts_with("http://")) {
            return Err(PluginError::invalid("MCP 需要 command 或 HTTP URL"));
        }
        if truthy(cfg.get("args")) && string_list(&cfg["args"]).is_none() {
            return Err(PluginError::invalid("MCP args 必须是字符串数组"));
        }
        if truthy(cfg.get("command")) && !cfg["command"].is_string() {
            return Err(PluginError::invalid("MCP command 必须是字符串"));
        }
        for key in ["env", "headers"] {
            if let Some(value) = cfg.get(key)
                && !string_map(value)
            {
                return Err(PluginError::invalid(format!("MCP {key} 必须是字符串映射")));
            }
        }
        if let Some(value) = cfg.get("network")
            && string_list(value).is_none()
        {
            return Err(PluginError::invalid("MCP network 必须是数组"));
        }
        let command = cfg.get("command").and_then(Value::as_str).unwrap_or("").to_owned();
        let args = cfg.get("args").and_then(string_list).unwrap_or_default();
        for (index, argument) in std::iter::once(command).chain(args).enumerate() {
            let value = argument.split_once('=').map_or(argument.as_str(), |(_, v)| v);
            if let Some(token) = PATH_TOKENS.iter().find(|t| value.starts_with(**t)) {
                let suffix = &value[token.len()..];
                if !suffix.is_empty() && !suffix.starts_with('/') {
                    return Err(PluginError::new("chat_plugin.mcp_path_invalid", "MCP 路径模板后必须是 / 或结束"));
                }
                let data = *token == "${PLUGIN_DATA}";
                let base = if data { PathBuf::from("/muteki-plugin-data") } else { root.map(Path::to_path_buf).unwrap_or_default() };
                if !data && root.is_none() {
                    continue;
                }
                let target = normalize(&base.join(suffix.trim_start_matches('/')));
                if !target.starts_with(normalize(&base)) {
                    return Err(PluginError::new("chat_plugin.mcp_path_invalid", "MCP 路径不得越过扩展包或私有数据目录"));
                }
                if !data && !target.exists() {
                    return Err(PluginError::new(
                        "chat_plugin.mcp_resource_missing",
                        "MCP 包内资源不存在；请导入包含脚本和资源的扩展包，并以 ${PLUGIN_ROOT}/... 引用",
                    ));
                }
            } else if index > 0 && value.starts_with('/') {
                return Err(PluginError::new(
                    "chat_plugin.mcp_path_outside_package",
                    "MCP 参数引用包外绝对路径；请将脚本和资源导入同一扩展包，并以 ${PLUGIN_ROOT}/... 引用",
                ));
            } else if value.contains("${") {
                return Err(PluginError::new(
                    "chat_plugin.mcp_path_invalid",
                    "MCP 路径模板无效，仅支持 ${PLUGIN_ROOT} 或 ${PLUGIN_DATA}",
                ));
            }
        }
    }
    Ok(())
}

fn strings_in(value: &Value) -> Vec<String> {
    string_list(value).unwrap_or_default()
}

fn skill_usable(record: &Value, skill: &Value, engine: &str, native: &[String]) -> bool {
    (skill["requires_native"] != true || native.iter().any(|e| e == engine))
        && (!truthy(Some(&skill["hooks"])) || super::hooks_approved(record))
}

/// Per-engine availability of a package's components in chat.
pub fn compatibility(record: &Value, verified: Option<&HashMap<String, HashSet<String>>>, check_tools: bool) -> Value {
    let requirements = &record["requirements"];
    let mut missing: Vec<String> = strings_in(&requirements["executables"])
        .into_iter()
        .filter(|name| which(name).is_none())
        .map(|name| format!("缺少程序 {name}"))
        .collect();
    let mut configured_env: Map<String, Value> = record["environment"].as_object().cloned().unwrap_or_default();
    for config in record["mcp"].as_object().into_iter().flat_map(|m| m.values()) {
        if let Some(env) = config["env"].as_object() {
            configured_env.extend(env.clone());
        }
    }
    missing.extend(
        strings_in(&requirements["env"])
            .into_iter()
            .filter(|name| !truthy(configured_env.get(name)))
            .map(|name| format!("插件未配置环境变量 {name}")),
    );
    let root = PathBuf::from(record["root"].as_str().unwrap_or("."));
    missing.extend(
        strings_in(&requirements["files"])
            .into_iter()
            .filter(|name| !contained(&root, name).is_ok_and(|p| p.is_file()))
            .map(|name| format!("缺少包内文件 {name}")),
    );
    let tools = strings_in(&requirements["tools"]);
    let declared = match record.get("declared_engines") {
        Some(value) if value.is_array() => strings_in(value),
        _ => engines(),
    };
    let native = native_skill_engines();
    let mut result = Map::new();
    for engine in engines() {
        let mut reasons = missing.clone();
        if !declared.contains(&engine) {
            reasons.push("插件未声明支持此引擎".into());
        }
        if check_tools {
            let have = verified.and_then(|v| v.get(&engine));
            let mut absent: Vec<&String> = tools.iter().filter(|t| !have.is_some_and(|h| h.contains(*t))).collect();
            absent.sort();
            absent.dedup();
            if !absent.is_empty() {
                reasons.push(format!(
                    "当前会话尚未确认所需工具：{}",
                    absent.iter().map(|s| s.as_str()).collect::<Vec<_>>().join("、")
                ));
            }
        }
        let mut available: BTreeSet<String> = BTreeSet::new();
        let mut unavailable: BTreeSet<String> = BTreeSet::new();
        let skills = record["skills"].as_array().cloned().unwrap_or_default();
        if skills.iter().any(|skill| skill_usable(record, skill, &engine, &native)) {
            available.insert("skills".into());
        }
        if !native.contains(&engine) && skills.iter().any(|skill| skill["requires_native"] == true) {
            unavailable.insert("Claude 专属 Skill 语义".into());
        }
        if record["mcp"].as_object().is_some_and(|m| !m.is_empty()) {
            if gateway_tools(&engine) {
                available.insert("mcp".into());
            } else {
                unavailable.insert("聊天 Gateway 工具尚未接入".into());
            }
        }
        for component in record["components"].as_array().into_iter().flatten() {
            let kind = component["kind"].as_str().unwrap_or("").to_owned();
            let needs_approval =
                kind == "hooks" || kind == "skill_hooks" || truthy(Some(&component["metadata"]["hooks"]));
            let approved = !needs_approval || super::hooks_approved(record);
            if strings_in(&component["engines"]).contains(&engine) && approved {
                available.insert(kind);
            } else {
                unavailable.insert(kind);
            }
        }
        for (server, config) in record["mcp"].as_object().into_iter().flatten() {
            let command = config["command"].as_str().unwrap_or("");
            if !command.is_empty() && !command.contains("${") && which(command).is_none() {
                reasons.push(format!("MCP {server} 缺少启动程序 {command}"));
            }
        }
        let status = if !reasons.is_empty() || available.is_empty() {
            "blocked"
        } else if !unavailable.is_empty() || !tools.is_empty() {
            "partial"
        } else {
            "supported"
        };
        result.insert(
            engine,
            json!({
                "status": status,
                "components": if reasons.is_empty() { available.into_iter().collect::<Vec<_>>() } else { Vec::new() },
                "unavailable": unavailable.into_iter().collect::<Vec<_>>(),
                "reasons": reasons, "required_tools": tools,
            }),
        );
    }
    Value::Object(result)
}

/// Every hook command a package would run, shown before native hooks are approved.
pub fn hook_commands(record: &Value) -> Vec<String> {
    fn collect(value: &Value, out: &mut Vec<String>) {
        match value {
            Value::Object(map) => {
                match map.get("type").and_then(Value::as_str) {
                    Some("command") => out.push(map.get("command").and_then(Value::as_str).unwrap_or("").to_owned()),
                    Some(kind @ ("prompt" | "agent")) => {
                        out.push(format!("{kind}: {}", map.get("prompt").and_then(Value::as_str).unwrap_or("")))
                    }
                    _ => {}
                }
                for item in map.values() {
                    collect(item, out);
                }
            }
            Value::Array(items) => items.iter().for_each(|item| collect(item, out)),
            _ => {}
        }
    }
    let mut out = Vec::new();
    for item in record["components"].as_array().into_iter().flatten() {
        if item["kind"] == "hooks" || item["kind"] == "skill_hooks" {
            collect(&item["config"], &mut out);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(root: &Path, relative: &str, text: &str) {
        let path = root.join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    #[test]
    fn a_skill_package_is_named_by_its_manifest_and_lists_its_skills() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "plugin.json", r#"{"name":"demo-pack","version":"1.2.0","description":"Demo"}"#);
        write(dir.path(), "skills/alpha/SKILL.md", "---\nname: alpha\ndescription: First\n---\nbody");
        write(dir.path(), "skills/beta/SKILL.md", "---\ndescription: Second\nallowed-tools: [Bash]\n---\nbody");
        let record = inspect(dir.path()).unwrap();
        assert_eq!(record["id"], "demo-pack");
        assert_eq!(record["version"], "1.2.0");
        let skills = record["skills"].as_array().unwrap();
        assert_eq!(skills.len(), 2);
        assert_eq!(skills[0]["name"], "alpha");
        assert_eq!(skills[0]["requires_native"], false);
        assert_eq!(skills[1]["name"], "beta", "the directory names a skill without a declared name");
        assert_eq!(skills[1]["requires_native"], true);
        assert_eq!(record["allowed_modes"], json!(["chat", "ctf", "pentest"]));
    }

    #[test]
    fn a_package_without_a_manifest_or_skill_is_refused_by_code() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "README.md", "nothing here");
        assert_eq!(inspect(dir.path()).unwrap_err().code, "chat_plugin.not_a_plugin");
    }

    #[test]
    fn mcp_paths_must_stay_inside_the_package() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "server.py", "print()");
        let inside = json!({"s":{"command":"python3","args":["${PLUGIN_ROOT}/server.py"]}});
        assert!(validate_mcp(&inside, Some(dir.path())).is_ok());
        let missing = json!({"s":{"command":"python3","args":["${PLUGIN_ROOT}/absent.py"]}});
        assert_eq!(validate_mcp(&missing, Some(dir.path())).unwrap_err().code, "chat_plugin.mcp_resource_missing");
        let escape = json!({"s":{"command":"python3","args":["${PLUGIN_ROOT}/../x.py"]}});
        assert_eq!(validate_mcp(&escape, Some(dir.path())).unwrap_err().code, "chat_plugin.mcp_path_invalid");
        let outside = json!({"s":{"command":"python3","args":["/etc/server.py"]}});
        assert_eq!(validate_mcp(&outside, Some(dir.path())).unwrap_err().code, "chat_plugin.mcp_path_outside_package");
        assert!(validate_mcp(&json!({"s":{"url":"ftp://x"}}), None).is_err());
        assert!(validate_mcp(&json!({"s":{"url":"https://example.com/mcp","headers":{"A":"b"}}}), None).is_ok());
    }

    #[test]
    fn mcp_declarations_merge_from_files_and_manifest() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "plugin.json", r#"{"name":"tools","mcpServers":{"remote":{"url":"https://example.com/mcp"}}}"#);
        write(dir.path(), ".mcp.json", r#"{"mcpServers":{"local":{"command":"node","args":["${PLUGIN_ROOT}/s.js"]}}}"#);
        write(dir.path(), "s.js", "");
        let record = inspect(dir.path()).unwrap();
        let names: Vec<&String> = record["mcp"].as_object().unwrap().keys().collect();
        assert_eq!(names, ["local", "remote"]);
        assert_eq!(record["description"], "MCP");
    }

    #[test]
    fn inline_command_templates_become_files_and_skills() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "plugin.json", r#"{"name":"cmds","commands":{"review":{"content":"Review the diff","description":"Review"}}}"#);
        let record = inspect(dir.path()).unwrap();
        assert!(dir.path().join(".muteki-commands/review.md").is_file());
        let skill = &record["skills"][0];
        assert_eq!(skill["name"], "review");
        assert_eq!(skill["component_kind"], "command");
        assert_eq!(record["native_components"], json!(["commands"]));
    }

    #[test]
    fn hooks_need_approval_of_the_exact_digest() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "plugin.json", r#"{"name":"hooked","hooks":{"hooks":{"PreToolUse":[{"hooks":[{"type":"command","command":"echo hi"}]}]}}}"#);
        write(dir.path(), "skills/a/SKILL.md", "---\nname: a\n---\nx");
        let mut record = inspect(dir.path()).unwrap();
        record["digest"] = json!("d1");
        assert_eq!(hook_commands(&record), ["echo hi"]);
        let blocked = compatibility(&record, None, false);
        assert!(blocked["claude"]["unavailable"].as_array().unwrap().contains(&json!("hooks")));
        record["hooks_approved_digest"] = json!("d1");
        let approved = compatibility(&record, None, false);
        assert!(approved["claude"]["components"].as_array().unwrap().contains(&json!("hooks")));
    }
}
