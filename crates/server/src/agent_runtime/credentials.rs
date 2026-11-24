//! Credential selection at the session launch boundary.
//!
//! A conversation names a stable credential id: `system:<engine>` for the
//! host's own CLI login, `account:<id>` for an account registered in Muteki.
//! The environment resolved here is handed to the child process only; it is
//! never persisted, and an explicit account never borrows the host's ambient
//! credentials.

use super::{RuntimeError, descriptors::EngineDescriptor, host_login};
use crate::store::Store;
use serde_json::{Value, json};
use sha2::{Digest, Sha512};
use std::{
    collections::HashMap,
    io::Write,
    path::Path,
    sync::{Mutex, OnceLock},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

fn quota_cooldowns() -> &'static Mutex<HashMap<String, (Instant, u64)>> {
    static MAP: OnceLock<Mutex<HashMap<String, (Instant, u64)>>> = OnceLock::new();
    MAP.get_or_init(|| Mutex::new(HashMap::new()))
}

pub fn record_quota_cooldown(credential_id: &str, resets_at: u64) {
    if credential_id.trim().is_empty() {
        return;
    }
    let now_unix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let secs = resets_at.saturating_sub(now_unix).clamp(60, 3600);
    let until = Instant::now() + Duration::from_secs(secs);
    quota_cooldowns()
        .lock()
        .unwrap()
        .insert(credential_id.to_owned(), (until, resets_at));
}

pub fn quota_cooldown_snapshots() -> Vec<Value> {
    let now_i = Instant::now();
    quota_cooldowns()
        .lock()
        .unwrap()
        .iter()
        .filter(|(_, (until, _))| *until > now_i)
        .map(|(cred, (until, resets_at))| {
            json!({
                "credential_id": cred,
                "remaining_secs": until.duration_since(now_i).as_secs(),
                "resets_at": resets_at,
            })
        })
        .collect()
}

pub fn clear_quota_cooldowns(filter: Option<&str>) -> usize {
    let mut map = quota_cooldowns().lock().unwrap();
    let before = map.len();
    if let Some(f) = filter.map(str::trim).filter(|v| !v.is_empty()) {
        map.retain(|k, _| !k.contains(f));
    } else {
        map.clear();
    }
    before.saturating_sub(map.len())
}

pub fn failover_unpinned_credential(store: &Store, engine: &str, current_id: &str) -> Option<String> {
    let now_i = Instant::now();
    let cooled = {
        let mut map = quota_cooldowns().lock().unwrap();
        map.retain(|_, (until, _)| *until > now_i);
        map.contains_key(current_id)
    };
    if !cooled {
        return None;
    }
    let rows = store.list("credential_accounts").ok()?;
    let guard = quota_cooldowns().lock().unwrap();
    for row in rows {
        if row["disabled"] == true || row["enabled"] == false {
            continue;
        }
        let matches_engine = row["engine"].as_str() == Some(engine)
            || row["details"]["target_engine"].as_str() == Some(engine);
        if !matches_engine {
            continue;
        }
        let Some(acct_id) = row["id"]
            .as_str()
            .or_else(|| row["account_id"].as_str())
            .map(str::trim)
            .filter(|v| !v.is_empty())
        else {
            continue;
        };
        let cand = format!("account:{acct_id}");
        if cand != current_id && !guard.get(&cand).is_some_and(|(until, _)| *until > now_i) {
            return Some(cand);
        }
    }
    None
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CredentialRef {
    System(String),
    Account(String),
}

impl CredentialRef {
    pub fn id(&self) -> String {
        match self {
            Self::System(engine) => format!("system:{engine}"),
            Self::Account(id) => format!("account:{id}"),
        }
    }
}

fn valid_account_id(value: &str) -> bool {
    let mut chars = value.chars();
    chars.next().is_some_and(|c| c.is_ascii_alphanumeric())
        && value.len() <= 64
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'))
}

/// Normalize a credential reference; an empty one means the host login.
pub fn canonical(value: &str, engine: &str) -> Result<CredentialRef, RuntimeError> {
    let text = value.trim();
    let text = text.strip_prefix("secret://credential-accounts/").unwrap_or(text);
    if text.is_empty() {
        return Ok(CredentialRef::System(engine.to_owned()));
    }
    if let Some(selected) = text.strip_prefix("system:") {
        let selected = selected.trim().to_ascii_lowercase();
        if selected != engine {
            return Err(RuntimeError::new(
                "credential.engine_mismatch",
                format!("凭据 system:{selected} 不能用于 {engine}"),
            ));
        }
        return Ok(CredentialRef::System(selected));
    }
    let id = text.strip_prefix("account:").unwrap_or(text).trim();
    if !valid_account_id(id) {
        return Err(RuntimeError::new("credential.invalid", "凭据标识无效"));
    }
    Ok(CredentialRef::Account(id.to_owned()))
}

#[derive(Debug, Clone, Default)]
pub struct Resolved {
    pub credential_id: String,
    pub env: Vec<(String, String)>,
}

fn text(row: &Value, key: &str) -> Option<String> {
    row[key].as_str().map(str::trim).filter(|s| !s.is_empty()).map(str::to_owned)
}

fn file(base: Option<&Path>, name: &str) -> Option<String> {
    let value = std::fs::read_to_string(base?.join(name)).ok()?;
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_owned())
}

fn dir(base: Option<&Path>, name: &str) -> Option<String> {
    let path = base?.join(name);
    path.is_dir()
        .then(|| std::fs::canonicalize(&path).unwrap_or(path).to_string_lossy().into_owned())
}

