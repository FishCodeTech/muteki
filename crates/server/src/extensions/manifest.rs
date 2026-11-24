use super::{ExtError, Result};
use regex::Regex;
use serde_json::{Value, json};
use std::{
    collections::HashSet,
    fs,
    path::{Component, Path, PathBuf},
    sync::OnceLock,
};

pub const CORE_VERSION: &str = "1.0.0";
pub const PLUGIN_SCHEMA: &str = "https://agent-plugins.org/schemas/1.0.0/plugin.schema.json";
pub const NAMESPACE: &str = "io.github.fishcodetech.muteki";

fn valid_id(id: &str) -> bool {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^[a-z0-9](?:[a-z0-9.-]*[a-z0-9])?$").unwrap())
        .is_match(id)
        && !id.contains("--")
        && !id.contains("..")
}

#[cfg(test)]
mod plugin_name_tests {
    use super::valid_id;

    #[test]
    fn accepts_portable_agent_plugin_names() {
        for name in ["x", "muteki-visualize", "acme.tools"] {
            assert!(valid_id(name), "{name}");
        }
        for name in ["", "_legacy", "My-Plugin", "two--hyphens", "two..dots"] {
            assert!(!valid_id(name), "{name}");
        }
    }
}

fn semver(text: &str) -> Option<(u64, u64, u64)> {
    static RE: OnceLock<Regex> = OnceLock::new();
    if !RE
        .get_or_init(|| Regex::new(r"^\d+\.\d+\.\d+$").unwrap())
        .is_match(text)
    {
        return None;
    }
    let mut parts = text.split('.').filter_map(|part| part.parse::<u64>().ok());
    Some((parts.next()?, parts.next()?, parts.next()?))
}

pub fn version_cmp(left: &str, right: &str) -> std::cmp::Ordering {
    semver(left).cmp(&semver(right))
}

fn range_satisfies(requirement: &str, version: &str) -> bool {
    let Some(target) = semver(version) else {
        return false;
    };
    let re = Regex::new(r"^(>=|<=|==|!=|>|<)\s*(\d+(?:\.\d+){0,2})$").unwrap();
    !requirement.trim().is_empty()
        && requirement.split(',').all(|raw| {
            let Some(caps) = re.captures(raw.trim()) else {
                return false;
            };
            let mut parts = caps[2].split('.').filter_map(|v| v.parse::<u64>().ok());
            let bound = (
                parts.next().unwrap_or(0),
                parts.next().unwrap_or(0),
                parts.next().unwrap_or(0),
            );
            match &caps[1] {
                ">=" => target >= bound,
                "<=" => target <= bound,
                "==" => target == bound,
                "!=" => target != bound,
                ">" => target > bound,
                "<" => target < bound,
                _ => false,
            }
        })
}

fn string<'a>(obj: &'a Value, field: &str) -> Result<&'a str> {
    obj[field]
        .as_str()
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .ok_or_else(|| {
            ExtError::validation(
                "manifest.invalid_fields",
                format!("{field} must be nonempty text"),
            )
        })
}

fn strings(obj: &Value, field: &str) -> Result<Vec<String>> {
    let Some(values) = obj[field].as_array() else {
        return Err(ExtError::validation(
            "manifest.invalid_fields",
            format!("{field} must be an array"),
        ));
    };
    values
        .iter()
        .map(|v| {
            v.as_str()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
                .ok_or_else(|| {
                    ExtError::validation(
                        "manifest.invalid_fields",
                        format!("{field} entries must be nonempty text"),
                    )
                })
        })
        .collect()
}