pub(crate) fn private_dir(path: &Path) -> Result<(), RuntimeError> {
    std::fs::create_dir_all(path)
        .map_err(|e| RuntimeError::new("runtime.io_error", format!("无法创建凭据状态目录：{e}")))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
            .map_err(|e| RuntimeError::new("runtime.io_error", format!("无法保护凭据状态目录：{e}")))?;
    }
    Ok(())
}

pub(crate) fn private_file(path: &Path, contents: &[u8]) -> Result<(), RuntimeError> {
    let temporary = path.with_extension(format!("{}.tmp", uuid::Uuid::new_v4().simple()));
    let write = || -> std::io::Result<()> {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut handle = options.open(&temporary)?;
        handle.write_all(contents)?;
        handle.sync_all()?;
        std::fs::rename(&temporary, path)?;
        Ok(())
    };
    let result = write();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result.map_err(|e| RuntimeError::new("runtime.io_error", format!("无法写入 Agent 凭据配置：{e}")))
}

fn capability_error(message: impl Into<String>) -> RuntimeError {
    RuntimeError::new("credential.model_capabilities_required", message)
}

fn optional_json(path: &Path, label: &str) -> Result<Option<Value>, RuntimeError> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(capability_error(format!("cannot read {label}: {error}"))),
    };
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(|error| capability_error(format!("invalid {label}: {error}")))
}

fn capability_integer(value: &Value, model: &str, field: &str) -> Result<u64, RuntimeError> {
    let parsed = match value {
        Value::Bool(true) => Some(1i128),
        Value::Bool(false) => Some(0i128),
        Value::Number(number) => number.as_i64().map(i128::from)
            .or_else(|| number.as_u64().map(i128::from))
            .or_else(|| number.as_f64().filter(|number| number.is_finite()
                && *number > i128::MIN as f64 && *number < i128::MAX as f64)
                .map(|number| number.trunc() as i128)),
        Value::String(text) => text.trim().parse::<i128>().ok(),
        _ => None,
    }.ok_or_else(|| capability_error(format!("model_capabilities[{model}].{field} must be an integer")))?;
    if parsed <= 0 {
        return Err(capability_error(format!("model_capabilities[{model}].{field} must be positive")));
    }
    u64::try_from(parsed).map_err(|_| capability_error(format!(
        "model_capabilities[{model}].{field} must be an integer")))
}

fn falsey(value: &Value) -> bool {
    match value {
        Value::Null | Value::Bool(false) => true,
        Value::Number(number) => number.as_f64() == Some(0.0),
        Value::String(text) => text.is_empty(),
        Value::Array(values) => values.is_empty(),
        Value::Object(values) => values.is_empty(),
        _ => false,
    }
}

pub(crate) fn normalize_model_capabilities(value: &Value) -> Result<serde_json::Map<String, Value>, RuntimeError> {
    let rows = value.as_object().ok_or_else(|| capability_error(
        "model_capabilities must be an object keyed by model id"))?;
    let mut normalized = serde_json::Map::new();
    for (raw_model, raw_capability) in rows {
        let model = raw_model.trim();
        let fields = raw_capability.as_object().filter(|_| !model.is_empty()).ok_or_else(||
            capability_error("each model_capabilities entry must name a model and an object"))?;
        let mut entry = serde_json::Map::new();
        for (field, alias) in [("context_window", "contextWindow"), ("max_output_tokens", "maxTokens")] {
            if let Some(raw) = fields.get(field).or_else(|| fields.get(alias)).filter(|raw| !raw.is_null()) {
                entry.insert(field.to_owned(), json!(capability_integer(raw, model, field)?));
            }
        }
        let default_input = json!(["text"]);
        let raw_input = fields.get("input").filter(|value| !falsey(value)).unwrap_or(&default_input);
        let inputs = raw_input.as_array().filter(|inputs| !inputs.is_empty()).ok_or_else(||
            capability_error(format!("model_capabilities[{model}].input must be a non-empty string list")))?;
        let mut input = Vec::<String>::new();
        for raw in inputs {
            let value = raw.as_str().map(str::trim).filter(|value| !value.is_empty()).ok_or_else(||
                capability_error(format!("model_capabilities[{model}].input must be a non-empty string list")))?;
            if !input.iter().any(|known| known == value) { input.push(value.to_owned()); }
        }
        entry.insert("input".to_owned(), json!(input));
        if let Some(raw) = fields.get("reasoning") {
            let value = raw.as_bool().ok_or_else(|| capability_error(format!(
                "model_capabilities[{model}].reasoning must be boolean")))?;
            entry.insert("reasoning".to_owned(), json!(value));
        }
        let map = fields.get("thinkingLevelMap").or_else(|| fields.get("thinking_level_map"));
        if let Some(raw) = map.filter(|raw| !raw.is_null()) {
            let rows = raw.as_object().ok_or_else(|| capability_error(format!(
                "model_capabilities[{model}].thinkingLevelMap must be an object")))?;
            let mut levels = serde_json::Map::new();
            for (name, wire) in rows {
                let level = if name == "none" { "off" } else { name };
                if !["off", "minimal", "low", "medium", "high", "xhigh", "max"].contains(&level) {
                    return Err(capability_error(format!(
                        "model_capabilities[{model}].thinkingLevelMap has an invalid level")));
                }
                if !wire.is_null() && !wire.is_string() {
                    return Err(capability_error(format!(
                        "model_capabilities[{model}].thinkingLevelMap values must be strings or null")));
                }
                if levels.get(level).is_some_and(|existing| existing != wire) {
                    return Err(capability_error(format!(
                        "model_capabilities[{model}].thinkingLevelMap has conflicting off/none aliases")));
                }
                levels.insert(level.to_owned(), wire.clone());
            }
            entry.insert("thinkingLevelMap".to_owned(), Value::Object(levels));
        }
        normalized.insert(model.to_owned(), Value::Object(entry));
    }
    Ok(normalized)
}

pub(crate) fn pi_compaction_settings(context_window: Option<u64>) -> Value {
    let window = context_window.unwrap_or(128_000);
    let reserve = ((window as f64 * 0.15).round() as u64).clamp(16_384, 65_536);
    let keep_recent = ((window as f64 * 0.06).round() as u64).clamp(6_000, 20_000);
    json!({
        "enabled": true,
        "reserveTokens": reserve,
        "keepRecentTokens": keep_recent
    })
}

pub(crate) fn pi_model_capabilities(base: Option<&Path>, row: &Value, model: &str) -> Result<Value, RuntimeError> {
    let imported = match base {
        Some(base) => optional_json(&base.join("MODEL_CAPABILITIES.json"), "MODEL_CAPABILITIES.json")?,
        None => None,
    };
    let explicit_row = match row.get("model_capabilities").filter(|value| !value.is_null()) {
        Some(value) => normalize_model_capabilities(value)?.remove(model),
        None => None,
    };
    if let Some(capabilities) = explicit_row {
        return Ok(capabilities);
    }
    let explicit_imported = match imported.as_ref() {
        Some(value) => normalize_model_capabilities(value)?.remove(model),
        None => None,
    };
    if let Some(capabilities) = explicit_imported {
        return Ok(capabilities);
    }
    let endpoint = text(row, "base_url").or_else(|| file(base, "BASE_URL"));
    if matches!(endpoint.as_deref().map(|url| url.trim_end_matches('/')),
        Some("https://ark.cn-beijing.volces.com/api/plan"
            | "https://ark.cn-beijing.volces.com/api/plan/v3"
            | "https://qianfan.baidubce.com/v2/tokenplan/personal"))
    {
        let known = match model {
            "kimi-k3" => Some((65_536, vec!["text", "image"])),
            "glm-5.3" => Some((65_536, vec!["text"])),
            "deepseek-v4.1-flash" => Some((393_216, vec!["text", "image"])),
            "deepseek-v4-flash" | "deepseek-v4-pro" => Some((393_216, vec!["text"])),
            _ => None,
        };
        if let Some((max_output_tokens, input)) = known {
            return Ok(json!({"context_window":1_024_000,"max_output_tokens":max_output_tokens,"input":input}));
        }
    }
    let generated = match base {
        Some(base) => optional_json(&base.join("pi-agent/models.json"), "existing Pi model metadata")?,
        None => None,
    };
    for provider in generated.as_ref().and_then(|value| value["providers"].as_object()).into_iter().flatten().map(|(_, provider)| provider) {
        for candidate in provider["models"].as_array().into_iter().flatten() {
            if candidate["id"] != model { continue; }
            let mut one = serde_json::Map::new();
            one.insert(model.to_owned(), candidate.clone());
            if let Ok(mut normalized) = normalize_model_capabilities(&Value::Object(one)) {
                let capability = normalized.remove(model).unwrap_or(Value::Null);
                if capability["context_window"] == 128_000 && capability["max_output_tokens"] == 8_192 {
                    return Ok(Value::Null);
                }
                return Ok(capability);
            }
            return Ok(Value::Null);
        }
    }
    Ok(Value::Null)
}

pub(crate) fn runtime_model_image_input(
    store: &Store,
    state_dir: &Path,
    runtime: &Value,
) -> Option<bool> {
    let adapter_id = runtime["adapter_id"].as_str().map(str::trim).unwrap_or("");
    let (engine, _) = super::descriptors::for_adapter(adapter_id)?;
    let model = runtime["model"].as_str().map(str::trim).filter(|m| !m.is_empty())?;
    let model_id = model.strip_prefix("muteki/").unwrap_or(model);
    let credential_raw = runtime["credential_id"].as_str().map(str::trim).unwrap_or("");
    let credential = canonical(credential_raw, engine.engine()).ok()?;
    if let CredentialRef::Account(account_id) = &credential
        && let Some(row) = account(store, account_id)
    {
        let base = crate::account_material::base(state_dir,account_id,&row).ok()?;
        let base = base.is_dir().then_some(base);
        if let Ok(caps) = pi_model_capabilities(base.as_deref(), &row, model_id)
            && let Some(inputs) = caps["input"].as_array()
            && !inputs.is_empty()
        {
            return Some(inputs.iter().any(|v| v.as_str() == Some("image")));
        }
    }
    let cred_id = credential.id();
    for catalog in store.list("model_catalogs").ok().into_iter().flatten() {
        if catalog["credential_id"] != cred_id {
            continue;
        }
        for entry in catalog["discovered_models"].as_array().into_iter().flatten() {
            if (entry["id"] == model || entry["id"] == model_id)
                && let Some(inputs) = entry["input"].as_array()
                && !inputs.is_empty()
            {
                return Some(inputs.iter().any(|v| v.as_str() == Some("image")));
            }
        }
    }
    None
}