pub fn relative_file(root: &Path, raw: &str, field: &str) -> Result<PathBuf> {
    let rel = Path::new(raw);
    if !raw.starts_with("./")
        || rel.components().any(|c| {
            matches!(
                c,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
    {
        return Err(ExtError::validation(
            "manifest.unsafe_path",
            format!("{field} must be a plugin-relative path"),
        ));
    }
    let root = root
        .canonicalize()
        .map_err(|e| ExtError::validation("manifest.unreadable", e.to_string()))?;
    let path = root.join(rel).canonicalize().map_err(|_| {
        ExtError::validation("manifest.missing_file", format!("{field} is missing"))
    })?;
    if !path.starts_with(&root) || !path.is_file() {
        return Err(ExtError::validation(
            "manifest.unsafe_path",
            format!("{field} escapes package root"),
        ));
    }
    Ok(path)
}

pub fn load(root: &Path) -> Result<Value> {
    let file = relative_file(root, "./plugin.json", "plugin.json")?;
    let raw = fs::read_to_string(file)
        .map_err(|e| ExtError::validation("manifest.unreadable", e.to_string()))?;
    let plugin: Value = serde_json::from_str(&raw)
        .map_err(|e| ExtError::validation("manifest.parse_error", e.to_string()))?;
    if !plugin.is_object() {
        return Err(ExtError::validation(
            "manifest.invalid_shape",
            "plugin.json root must be an object",
        ));
    }
    for field in ["description", "homepage", "repository", "license"] {
        if plugin.get(field).is_some_and(|value| !value.is_string()) {
            return Err(ExtError::validation(
                "manifest.invalid_plugin_field",
                format!("{field} must be a string"),
            ));
        }
    }
    if plugin.get("author").is_some_and(|value| !value.is_object())
        || plugin["author"].as_object().is_some_and(|obj| {
            obj.iter().any(|(key, value)| {
                !["name", "email", "url"].contains(&key.as_str()) || !value.is_string()
            })
        })
    {
        return Err(ExtError::validation(
            "manifest.invalid_plugin_field",
            "author must contain only name/email/url strings",
        ));
    }
    if plugin.get("keywords").is_some_and(|value| {
        value
            .as_array()
            .is_none_or(|items| items.iter().any(|item| !item.is_string()))
    }) {
        return Err(ExtError::validation(
            "manifest.invalid_plugin_field",
            "keywords must be an array of strings",
        ));
    }
    if plugin["$schema"] != PLUGIN_SCHEMA {
        return Err(ExtError::validation(
            "manifest.unsupported_plugin_schema",
            "unsupported Agent Plugins schema",
        ));
    }
    let id = string(&plugin, "name")?;
    if !valid_id(id) || id.len() > 64 {
        return Err(ExtError::validation(
            "manifest.invalid_id",
            "Agent Plugin name must use lowercase letters, digits, hyphens or dots without adjacent hyphens or dots",
        ));
    }
    let client = plugin["extensions"][NAMESPACE].as_object().ok_or_else(|| {
        ExtError::validation(
            "manifest.missing_muteki_extension",
            "Muteki extension namespace is missing",
        )
    })?;
    let client = Value::Object(client.clone());
    let allowed = [
        "manifest_version",
        "extension_version",
        "requires_core",
        "origin",
        "retain_previous_versions",
        "entrypoints",
        "provides",
        "requires",
        "permissions",
        "config_schema",
        "state_schema",
        "ui",
    ];
    let unsupported = client
        .as_object()
        .into_iter()
        .flat_map(|obj| obj.keys())
        .filter(|key| !allowed.contains(&key.as_str()))
        .cloned()
        .collect::<Vec<_>>();
    if !unsupported.is_empty() {
        return Err(ExtError::validation(
            "manifest.invalid_muteki_fields",
            format!("不支持的 Muteki 平台扩展字段：{}；聊天插件请在“Agent 扩展”中管理", unsupported.join(", ")),
        ));
    }
    if client["manifest_version"].as_u64().unwrap_or(1) != 1 {
        return Err(ExtError::validation(
            "manifest.unsupported_manifest_version",
            "manifest_version must be 1",
        ));
    }
    let version = string(&client, "extension_version")?;
    if semver(version).is_none() {
        return Err(ExtError::validation(
            "manifest.invalid_version",
            "extension_version must be strict semver",
        ));
    }
    let plugin_version = string(&plugin, "version")?;
    if semver(plugin_version).is_none() {
        return Err(ExtError::validation(
            "manifest.invalid_version",
            "plugin version must be strict semver",
        ));
    }
    let requires_core = string(&client, "requires_core")?;
    if !range_satisfies(requires_core, CORE_VERSION) {
        return Err(ExtError::validation(
            "manifest.core_version_incompatible",
            "requires_core excludes this host",
        ));
    }
    let origin = client["origin"].as_str().unwrap_or("installed");
    if !["builtin", "verified", "community", "installed"].contains(&origin) {
        return Err(ExtError::validation(
            "manifest.invalid_origin",
            "unknown origin",
        ));
    }
    let entry = &client["entrypoints"];
    if !entry.is_object() {
        return Err(ExtError::validation(
            "manifest.invalid_fields",
            "entrypoints must be an object",
        ));
    }
    let host = entry["host"].as_str().unwrap_or("subprocess");
    if host != "subprocess" {
        return Err(ExtError::validation(
            "manifest.in_process_not_allowed",
            "third-party extensions must use subprocess",
        ));
    }
    let command = strings(entry, "command")?;
    if command.is_empty() {
        return Err(ExtError::validation(
            "manifest.missing_command",
            "subprocess command is empty",
        ));
    }
    let permissions = &client["permissions"];
    if client.get("permissions").is_some_and(|v| !v.is_object()) {
        return Err(ExtError::validation(
            "manifest.invalid_fields",
            "permissions must be an object",
        ));
    }
    if client
        .get("retain_previous_versions")
        .is_some_and(|v| !v.is_boolean())
    {
        return Err(ExtError::validation(
            "manifest.invalid_fields",
            "retain_previous_versions must be boolean",
        ));
    }
    let mut normalized = json!({"filesystem":[],"network":[],"secrets":[],"events_write":[]});
    for field in ["filesystem", "network", "secrets", "events_write"] {
        if permissions.get(field).is_some() {
            normalized[field] = json!(strings(permissions, field)?);
        }
    }
    for token in normalized["filesystem"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
    {
        if ![
            "workspace-read",
            "workspace-write",
            "state-read",
            "state-write",
        ]
        .contains(&token)
        {
            return Err(ExtError::validation(
                "manifest.invalid_filesystem_permission",
                format!("unknown token {token}"),
            ));
        }
    }
    let secret_re = Regex::new(r"^(secret://)?[a-z0-9_-]+(?:/[A-Za-z0-9_.-]+){1,2}$").unwrap();
    for reference in normalized["secrets"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
    {
        if !secret_re.is_match(reference) {
            return Err(ExtError::validation(
                "manifest.invalid_secret_ref",
                "secret reference has invalid shape",
            ));
        }
    }
    for host in normalized["network"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
    {
        if host == "*"
            || host.is_empty()
            || host
                .chars()
                .any(|c| c.is_whitespace() || c == '/' || c == ':')
        {
            return Err(ExtError::validation(
                "manifest.invalid_network_permission",
                "network entries must be hostnames",
            ));
        }
    }
    for pattern in normalized["events_write"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
    {
        if !pattern.starts_with(&format!("ext.{id}.")) {
            return Err(ExtError::validation(
                "manifest.invalid_events_write",
                "event namespace leaves extension id",
            ));
        }
    }
    let mut seen = HashSet::new();
    if client.get("provides").is_some_and(|v| !v.is_array())
        || client.get("requires").is_some_and(|v| !v.is_array())
    {
        return Err(ExtError::validation(
            "manifest.invalid_fields",
            "provides and requires must be arrays",
        ));
    }
    let provides = client["provides"].as_array().cloned().unwrap_or_default();
    for item in &provides {
        let kind = string(item, "type")?;
        if ![
            "domain-module",
            "run-executor",
            "external-agent-adapter",
            "platform-adapter",
            "workspace-provider",
            "graph-service",
            "policy-gate",
            "data-connection",
            "ui-contribution",
            "agent-capability-binding",
            "tool-integration",
            "protocol-binding",
        ]
        .contains(&kind)
        {
            return Err(ExtError::validation(
                "manifest.invalid_provide_type",
                format!("unsupported provide type {kind}"),
            ));
        }
        let key = (kind.to_owned(), string(item, "id")?.to_owned());
        if item["api_version"].as_u64().unwrap_or(1) == 0 || !seen.insert(key) {
            return Err(ExtError::validation(
                "manifest.duplicate_provide",
                "invalid or duplicate provided capability",
            ));
        }
    }
    let requires = client["requires"].as_array().cloned().unwrap_or_default();
    for item in &requires {
        string(item, "capability")?;
        if item["version"].as_u64().unwrap_or(1) == 0 {
            return Err(ExtError::validation(
                "manifest.invalid_require_version",
                "requires version must be positive",
            ));
        }
    }
    let namespace_root = root.join(NAMESPACE).canonicalize().ok();
    for field in ["config_schema", "state_schema", "ui"] {
        if client
            .get(field)
            .is_some_and(|v| !v.is_null() && !v.is_string())
        {
            return Err(ExtError::validation(
                "manifest.invalid_fields",
                format!("{field} must be a path string"),
            ));
        }
        if let Some(path) = client[field].as_str() {
            let resolved = relative_file(root, path, field)?;
            if !namespace_root
                .as_ref()
                .is_some_and(|base| resolved.starts_with(base))
            {
                return Err(ExtError::validation(
                    "manifest.invalid_client_extension_path",
                    format!("{field} must be inside {NAMESPACE}/"),
                ));
            }
        }
    }
    for (index, item) in command.iter().enumerate() {
        if item.starts_with("./") {
            let resolved = relative_file(root, item, &format!("entrypoints.command[{index}]"))?;
            if !namespace_root
                .as_ref()
                .is_some_and(|base| resolved.starts_with(base))
            {
                return Err(ExtError::validation(
                    "manifest.invalid_client_extension_path",
                    "entrypoint must be in Muteki namespace directory",
                ));
            }
        }
    }
    let mut manifest = json!({
        "plugin_schema":PLUGIN_SCHEMA,"client_namespace":NAMESPACE,"manifest_version":1,
        "id":id,"plugin_version":plugin_version,"version":version,
        "description":plugin["description"],"author":plugin["author"],"homepage":plugin["homepage"],
        "repository":plugin["repository"],"license":plugin["license"],"keywords":plugin["keywords"],
        "requires_core":requires_core,"origin":origin,
        "retain_previous_versions":client["retain_previous_versions"].as_bool().unwrap_or(true),
        "entrypoints":{"host":"subprocess","command":command},
        "provides":provides,"requires":requires,"permissions":normalized,
        "config_schema":client["config_schema"],"state_schema":client["state_schema"],"ui":client["ui"],
    });
    for field in [
        "description",
        "author",
        "homepage",
        "repository",
        "license",
        "keywords",
    ] {
        if manifest[field].is_null() {
            manifest[field] = match field {
                "author" => json!({}),
                "keywords" => json!([]),
                _ => json!(""),
            };
        }
    }
    if let Some(ui) = manifest["ui"].as_str() {
        load_ui(root, ui)?;
    }
    if let Some(schema) = manifest["config_schema"].as_str() {
        load_schema(root, schema)?;
    }
    Ok(manifest)
}

pub fn load_schema(root: &Path, relative: &str) -> Result<Value> {
    let path = relative_file(root, relative, "config_schema")?;
    let value: Value = serde_json::from_slice(
        &fs::read(path)
            .map_err(|e| ExtError::validation("manifest.invalid_schema", e.to_string()))?,
    )
    .map_err(|e| ExtError::validation("manifest.invalid_schema", e.to_string()))?;
    if !value.is_object() {
        return Err(ExtError::validation(
            "manifest.invalid_schema",
            "schema root must be object",
        ));
    }
    Ok(value)
}

pub fn validate_schema(value: &Value, schema: &Value, field: &str) -> Result<()> {
    let type_ok = match schema["type"].as_str() {
        Some("object") => value.is_object(),
        Some("array") => value.is_array(),
        Some("string") => value.is_string(),
        Some("integer") => value.is_i64() || value.is_u64(),
        Some("number") => value.is_number(),
        Some("boolean") => value.is_boolean(),
        Some("null") => value.is_null(),
        _ => true,
    };
    if !type_ok {
        return Err(ExtError::validation(
            "extension.config_invalid",
            format!("{field} has invalid type"),
        ));
    }
    if schema["enum"]
        .as_array()
        .is_some_and(|allowed| !allowed.contains(value))
    {
        return Err(ExtError::validation(
            "extension.config_invalid",
            format!("{field} is not an allowed value"),
        ));
    }
    if let Some(obj) = value.as_object() {
        for required in schema["required"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
        {
            if !obj.contains_key(required) {
                return Err(ExtError::validation(
                    "extension.config_invalid",
                    format!("{field}.{required} is required"),
                ));
            }
        }
        let props = schema["properties"].as_object();
        if schema["additionalProperties"] == false
            && obj
                .keys()
                .any(|key| !props.is_some_and(|map| map.contains_key(key)))
        {
            return Err(ExtError::validation(
                "extension.config_invalid",
                format!("{field} has unknown keys"),
            ));
        }
        if let Some(props) = props {
            for (key, child) in props {
                if let Some(item) = obj.get(key) {
                    validate_schema(item, child, &format!("{field}.{key}"))?;
                }
            }
        }
    }
    if let (Some(items), Some(child)) = (
        value.as_array(),
        schema.get("items").filter(|v| v.is_object()),
    ) {
        for (index, item) in items.iter().enumerate() {
            validate_schema(item, child, &format!("{field}[{index}]"))?;
        }
    }
    Ok(())
}

pub fn load_ui(root: &Path, relative: &str) -> Result<Value> {
    let path = relative_file(root, relative, "ui")?;
    let metadata = fs::metadata(&path)
        .map_err(|e| ExtError::validation("extension.ui_unreadable", e.to_string()))?;
    if metadata.len() > 64 * 1024 {
        return Err(ExtError::validation(
            "extension.ui_too_large",
            "UI contribution exceeds 64 KiB",
        ));
    }
    let value: Value = serde_json::from_slice(
        &fs::read(&path)
            .map_err(|e| ExtError::validation("extension.ui_unreadable", e.to_string()))?,
    )
    .map_err(|e| ExtError::validation("extension.ui_unreadable", e.to_string()))?;
    if !value.is_object() || value["schema_version"] != 1 {
        return Err(ExtError::validation(
            "extension.ui_schema_version",
            "UI schema_version must be 1",
        ));
    }
    let allowed = [
        "schema_version",
        "navigation",
        "command_forms",
        "status_labels",
        "board",
        "artifact_viewers",
    ];
    fn scan(value: &Value, count: &mut usize) -> bool {
        if let Some(obj) = value.as_object() {
            for (key, item) in obj {
                if [
                    "script",
                    "scripts",
                    "javascript",
                    "srcdoc",
                    "html",
                    "dangerouslySetInnerHTML",
                ]
                .contains(&key.as_str())
                    || !scan(item, count)
                {
                    return false;
                }
            }
        } else if let Some(items) = value.as_array() {
            *count += items.len();
            for item in items {
                if !scan(item, count) {
                    return false;
                }
            }
        }
        true
    }
    let mut count = 0;
    if !scan(&value, &mut count) || count > 64 {
        return Err(ExtError::validation(
            "extension.ui_script_denied",
            "UI contains script fields or too many items",
        ));
    }
    if value
        .as_object()
        .is_some_and(|obj| obj.keys().any(|key| !allowed.contains(&key.as_str())))
    {
        return Err(ExtError::validation(
            "extension.ui_component_denied",
            "unsupported UI contribution field",
        ));
    }
    for field in ["navigation", "command_forms", "artifact_viewers"] {
        if value.get(field).is_some_and(|v| !v.is_array()) {
            return Err(ExtError::validation(
                "extension.ui_invalid",
                format!("{field} must be an array"),
            ));
        }
    }
    Ok(value)
}