fn pi_account_env(
    state_dir: &Path,
    account_id: &str,
    row: &Value,
    base: Option<&Path>,
    secret: Option<&str>,
    base_url: Option<&str>,
    model: Option<&str>,
) -> Result<Vec<(String, String)>, RuntimeError> {
    let mut env = Vec::new();
    if let Some(base_url) = base_url {
        let api_key = secret.filter(|s| !s.is_empty()).unwrap_or("muteki-keyless");
        let selected = model
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
            .or_else(|| text(row, "default_model"))
            .or_else(|| {
                row["models"]
                    .as_array()
                    .and_then(|rows| rows.first())
                    .and_then(|v| v.as_str().or_else(|| v["id"].as_str()))
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_owned)
            })
            .or_else(|| file(base, "MODEL"))
            .ok_or_else(|| {
                RuntimeError::new("credential.model_required", "Pi 自定义端点必须指定模型 ID")
            })?;
        let model_id = selected.strip_prefix("muteki/").unwrap_or(&selected);
        let capabilities = pi_model_capabilities(base, row, model_id)?;
        let mut definition = json!({
            "id":model_id,
            "input":capabilities["input"].as_array().cloned().unwrap_or_else(||vec![json!("text")]),
            "reasoning":capabilities["reasoning"].as_bool().unwrap_or(false)
        });
        for (source, target) in [
            ("context_window", "contextWindow"),
            ("contextWindow", "contextWindow"),
            ("max_output_tokens", "maxTokens"),
            ("maxTokens", "maxTokens"),
            ("thinkingLevelMap", "thinkingLevelMap"),
        ] {
            if let Some(value) = capabilities.get(source) {
                definition[target] = value.clone();
            }
        }
        let compaction = pi_compaction_settings(definition["contextWindow"].as_u64());
        let models = json!({"providers":{"muteki":{
            "baseUrl":base_url,"api":"openai-completions","apiKey":api_key,
            "models":[definition]
        }}});
        let settings = json!({
            "httpIdleTimeoutMs":300_000,
            "compaction":compaction,
            "retry":{"provider":{"timeoutMs":300_000,"maxRetries":0}}
        });
        let root = state_dir.join("runtime-credentials").join(account_id);
        let identity = Sha512::digest(model_id.as_bytes());
        let profile = root.join("pi-agent").join(
            format!("{:x}", identity)
                .chars()
                .take(32)
                .collect::<String>(),
        );
        for dir in [
            state_dir.join("runtime-credentials"),
            root,
            profile.parent().unwrap().to_owned(),
            profile.clone(),
        ] {
            private_dir(&dir)?;
        }
        private_file(
            &profile.join("models.json"),
            format!("{}\n", models).as_bytes(),
        )?;
        private_file(
            &profile.join("settings.json"),
            format!("{}\n", settings).as_bytes(),
        )?;
        env.push((
            "PI_CODING_AGENT_DIR".into(),
            profile.to_string_lossy().into_owned(),
        ));
        env.push(("MUTEKI_PI_PROVIDER".into(), "muteki".into()));
        env.push(("MUTEKI_PI_MODEL".into(), model_id.to_owned()));
    } else if let Some(secret) = secret {
        env.push(("OPENAI_API_KEY".into(), secret.to_owned()));
    }
    Ok(env)
}

fn account_model(model: Option<&str>, row: &Value, base: Option<&Path>, fallback: &str) -> String {
    model.map(str::trim).filter(|value| !value.is_empty()).map(str::to_owned)
        .or_else(|| text(row, "default_model"))
        .or_else(|| file(base, "MODEL"))
        .unwrap_or_else(|| fallback.to_owned())
}

fn account_profile(state_dir: &Path, id: &str, engine: &str, model: &str) -> Result<std::path::PathBuf, RuntimeError> {
    let root = state_dir.join("runtime-credentials").join(id);
    let digest = Sha512::digest(model.as_bytes());
    let profile = root.join(format!("{engine}-agent"))
        .join(format!("{:x}", digest).chars().take(32).collect::<String>());
    for dir in [state_dir.join("runtime-credentials"), root, profile.parent().unwrap().to_owned(), profile.clone()] {
        private_dir(&dir)?;
    }
    Ok(profile)
}

fn omp_account_env(state_dir: &Path, id: &str, row: &Value, base: Option<&Path>,
    secret: Option<&str>, base_url: Option<&str>, model: Option<&str>) -> Result<Vec<(String,String)>, RuntimeError> {
    let mut env = Vec::new();
    if let Some(base_url) = base_url {
        let api_key = secret.filter(|s| !s.is_empty()).unwrap_or("muteki-keyless");
        let selected = account_model(model,row,base,"");
        if selected.is_empty() {
            return Err(RuntimeError::new("credential.model_required", "OMP 自定义端点必须指定模型 ID"));
        }
        let model_id = selected.strip_prefix("muteki/").unwrap_or(&selected);
        let capabilities = pi_model_capabilities(base,row,model_id)?;
        let mut config = format!("providers:\n  muteki:\n    baseUrl: {}\n    api: openai-completions\n    apiKey: {}\n    models:\n      - id: {}\n",
            json!(base_url),json!(api_key),json!(model_id));
        for (source,target) in [("context_window","contextWindow"),("max_output_tokens","maxTokens")] {
            if let Some(value)=capabilities[source].as_u64() {
                config.push_str(&format!("        {target}: {value}\n"));
            }
        }
        let profile=account_profile(state_dir,id,"omp",model_id)?;
        private_file(&profile.join("models.yml"),config.as_bytes())?;
        env.push(("PI_CODING_AGENT_DIR".into(),profile.to_string_lossy().into_owned()));
        env.push(("MUTEKI_OMP_PROVIDER".into(),"muteki".into()));
        env.push(("MUTEKI_OMP_MODEL".into(),model_id.to_owned()));
        env.push(("OPENAI_BASE_URL".into(),base_url.to_owned()));
        env.push(("OPENAI_API_KEY".into(),api_key.to_owned()));
    } else if let Some(secret)=secret {
        env.push(("OPENAI_API_KEY".into(),secret.to_owned()));
    }
    Ok(env)
}

fn not_wired(engine: &EngineDescriptor) -> RuntimeError {
    RuntimeError::unsupported(format!(
        "{} 的已登记凭据尚未接入原生会话；请改用本机宿主登录",
        engine.identity.display_name
    ))
}

pub fn account(store: &Store, id: &str) -> Option<Value> {
    store.get("credential_accounts", id).ok().flatten().or_else(|| {
        store
            .list("credential_accounts")
            .ok()?
            .into_iter()
            .find(|row| row["account_id"] == id || row["id"] == id)
    })
}

async fn system(engine: &EngineDescriptor) -> Result<Resolved, RuntimeError> {
    let id = engine.engine();
    let mut env = Vec::new();
    for key in engine.public()["credentials"]["env_keys"].as_array().into_iter().flatten().filter_map(Value::as_str) {
        let Some(target) = key.strip_suffix("_FILE") else { continue; };
        if std::env::var(target).is_ok_and(|value| !value.trim().is_empty()) { continue; }
        if let Ok(path) = std::env::var(key) && !path.trim().is_empty() {
            let value = std::fs::read_to_string(&path)
                .map_err(|error| RuntimeError::new("credential.material_invalid", format!("cannot read {key}: {error}")))?;
            if value.trim().is_empty() {
                return Err(RuntimeError::new("credential.material_invalid", format!("{key} contains no credential")));
            }
            env.push((target.to_owned(), value.trim().to_owned()));
        }
    }
    let detected = host_login::detect(id, true).await;
    let status = if detected != host_login::DISABLED && !env.is_empty() { host_login::PRESENT } else { detected };
    let guidance = &engine.login.guidance_command;
    match status {
        host_login::DISABLED => Err(RuntimeError::new(
            "host_discovery_disabled",
            "此服务未提供宿主登录发现；请选择已登记凭据",
        )),
        host_login::ABSENT => Err(RuntimeError::new(
            "credential.host_login_missing",
            format!("宿主 {id} 登录态不可用；请先在本机执行 {guidance}"),
        )),
        host_login::UNKNOWN if engine.login.system_login_only => Err(RuntimeError::new(
            "credential.host_login_missing",
            format!("请先在本机执行 {guidance}"),
        )),
        _ => Ok(Resolved {
            credential_id: format!("system:{id}"),
            env,
        }),
    }
}

pub async fn resolve(
    store: &Store,
    state_dir: &Path,
    engine: &EngineDescriptor,
    credential_id: &str,
    model: Option<&str>,
) -> Result<Resolved, RuntimeError> {
    let name = engine.engine();
    let id = match canonical(credential_id, name)? {
        CredentialRef::System(_) => return system(engine).await,
        CredentialRef::Account(id) => id,
    };
    if engine.login.system_login_only {
        return Err(RuntimeError::new(
            "credential.system_login_only",
            format!("{} 仅支持本机系统登录凭据", engine.identity.display_name),
        ));
    }
    let row = account(store, &id)
        .ok_or_else(|| RuntimeError::new("credential.unavailable", format!("凭据账号不可用：{id}")))?;
    let owner = text(&row["details"], "target_engine")
        .or_else(|| text(&row, "worker_engine"))
        .or_else(|| text(&row, "engine"))
        .unwrap_or_default()
        .to_ascii_lowercase();
    if !["", "api", "unknown", name].contains(&owner.as_str()) {
        return Err(RuntimeError::new(
            "credential.engine_mismatch",
            format!("凭据 account:{id} 属于 {owner}，不能用于 {name}"),
        ));
    }
    let base = crate::account_material::base(state_dir,&id,&row)
        .map_err(|error|RuntimeError::new("credential.material_invalid",error.to_string()))?;
    let base = base.is_dir().then_some(base);
    let base = base.as_deref();
    let secret = text(&row, "secret").or_else(|| file(base, "API_KEY"));
    let base_url = text(&row, "base_url").or_else(|| file(base, "BASE_URL"));
    let mut env: Vec<(String, String)> = Vec::new();
    let mut set = |key: &str, value: String| env.push((key.to_owned(), value));
    match name {
        "pi" => {
            env.extend(pi_account_env(state_dir, &id, &row, base, secret.as_deref(), base_url.as_deref(), model)?);
        }
        "omp" => {
            env.extend(omp_account_env(state_dir, &id, &row, base, secret.as_deref(), base_url.as_deref(), model)?);
        }
        "claude" => match &secret {
            Some(secret) => {
                set("ANTHROPIC_API_KEY", secret.clone());
                set("ANTHROPIC_AUTH_TOKEN", secret.clone());
                if let Some(url) = &base_url {
                    set("ANTHROPIC_BASE_URL", url.clone());
                }
            }
            None => {
                if let Some(token) = file(base, "CLAUDE_CODE_OAUTH_TOKEN") {
                    set("CLAUDE_CODE_OAUTH_TOKEN", token);
                }
            }
        },
        "codex" => match &secret {
            Some(secret) => {
                set("OPENAI_API_KEY", secret.clone());
                if let Some(url) = &base_url {
                    set("OPENAI_BASE_URL", url.clone());
                }
            }
            None => {
                if let Some(home) = dir(base, "codex-home") {
                    set("CODEX_HOME", home);
                }
            }
        },
        "cursor" => {
            if let Some(secret) = secret.clone().or_else(|| file(base, "CURSOR_API_KEY")) {
                set("CURSOR_API_KEY", secret);
                if let Some(url) = &base_url {
                    set("CURSOR_ENDPOINT", url.clone());
                }
            }
        }
        "grok" => match &secret {
            Some(secret) => {
                set("XAI_API_KEY", secret.clone());
                if let Some(url) = &base_url {
                    set("GROK_MODELS_BASE_URL", url.clone());
                }
            }
            None => {
                if let Some(home) = dir(base, "grok-home") {
                    set("GROK_HOME", home);
                }
            }
        },
        "opencode" => {
            if secret.is_some() || base_url.is_some() {
                let api_key = secret.clone().unwrap_or_else(|| "muteki-keyless".to_owned());
                set("OPENAI_API_KEY", api_key.clone());
                set("OPENCODE_API_KEY", api_key);
                let digest = Sha512::digest(model.unwrap_or("").as_bytes());
                let root = state_dir.join("runtime-credentials").join(&id).join("opencode")
                    .join(format!("{:x}", digest).chars().take(32).collect::<String>());
                for sub in ["data", "config", "cache", "state"] {
                    private_dir(&root.join(sub))?;
                }
                if let Some(base_url) = &base_url {
                    set("OPENAI_BASE_URL", base_url.clone());
                    let selected_model = account_model(model, &row, base, "");
                    {
                        let selected_model = selected_model.strip_prefix("muteki/").unwrap_or(&selected_model);
                        let provider_name = text(&row, "provider")
                            .filter(|value| !value.is_empty())
                            .unwrap_or_else(|| "Muteki 凭据中心".to_owned());
                        let mut models = serde_json::Map::new();
                        let imported_models = base.and_then(|dir| file(Some(dir), "MODELS.json"))
                            .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
                            .unwrap_or(Value::Null);
                        for entry in imported_models.as_array().into_iter().flatten()
                            .chain(row["models"].as_array().into_iter().flatten()) {
                            let id = entry.as_str().or_else(|| entry["id"].as_str()).unwrap_or("").trim();
                            if !id.is_empty() {
                                models.insert(id.to_owned(), json!({"name":entry["name"].as_str().unwrap_or(id)}));
                            }
                        }
                        if !selected_model.is_empty() {
                            models.insert(selected_model.to_owned(), json!({"name":selected_model}));
                        }
                        let mut config = json!({"$schema":"https://opencode.ai/config.json",
                            "providers":{"muteki":{"package":"@opencode/ai/providers/openai-compatible",
                                "name":provider_name,"env":["OPENCODE_API_KEY"],
                                "settings":{"baseURL":base_url,"apiKey":"{env:OPENCODE_API_KEY}"},"models":models}}});
                        if !selected_model.is_empty() {
                            config["model"] = json!(format!("muteki/{selected_model}"));
                        }
                        set("OPENCODE_CONFIG_CONTENT", config.to_string());
                        set("MUTEKI_OPENCODE_PROVIDER", "muteki".to_owned());
                        set("MUTEKI_OPENCODE_MODEL", selected_model.to_owned());
                        let config_dir = root.join("config").join("opencode");
                        private_dir(&config_dir)?;
                        let config_path = config_dir.join("opencode.json");
                        private_file(&config_path, config.to_string().as_bytes())?;
                        set("OPENCODE_CONFIG", config_path.to_string_lossy().into_owned());
                    }
                }
                let root = root.to_string_lossy().into_owned();
                set("XDG_DATA_HOME", format!("{root}/data"));
                set("XDG_CONFIG_HOME", format!("{root}/config"));
                set("XDG_CACHE_HOME", format!("{root}/cache"));
                set("XDG_STATE_HOME", format!("{root}/state"));
            }
        }
        "droid" => {
            if base_url.is_some() {
                return Err(RuntimeError::unsupported("Droid 凭据不支持自定义 API 端点；请使用 Factory API Key"));
            }
            if let Some(secret) = &secret {
                set("FACTORY_API_KEY", secret.clone());
            }
        }
        "kimi" => {
            if let Some(secret) = &secret {
                let selected = account_model(model, &row, base, "kimi-for-coding");
                let selected = selected.strip_prefix("kimi-code/").unwrap_or(&selected);
                let capabilities = pi_model_capabilities(base, &row, selected)?;
                let provider_type = if text(&row, "provider").as_deref() == Some("kimi") { "kimi" }
                    else if base_url.is_some() { "openai" } else { "kimi" };
                set("KIMI_MODEL_API_KEY", secret.clone());
                if let Some(url) = &base_url { set("KIMI_MODEL_BASE_URL", url.clone()); }
                set("KIMI_MODEL_NAME", selected.to_owned());
                set("KIMI_MODEL_PROVIDER_TYPE", provider_type.to_owned());
                if let Some(window)=capabilities["context_window"].as_u64() {
                    set("KIMI_MODEL_MAX_CONTEXT_SIZE",window.to_string());
                }
                if provider_type == "openai"
                    && let Some(tokens)=capabilities["max_output_tokens"].as_u64() {
                    set("KIMI_MODEL_MAX_OUTPUT_SIZE",tokens.to_string());
                }
                let mut tags=vec!["tool_use"];
                if capabilities["reasoning"] == true { tags.push("thinking"); }
                if capabilities["input"].as_array().is_some_and(|inputs|inputs.iter().any(|v|v=="image")) { tags.push("image_in"); }
                if capabilities["input"].as_array().is_some_and(|inputs|inputs.iter().any(|v|v=="video")) { tags.push("video_in"); }
                if provider_type == "kimi" { tags.push("dynamically_loaded_tools"); }
                set("KIMI_MODEL_CAPABILITIES",tags.join(","));
                let profile=account_profile(state_dir,&id,"kimi",selected)?;
                let mut config=String::new();
                if let Some(window)=capabilities["context_window"].as_u64() {
                    config.push_str(&format!("[loop_control]\ncompaction_trigger_ratio = 0.9\nreserved_context_size = {}\n\n",
                        ((window as f64 * 0.10).round() as u64).max(16_384)));
                }
                config.push_str("[experimental]\nnotify_user = true\nsubagent_fork = true\nwait_for = true\ntower = false\n\"tool-select\" = true\n");
                private_file(&profile.join("config.toml"),config.as_bytes())?;
                set("KIMI_CODE_HOME",profile.to_string_lossy().into_owned());
            } else if let Some(home) = dir(base, "kimi-home") {
                set("KIMI_CODE_HOME", home);
            }
        }
        _ => return Err(not_wired(engine)),
    }
    if env.is_empty() {
        return Err(RuntimeError::new(
            "credential.unavailable",
            format!("凭据账号 {id} 没有可用于 {} 的密钥或登录目录", engine.identity.display_name),
        ));
    }
    let mut out: Vec<(String, String)> = engine.public()["credentials"]["env_keys"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .filter(|key| !key.ends_with("_FILE"))
        .map(|key| (key.to_owned(), String::new()))
        .collect();
    out.extend(env);
    if let Some(revision) = row["revision"].as_str() {
        out.push(("MUTEKI_CREDENTIAL_CONFIGURATION_REVISION".into(),revision.into()));
    }
    Ok(Resolved {
        credential_id: format!("account:{id}"),
        env: out,
    })
}

/// Host variables a configured runtime instance references with `env:NAME`.
pub fn env_refs(refs: &Value) -> Result<Vec<(String, String)>, RuntimeError> {
    let mut out = Vec::new();
    for (name, reference) in refs.as_object().into_iter().flatten() {
        let reference = reference.as_str().unwrap_or("").trim();
        if reference.is_empty() {
            continue;
        }
        if let Some(host) = reference.strip_prefix("env:") {
            out.push((name.clone(), std::env::var(host.trim()).unwrap_or_default()));
        } else {
            return Err(RuntimeError::unsupported(format!(
                "环境引用 {name} 使用 {reference}；此版本只解析 env: 引用"
            )));
        }
    }
    Ok(out)
}

/// Resolve Runtime environment references only when launching a session.
/// Platform SecretStore handles remain opaque in stored Runtime settings.
pub fn env_refs_with_platform(state: &crate::AppState, refs: &Value) -> Result<Vec<(String, String)>, RuntimeError> {
    let mut out = Vec::new();
    for (name, reference) in refs.as_object().into_iter().flatten() {
        let reference = reference.as_str().unwrap_or("").trim();
        if reference.is_empty() { continue; }
        let value = if let Some(host) = reference.strip_prefix("env:") {
            std::env::var(host.trim()).map_err(|_|RuntimeError::new("runtime.env_ref_unresolved",format!("Runtime 环境引用无法解析：{name}")))?
        } else if reference.starts_with("secret://platform/") {
            crate::runs::competition_api::resolve_platform_secret(state,reference)
                .map_err(|_|RuntimeError::new("runtime.env_ref_unresolved",format!("Runtime 环境引用无法解析：{name}")))?
        } else {
            return Err(RuntimeError::new("runtime.env_ref_unresolved",format!("Runtime 环境引用无法解析：{name}")));
        };
        out.push((name.clone(),value));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_runtime::descriptors::find;
    use serde_json::json;

    #[test]
    fn references_normalize_and_refuse_a_foreign_engine() {
        assert_eq!(canonical("", "codex").unwrap(), CredentialRef::System("codex".into()));
        assert_eq!(canonical("system:codex", "codex").unwrap(), CredentialRef::System("codex".into()));
        assert_eq!(canonical("secret://credential-accounts/work", "codex").unwrap(), CredentialRef::Account("work".into()));
        assert_eq!(canonical("account:work", "codex").unwrap(), CredentialRef::Account("work".into()));
        assert_eq!(canonical("system:claude", "codex").unwrap_err().code, "credential.engine_mismatch");
        assert_eq!(canonical("account:../x", "codex").unwrap_err().code, "credential.invalid");
    }

    #[tokio::test]
    async fn an_account_resolves_to_engine_variables_over_a_reset_overlay() {
        let store = Store::open(":memory:").unwrap();
        store.put("credential_accounts", "work", &json!({"id":"work","account_id":"work","engine":"claude","secret":"synthetic","base_url":"https://example.invalid"})).unwrap();
        let state = std::env::temp_dir().join(format!("muteki-cred-{}", uuid::Uuid::new_v4()));
        let resolved = resolve(&store, &state, find("claude").unwrap(), "account:work", None).await.unwrap();
        assert_eq!(resolved.credential_id, "account:work");
        let last = |key: &str| resolved.env.iter().rev().find(|(k, _)| k == key).map(|(_, v)| v.as_str());
        assert_eq!(last("ANTHROPIC_API_KEY"), Some("synthetic"));
        assert_eq!(last("ANTHROPIC_BASE_URL"), Some("https://example.invalid"));
        assert_eq!(last("CLAUDE_CODE_OAUTH_TOKEN"), Some(""));
        let error = resolve(&store, &state, find("codex").unwrap(), "account:work", None).await.unwrap_err();
        assert_eq!(error.code, "credential.engine_mismatch");
        let error = resolve(&store, &state, find("devin").unwrap(), "account:work", None).await.unwrap_err();
        assert_eq!(error.code, "credential.system_login_only");
        let error = resolve(&store, &state, find("claude").unwrap(), "account:missing", None).await.unwrap_err();
        assert_eq!(error.code, "credential.unavailable");
    }

    #[tokio::test]
    async fn pi_account_projects_a_private_model_profile_for_the_selected_model() {
        let store = Store::open(":memory:").unwrap();
        store
            .put(
                "credential_accounts",
                "pi-ollama",
                &json!({
                    "account_id":"pi-ollama", "engine":"api", "details":{"target_engine":"pi"},
                    "secret":"synthetic-pi-key", "base_url":"https://example.invalid/v1",
                    "default_model":"older-model"
                }),
            )
            .unwrap();
        let state =
            std::env::temp_dir().join(format!("muteki-pi-credential-{}", uuid::Uuid::new_v4()));
        let result = resolve(
            &store,
            &state,
            find("pi").unwrap(),
            "account:pi-ollama",
            Some("glm-5.3-flash:cloud"),
        )
        .await
        .unwrap();
        let env = |key: &str| {
            result
                .env
                .iter()
                .rev()
                .find(|(name, _)| name == key)
                .map(|(_, value)| value.as_str())
        };
        assert_eq!(env("MUTEKI_PI_PROVIDER"), Some("muteki"));
        assert_eq!(env("MUTEKI_PI_MODEL"), Some("glm-5.3-flash:cloud"));
        assert_eq!(env("OPENAI_API_KEY"), Some(""));
        let profile = std::path::Path::new(env("PI_CODING_AGENT_DIR").unwrap());
        let models: Value =
            serde_json::from_slice(&std::fs::read(profile.join("models.json")).unwrap()).unwrap();
        let settings: Value =
            serde_json::from_slice(&std::fs::read(profile.join("settings.json")).unwrap()).unwrap();
        assert_eq!(models["providers"]["muteki"]["api"], "openai-completions");
        assert_eq!(
            models["providers"]["muteki"]["models"][0]["id"],
            "glm-5.3-flash:cloud"
        );
        assert_eq!(models["providers"]["muteki"]["apiKey"], "synthetic-pi-key");
        assert_eq!(settings["retry"]["provider"]["maxRetries"], 0);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(profile).unwrap().permissions().mode() & 0o777,
                0o700
            );
            assert_eq!(
                std::fs::metadata(profile.join("models.json"))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
            assert_eq!(
                std::fs::metadata(profile.join("settings.json"))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
        std::fs::remove_dir_all(&state).unwrap();
    }

    #[test]
    fn only_host_variable_references_are_resolved() {
        assert!(env_refs(&json!({"A":"env:PATH"})).unwrap()[0].1.len() > 0);
        assert_eq!(env_refs(&json!({"A":"secret://x"})).unwrap_err().code, "capability.unsupported");
    }

    #[test]
    fn pi_compaction_settings_scales_reserve_and_keep_recent() {
        assert_eq!(
            pi_compaction_settings(None),
            json!({"enabled": true, "reserveTokens": 19_200, "keepRecentTokens": 7_680})
        );
        assert_eq!(
            pi_compaction_settings(Some(128_000)),
            json!({"enabled": true, "reserveTokens": 19_200, "keepRecentTokens": 7_680})
        );
        assert_eq!(
            pi_compaction_settings(Some(200_000)),
            json!({"enabled": true, "reserveTokens": 30_000, "keepRecentTokens": 12_000})
        );
        assert_eq!(
            pi_compaction_settings(Some(1_024_000)),
            json!({"enabled": true, "reserveTokens": 65_536, "keepRecentTokens": 20_000})
        );
    }

    #[tokio::test]
    async fn pi_account_capabilities_override_imported_and_populate_profile() {
        let state = std::env::temp_dir().join(format!("muteki-pi-cap-{}", uuid::Uuid::new_v4()));
        let imported = state.join("imported-credentials/accounts/pi-custom");
        std::fs::create_dir_all(&imported).unwrap();
        std::fs::write(
            imported.join("MODEL_CAPABILITIES.json"),
            serde_json::to_vec(&json!({
                "deepseek-flash": {"context_window": 128_000, "max_output_tokens": 16_384},
                "fallback-only": {"context_window": 256_000, "max_output_tokens": 32_768}
            }))
            .unwrap(),
        )
        .unwrap();
        let store = Store::open(":memory:").unwrap();
        store
            .put(
                "credential_accounts",
                "pi-custom",
                &json!({
                    "account_id": "pi-custom",
                    "engine": "api",
                    "worker_engine": "pi",
                    "connection": "custom_endpoint",
                    "secret": "synthetic-key",
                    "base_url": "https://example.invalid/v1",
                    "default_model": "deepseek-flash",
                    "model_capabilities": {
                        "deepseek-flash": {
                            "context_window": 1_024_000,
                            "max_output_tokens": 65_536,
                            "reasoning": true,
                            "input": ["text", "image"]
                        }
                    }
                }),
            )
            .unwrap();
        let row = store.get("credential_accounts", "pi-custom").unwrap().unwrap();
        let fallback_cap = pi_model_capabilities(Some(&imported), &row, "fallback-only").unwrap();
        assert_eq!(fallback_cap["context_window"], 256_000);
        let result = resolve(
            &store,
            &state,
            find("pi").unwrap(),
            "account:pi-custom",
            Some("deepseek-flash"),
        )
        .await
        .unwrap();
        let profile = std::path::Path::new(
            result
                .env
                .iter()
                .rev()
                .find(|(k, _)| k == "PI_CODING_AGENT_DIR")
                .map(|(_, v)| v.as_str())
                .unwrap(),
        );
        let models: Value =
            serde_json::from_slice(&std::fs::read(profile.join("models.json")).unwrap()).unwrap();
        let settings: Value =
            serde_json::from_slice(&std::fs::read(profile.join("settings.json")).unwrap()).unwrap();
        assert_eq!(models["providers"]["muteki"]["models"][0]["contextWindow"], 1_024_000);
        assert_eq!(models["providers"]["muteki"]["models"][0]["maxTokens"], 65_536);
        assert_eq!(models["providers"]["muteki"]["models"][0]["reasoning"], true);
        assert_eq!(models["providers"]["muteki"]["models"][0]["input"], json!(["text", "image"]));
        assert_eq!(settings["compaction"]["reserveTokens"], 65_536);
        assert_eq!(settings["compaction"]["keepRecentTokens"], 20_000);
        std::fs::remove_dir_all(&state).unwrap();
    }
}
