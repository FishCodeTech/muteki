//! Subscription limit reads that never start a model turn.
//!
//! Window mappings follow the original provider_limits (adapted there from
//! T3 Code's codex/claude/grok/openCode usage-limit readers). A credential id
//! selects the same stored account or host login a session would use.
//! Successful snapshots are cached; a failed refresh keeps the previous
//! windows, marked stale, only while that credential's revision is unchanged.

use crate::{
    AppState,
    agent_runtime::{binaries, credentials, descriptors, host_login},
    native_chat,
};
use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::StatusCode,
    routing::{get, post},
};
use futures_util::{StreamExt, stream};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    path::{Path as FsPath, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

pub(crate) mod cursor_account;

pub const SUPPORTED: [&str; 5] = ["codex", "claude", "grok", "cursor", "opencode"];
const UNSUPPORTED: [&str; 6] = ["pi", "kimi", "omp", "devin", "droid", "agy"];
const SESSION: f64 = 5.0 * 3600.0;
const WEEK: f64 = 7.0 * 86400.0;
const MONTH: f64 = 30.0 * 86400.0;
const CACHE_SECONDS: f64 = 60.0;
const PROBE_TIMEOUT: Duration = Duration::from_secs(25);
const HTTP_TIMEOUT: Duration = Duration::from_secs(10);
const CURSOR_DEFAULT_ENDPOINT: &str = "https://api2.cursor.sh";
const CURSOR_LIMITS_URL: &str =
    "https://api2.cursor.sh/aiserver.v1.DashboardService/GetCurrentPeriodUsage";
const CURSOR_WINDOWS: [(&str, &str, &str); 3] = [
    ("totalPercentUsed", "总体", "两个额度池的合计用量，不是第三个额度。"),
    ("autoPercentUsed", "Cursor 模型", "Grok 与 Composer 优先使用这个池；Auto 可使用任一池。"),
    (
        "apiPercentUsed",
        "其他模型",
        "Claude、GPT、Gemini 使用这个池；Grok 与 Composer 用完自己的池后也会落到这里。",
    ),
];
const GROK_SELECTORS: [&str; 13] = [
    "XAI_API_KEY",
    "GROK_OIDC_ISSUER",
    "GROK_OIDC_CLIENT_ID",
    "GROK_OAUTH2_ISSUER",
    "GROK_OAUTH2_CLIENT_ID",
    "GROK_OAUTH2_PRINCIPAL_TYPE",
    "GROK_OAUTH2_PRINCIPAL_ID",
    "GROK_AUTH_PROVIDER_COMMAND",
    "GROK_LOCAL_AUTH",
    "GROK_CLI_CHAT_PROXY_BASE_URL",
    "GROK_MODELS_BASE_URL",
    "GROK_CONFIG",
    "GROK_CONFIG_PATH",
];
const PROBE_ENV: [&str; 20] = [
    "HOME",
    "CODEX_HOME",
    "OPENAI_API_KEY",
    "OPENAI_BASE_URL",
    "CLAUDE_CONFIG_DIR",
    "CLAUDE_CODE_OAUTH_TOKEN",
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_AUTH_TOKEN",
    "ANTHROPIC_BASE_URL",
    "GROK_HOME",
    "GROK_AUTH",
    "XDG_DATA_HOME",
    "XDG_CONFIG_HOME",
    "OPENCODE_SERVER_URL",
    "OPENCODE_API_KEY",
    "OPENCODE_AUTH_CONTENT",
    "CURSOR_API_ENDPOINT",
    "CURSOR_AUTH_TOKEN",
    "CURSOR_API_KEY",
    "AGENT_CLI_CREDENTIAL_STORE",
];

fn engine_name(engine: &str) -> &'static str {
    match engine {
        "codex" => "Codex",
        "claude" => "Claude Code",
        "grok" => "Grok",
        "cursor" => "Cursor",
        "opencode" => "OpenCode",
        "pi" => "Pi",
        "kimi" => "Kimi",
        "omp" => "OMP",
        "devin" => "Devin",
        "droid" => "Droid",
        "agy" => "Antigravity",
        _ => "Agent",
    }
}

fn kind_label(kind: &str) -> &'static str {
    match kind {
        "session" => "会话额度",
        "weekly" => "周额度",
        "monthly" => "月额度",
        _ => "订阅额度",
    }
}

/// A typed, client-safe failure; never carries token values or HTTP bodies.
#[derive(Debug, Clone)]
pub(crate) struct Fail {
    pub status: &'static str,
    pub code: &'static str,
    pub message: String,
    /// The account a failed request authenticated as, when known, so a stale
    /// snapshot is only retained for the same login.
    identity: Option<String>,
}

fn fail(status: &'static str, code: &'static str, message: impl Into<String>) -> Fail {
    Fail { status, code, message: message.into(), identity: None }
}

fn unsupported(code: &'static str, message: impl Into<String>) -> Fail {
    fail("unsupported", code, message)
}

fn unauthenticated(code: &'static str, message: impl Into<String>) -> Fail {
    fail("unauthenticated", code, message)
}

fn error(code: &'static str, message: impl Into<String>) -> Fail {
    fail("error", code, message)
}

fn number(value: &Value) -> Option<f64> {
    value.as_f64().filter(|n| n.is_finite())
}

fn reset_at(value: &Value) -> Option<f64> {
    if let Some(n) = number(value) {
        return (n > 0.0).then_some(n);
    }
    let parsed = chrono::DateTime::parse_from_rfc3339(value.as_str()?.trim()).ok()?;
    Some(parsed.timestamp() as f64 + f64::from(parsed.timestamp_subsec_micros()) / 1e6)
}

fn window(
    id: &str,
    kind: &str,
    used: f64,
    reset: &Value,
    seconds: Option<f64>,
    label: Option<String>,
) -> Value {
    json!({
        "id": id,
        "kind": kind,
        "label": label.unwrap_or_else(|| kind_label(kind).to_owned()),
        "used_percent": used.clamp(0.0, 100.0),
        "reset_at": reset_at(reset),
        "window_seconds": seconds,
    })
}

fn truthy(value: Option<&Value>) -> Option<&Value> {
    value.filter(|v| match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Object(map) => !map.is_empty(),
        Value::Array(items) => !items.is_empty(),
        Value::String(s) => !s.is_empty(),
        Value::Number(_) => true,
    })
}

pub(crate) fn codex_windows(response: &Value) -> Vec<Value> {
    let empty = json!({});
    let snapshot = truthy(response.get("rateLimitsByLimitId").and_then(|b| b.get("codex")))
        .or_else(|| truthy(response.get("rateLimits")))
        .unwrap_or(&empty);
    if !matches!(snapshot.get("limitId"), None | Some(Value::Null))
        && snapshot["limitId"] != "" && snapshot["limitId"] != "codex"
    {
        return Vec::new();
    }
    let monthly = matches!(snapshot["planType"].as_str(), Some("free" | "go"));
    let mut windows = Vec::new();
    for (id, fallback) in [("primary", if monthly { MONTH } else { SESSION }), ("secondary", WEEK)] {
        let row = &snapshot[id];
        let Some(used) = number(&row["usedPercent"]) else { continue };
        let seconds = number(&row["windowDurationMins"])
            .filter(|minutes| *minutes > 0.0)
            .map_or(fallback, |minutes| minutes * 60.0);
        let kind = if seconds >= MONTH { "monthly" } else if seconds >= WEEK { "weekly" } else { "session" };
        windows.push(window(id, kind, used, &row["resetsAt"], Some(seconds), None));
    }
    windows
}

pub(crate) fn claude_windows(response: &Value) -> Result<Vec<Value>, Fail> {
    let limits = &response["rate_limits"];
    if truthy(response.get("rate_limits_available")).is_none() || !limits.is_object() {
        return Err(unsupported("no_subscription_limits", "此 Claude 账户未提供订阅额度。"));
    }
    let mut windows = Vec::new();
    for (id, kind, seconds) in [("five_hour", "session", SESSION), ("seven_day", "weekly", WEEK)] {
        if let Some(used) = number(&limits[id]["utilization"]) {
            windows.push(window(id, kind, used, &limits[id]["resets_at"], Some(seconds), None));
        }
    }
    for row in limits["model_scoped"].as_array().into_iter().flatten() {
        let (Some(name), Some(used)) = (row["display_name"].as_str(), number(&row["utilization"])) else {
            continue;
        };
        let slug: String = name
            .to_lowercase()
            .split(|c: char| !c.is_ascii_lowercase() && !c.is_ascii_digit())
            .collect::<Vec<_>>()
            .join("_");
        let slug = collapse_underscores(&slug);
        windows.push(window(
            &format!("seven_day_{slug}"),
            "weekly",
            used,
            &row["resets_at"],
            Some(WEEK),
            Some(format!("周额度 · {name}")),
        ));
    }
    Ok(windows)
}

fn collapse_underscores(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        if c == '_' && out.ends_with('_') {
            continue;
        }
        out.push(c);
    }
    out
}

pub(crate) fn grok_windows(response: &Value) -> Vec<Value> {
    let config = &response["config"];
    let Some(used) = number(&config["creditUsagePercent"]) else { return Vec::new() };
    let period = &config["currentPeriod"];
    let kind = match period["type"].as_str().unwrap_or("").trim_start_matches("USAGE_PERIOD_TYPE_") {
        "WEEKLY" => "weekly",
        "MONTHLY" => "monthly",
        _ => "other",
    };
    vec![window("subscription", kind, used, &period["end"], None, None)]
}

pub(crate) fn opencode_windows(response: &Value) -> Result<Vec<Value>, Fail> {
    let usage = &response["usage"];
    let mut windows = Vec::new();
    for (id, kind, seconds) in [
        ("rolling", "session", Some(SESSION)),
        ("weekly", "weekly", Some(WEEK)),
        ("monthly", "monthly", None),
    ] {
        let row = &usage[id];
        let Some(used) = number(&row["percent"]).filter(|_| reset_at(&row["resetsAt"]).is_some()) else {
            return Err(error("invalid_response", "OpenCode Go 额度响应结构无效。"));
        };
        windows.push(window(
            &format!("go_{id}"),
            kind,
            used,
            &row["resetsAt"],
            seconds,
            Some(format!("Go · {}", kind_label(kind))),
        ));
    }
    Ok(windows)
}

fn sha(text: &str) -> String {
    format!("{:x}", Sha256::digest(text.as_bytes()))
}

fn fingerprint(engine: &str, identity: &str) -> String {
    sha(&format!("{engine}\0{identity}"))
}

/// Process environment with a credential overlay on top. An empty overlay
/// value shadows the host variable, as it does for the session child.
#[derive(Clone, Default)]
struct Env {
    overlay: Vec<(String, String)>,
}

impl Env {
    fn get(&self, key: &str) -> Option<String> {
        if let Some((_, value)) = self.overlay.iter().rev().find(|(name, _)| name == key) {
            let value = value.trim();
            return (!value.is_empty()).then(|| value.to_owned());
        }
        crate::shell_env::var(key)
            .map(|v| v.trim().to_owned())
            .filter(|v| !v.is_empty())
    }

    fn home(&self) -> PathBuf {
        self.get("HOME").map(PathBuf::from).unwrap_or_default()
    }

    fn root(&self, variable: &str, relative: &str) -> PathBuf {
        self.get(variable).map(PathBuf::from).unwrap_or_else(|| self.home().join(relative))
    }

    fn launch_for(&self, binary: &FsPath) -> agent::LaunchEnv {
        let mut pairs = self.overlay.clone();
        if !pairs.iter().any(|(key, _)| key == "PATH")
            && let Ok(path_env) = crate::agent_runtime::binaries::launch_path(binary)
        {
            pairs.push(("PATH".to_owned(), path_env));
        }
        agent::LaunchEnv { env: pairs, home: None }
    }
}

fn json_file(path: &FsPath) -> Result<Value, Fail> {
    let raw = match std::fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(json!({})),
        Err(_) => return Err(error("invalid_credentials", "登录文件无法读取。")),
    };
    match serde_json::from_str::<Value>(&raw) {
        Ok(value) if value.is_object() => Ok(value),
        _ => Err(error("invalid_credentials", "登录文件结构无效。")),
    }
}

fn http() -> reqwest::Client {
    crate::network_proxy::http_client_builder()
        .timeout(HTTP_TIMEOUT)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap_or_default()
}

fn transport(e: &reqwest::Error) -> Fail {
    if e.is_timeout() {
        error("timeout", "额度读取超时，请稍后刷新。")
    } else {
        error("network_error", format!("额度接口连接失败（{}）。", if e.is_connect() { "连接" } else { "请求" }))
    }
}

async fn get_json(url: &str, token: &str, go: bool) -> Result<Value, Fail> {
    let response = http().get(url).bearer_auth(token).send().await.map_err(|e| transport(&e))?;
    let status = response.status().as_u16();
    if go && status == 403 {
        return Err(unsupported("go_subscription_required", "此账户没有 OpenCode Go 订阅。"));
    }
    if matches!(status, 401 | 403) {
        return Err(unauthenticated("login_expired", "登录已失效，请重新登录后刷新。"));
    }
    if !response.status().is_success() {
        return Err(error("http_error", format!("额度接口返回 HTTP {status}。")));
    }
    match response.json::<Value>().await {
        Ok(value) if value.is_object() => Ok(value),
        Ok(_) | Err(_) => Err(error("invalid_response", "额度接口响应结构无效。")),
    }
}

fn agent_failure(e: &agent::AgentError) -> Fail {
    match e {
        agent::AgentError::Spawn(_) => error("cli_missing", "所需 CLI 未安装或路径不可用。"),
        agent::AgentError::Provider(_) => error("rpc_error", "额度读取失败（Provider 返回错误）。"),
        agent::AgentError::Protocol(_) => error("probe_failed", "额度读取失败（协议错误）。"),
        agent::AgentError::Io(_) => error("probe_failed", "额度读取失败（IO 错误）。"),
    }
}

async fn binary(engine: &str) -> Result<PathBuf, Fail> {
    let descriptor = descriptors::find(engine)
        .ok_or_else(|| error("cli_missing", "所需 CLI 未安装或路径不可用。"))?;
    binaries::resolve(descriptor, None)
        .await
        .map(|resolved| resolved.path)
        .map_err(|_| error("cli_missing", "所需 CLI 未安装或路径不可用。"))
}

async fn bounded<T>(future: impl std::future::Future<Output = Result<T, agent::AgentError>>) -> Result<T, Fail> {
    match tokio::time::timeout(PROBE_TIMEOUT, future).await {
        Ok(Ok(value)) => Ok(value),
        Ok(Err(e)) => Err(agent_failure(&e)),
        Err(_) => Err(error("timeout", "额度读取超时，请稍后刷新。")),
    }
}

type Probe = (Vec<Value>, Option<String>);

async fn codex(env: &Env) -> Result<Probe, Fail> {
    if env.get("OPENAI_API_KEY").is_some() || env.get("OPENAI_BASE_URL").is_some() {
        return Err(unsupported("api_key", "当前 Codex 环境使用 API key 或自定义端点。"));
    }
    let auth = json_file(&env.root("CODEX_HOME", ".codex").join("auth.json"))?;
    if auth.as_object().is_none_or(|map| map.is_empty()) {
        return Err(unauthenticated("login_missing", "请先登录此 Codex 账户。"));
    }
    if !auth["tokens"].is_object() || auth["auth_mode"].as_str().is_some_and(|m| m.eq_ignore_ascii_case("apikey")) {
        return Err(unsupported("subscription_required", "此 Codex 登录不是 ChatGPT 订阅账户。"));
    }
    let path = binary("codex").await?;
    let launch = env.launch_for(&path);
    let response = bounded(agent::codex::read_rate_limits(Some(path), launch)).await?;
    let key = auth["tokens"]["account_id"].as_str().unwrap_or("").trim().to_owned();
    Ok((codex_windows(&response), (!key.is_empty()).then(|| fingerprint("codex", &key))))
}

fn private_dir(path: &FsPath) -> Result<(), Fail> {
    std::fs::create_dir_all(path).map_err(|_| error("probe_failed", "无法创建额度探测目录。"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700));
    }
    Ok(())
}

async fn claude(env: &mut Env, stored: Option<&str>, state_dir: &FsPath) -> Result<Probe, Fail> {
    if ["ANTHROPIC_API_KEY", "ANTHROPIC_AUTH_TOKEN", "ANTHROPIC_BASE_URL"].iter().any(|k| env.get(k).is_some()) {
        return Err(unsupported("api_key", "当前 Claude 环境使用 API key 或自定义端点。"));
    }
    let mut key = None;
    if let Some(id) = stored {
        let token = env
            .get("CLAUDE_CODE_OAUTH_TOKEN")
            .ok_or_else(|| unauthenticated("credential_missing", "账户 OAuth token 缺失。"))?;
        let config = state_dir.join("_secrets").join("usage_probes").join(id);
        private_dir(&config)?;
        env.overlay.push(("CLAUDE_CONFIG_DIR".into(), config.to_string_lossy().into_owned()));
        key = Some(token);
    } else if let Ok(raw) = std::fs::read_to_string(env.home().join(".claude.json"))
        && let Ok(config) = serde_json::from_str::<Value>(&raw)
        && let Some(email) = config["oauthAccount"]["emailAddress"].as_str()
        && !email.trim().is_empty()
    {
        key = Some(email.trim().to_lowercase());
    }
    let path = binary("claude").await?;
    let launch = env.launch_for(&path);
    let response = bounded(agent::claude::read_usage(Some(path), launch)).await?;
    Ok((claude_windows(&response)?, key.map(|k| fingerprint("claude", &k))))
}

fn grok_custom_config(path: &FsPath) -> bool {
    static PATTERN: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let pattern = PATTERN.get_or_init(|| {
        regex::Regex::new(r#"(?m)^\s*(?:\[\[?\s*)?["']?(?:auth|grok_com_config|endpoints)["']?\s*[.\]=]"#)
            .expect("static pattern")
    });
    std::fs::read_to_string(path).is_ok_and(|text| pattern.is_match(&text))
}

async fn grok(env: &Env, stored: bool) -> Result<Probe, Fail> {
    if GROK_SELECTORS.iter().any(|key| env.get(key).is_some()) {
        return Err(unsupported("custom_auth", "此 Grok 登录配置暂不支持额度读取。"));
    }
    let root = env.root("GROK_HOME", ".grok");
    for path in [
        root.join("config.toml"),
        root.join("managed_config.toml"),
        root.join("requirements.toml"),
        PathBuf::from("/etc/grok/managed_config.toml"),
        PathBuf::from("/etc/grok/requirements.toml"),
    ] {
        if grok_custom_config(&path) {
            return Err(unsupported("custom_auth", "此 Grok 配置选择了其他账户或端点。"));
        }
    }
    let raw = match env.get("GROK_AUTH").filter(|_| !stored) {
        Some(inline) => serde_json::from_str::<Value>(&inline)
            .ok()
            .filter(Value::is_object)
            .ok_or_else(|| error("invalid_credentials", "登录文件结构无效。"))?,
        None => json_file(&root.join("auth.json"))?,
    };
    let empty = json!({});
    let credential = truthy(raw.get("https://auth.x.ai::b1a00492-073a-47ea-816f-4c329264a828"))
        .or_else(|| truthy(raw.get("https://accounts.x.ai/sign-in")))
        .unwrap_or(&empty);
    if credential["auth_mode"] == "api_key" {
        return Err(unsupported("api_key", "Grok API key 账户不提供订阅额度。"));
    }
    let token = credential["key"].as_str().unwrap_or("").trim().to_owned();
    if token.is_empty() {
        return Err(unauthenticated("login_missing", "请先登录此 Grok 账户。"));
    }
    let response = get_json("https://cli-chat-proxy.grok.com/v1/billing?format=credits", &token, false).await?;
    let email = credential["email"].as_str().unwrap_or("").trim().to_lowercase();
    let identity = if email.is_empty() { token } else { email };
    Ok((grok_windows(&response), Some(fingerprint("grok", &identity))))
}

async fn opencode(env: &Env, stored: Option<&Value>) -> Result<Probe, Fail> {
    if env.get("OPENCODE_SERVER_URL").is_some() || env.get("OPENAI_BASE_URL").is_some() {
        return Err(unsupported("external_server", "外部 OpenCode 服务或自定义端点的额度暂未支持。"));
    }
    let token = match stored {
        Some(row) => {
            if !matches!(row["provider"].as_str(), Some("opencode-go" | "opencode" | "opencode-zen")) {
                return Err(unsupported("go_credential_required", "此凭据未标记为 OpenCode Go / Zen，暂不查询订阅额度。"));
            }
            env.get("OPENCODE_API_KEY").unwrap_or_default()
        }
        None => {
            let raw = match env.get("OPENCODE_AUTH_CONTENT") {
                Some(inline) => serde_json::from_str::<Value>(&inline).unwrap_or_else(|_| json!({})),
                None => json_file(&env.root("XDG_DATA_HOME", ".local/share").join("opencode/auth.json"))?,
            };
            let credential = &raw["opencode-go"];
            if credential["type"] == "api" {
                credential["key"].as_str().unwrap_or("").trim().to_owned()
            } else {
                env.get("OPENCODE_API_KEY").unwrap_or_default()
            }
        }
    };
    if token.is_empty() {
        return Err(unsupported("go_subscription_required", "暂未检测到 OpenCode Go 订阅登录。"));
    }
    let response = get_json("https://opencode.ai/zen/go/v1/usage", &token, true).await?;
    Ok((opencode_windows(&response)?, Some(fingerprint("opencode-go", &token))))
}

pub(crate) fn cursor_settings_enabled(state_dir: &FsPath) -> bool {
    std::fs::read_to_string(state_dir.join("_usage_settings.json"))
        .ok()
        .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
        .is_some_and(|settings| settings["cursor_account_usage_enabled"] == true)
}

fn cursor_auth_file(env: &Env) -> PathBuf {
    if cfg!(target_os = "macos") {
        return env.home().join(".cursor/auth.json");
    }
    let config = env.get("XDG_CONFIG_HOME").map(PathBuf::from).filter(|p| p.is_absolute());
    config.unwrap_or_else(|| env.home().join(".config")).join("cursor/auth.json")
}

fn jwt_subject(token: &str) -> Result<String, Fail> {
    use base64::Engine;
    let invalid = || unauthenticated("token_invalid", "Cursor 登录凭据格式无法识别。重新执行 cursor-agent login 后刷新。");
    let payload = token.split('.').nth(1).ok_or_else(invalid)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload.trim_end_matches('='))
        .map_err(|_| invalid())?;
    let claims: Value = serde_json::from_slice(&bytes).map_err(|_| invalid())?;
    let subject = claims["sub"].as_str().unwrap_or("");
    if subject.rsplit('|').next().unwrap_or("").is_empty() {
        return Err(invalid());
    }
    Ok(subject.to_owned())
}

async fn cursor_keychain_token() -> Result<Option<String>, Fail> {
    let output = tokio::time::timeout(
        Duration::from_secs(30),
        crate::host_process::command("/usr/bin/security")
            .args(["find-generic-password", "-s", "cursor-access-token", "-w"])
            .stdin(std::process::Stdio::null())
            .kill_on_drop(true)
            .output(),
    )
    .await
    .map_err(|_| error("keychain_timeout", "Cursor 钥匙串读取超时，其他功能可继续使用。"))?
    .map_err(|_| error("keychain_failed", "无法启动 macOS 钥匙串读取程序。"))?;
    match output.status.code() {
        Some(44) => return Ok(None),
        Some(36) => return Err(unauthenticated("keychain_interaction_required", "Cursor 登录项需要 macOS 钥匙串授权，当前读取不可用。")),
        Some(128 | 51) => return Err(unauthenticated("keychain_denied", "macOS 钥匙串未授权非交互访问 Cursor 登录项；账号用量暂不可读，其他功能可继续使用。")),
        Some(0) => {}
        status => return Err(error("keychain_failed", format!("读取 macOS 钥匙串失败（exit {}）。", status.unwrap_or(-1)))),
    }
    let token = String::from_utf8(output.stdout)
        .map_err(|_| unauthenticated("token_invalid", "钥匙串中的 Cursor 登录凭据不是有效文本。重新执行 cursor-agent login 后刷新。"))?;
    Ok((!token.trim().is_empty()).then(|| token.trim().to_owned()))
}

async fn cursor_token(env: &Env, state_dir: &FsPath) -> Result<String, Fail> {
    if env.get("CURSOR_AUTH_TOKEN").is_some() {
        return Err(unauthenticated(
            "login_unavailable",
            "服务进程设置了 CURSOR_AUTH_TOKEN；账号历史需要此服务上的持久化 Cursor CLI 登录。",
        ));
    }
    if env.get("CURSOR_API_KEY").is_some() {
        return Err(unsupported(
            "api_key_only",
            "服务进程设置了 CURSOR_API_KEY，它可能属于另一个账号。读取账号用量需要此服务上的持久化 Cursor CLI 登录。",
        ));
    }
    let store = env.get("AGENT_CLI_CREDENTIAL_STORE").unwrap_or_default();
    if store == "memory" {
        return Err(unauthenticated(
            "login_missing",
            "AGENT_CLI_CREDENTIAL_STORE=memory 时 Cursor CLI 不保存登录；账号历史需要持久化 CLI 登录。",
        ));
    }
    if cfg!(target_os = "macos") && store != "file" {
        if !cursor_settings_enabled(state_dir) {
            return Err(unauthenticated(
                "keychain_disabled",
                "Cursor 账号用量未开启。开启后 Muteki 会使用已有的钥匙串读取授权获取 Cursor CLI 登录。",
            ));
        }
        return cursor_keychain_token().await?.ok_or_else(|| {
            unauthenticated(
                "login_missing",
                "钥匙串中没有 Cursor CLI 登录。在运行 Muteki 服务的电脑上执行 cursor agent login 完成浏览器登录后刷新；引擎的 API key 不等于账号用量登录。",
            )
        });
    }
    let path = cursor_auth_file(env);
    let raw = match std::fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(unauthenticated(
                "login_missing",
                format!("未找到 Cursor CLI 登录（{}）。执行 cursor-agent login 后刷新。", path.display()),
            ));
        }
        Err(_) => return Err(error("auth_file_unreadable", format!("无法读取 {}。", path.display()))),
    };
    let value: Value = serde_json::from_str(&raw)
        .map_err(|_| error("auth_file_unreadable", format!("无法读取 {}：格式无效。", path.display())))?;
    let token = value["accessToken"].as_str().unwrap_or("").trim().to_owned();
    if token.is_empty() {
        return Err(unauthenticated(
            "login_missing",
            format!("{} 中没有 Cursor 登录。执行 cursor-agent login 后刷新。", path.display()),
        ));
    }
    Ok(token)
}

async fn cursor(env: &Env, stored: bool, state_dir: &FsPath) -> Result<Probe, Fail> {
    if stored {
        return Err(unsupported("host_login_required", "Cursor 额度需使用本机持久化 CLI 登录。"));
    }
    let endpoint = env.get("CURSOR_API_ENDPOINT").unwrap_or_else(|| CURSOR_DEFAULT_ENDPOINT.into());
    if endpoint.trim_end_matches('/') != CURSOR_DEFAULT_ENDPOINT {
        return Err(unsupported("custom_endpoint", "Cursor 自定义端点的额度暂未支持。"));
    }
    let token = cursor_token(env, state_dir).await?;
    let account = sha(&jwt_subject(&token)?);
    let with_identity = |mut f: Fail| {
        f.identity = Some(account.clone());
        f
    };
    let response = http()
        .post(CURSOR_LIMITS_URL)
        .bearer_auth(&token)
        .header("Content-Type", "application/json")
        .header("connect-protocol-version", "1")
        .header("x-cursor-client-type", "cli")
        .body("{}")
        .send()
        .await
        .map_err(|e| {
            with_identity(if e.is_timeout() {
                error("request_timeout", "Cursor 账号接口请求超时。")
            } else {
                error("request_failed", "无法连接 Cursor 账号接口。")
            })
        })?;
    let status = response.status();
    if matches!(status.as_u16(), 401 | 403) {
        return Err(with_identity(unauthenticated("login_expired", "Cursor 登录已失效，请重新登录。")));
    }
    if status.is_redirection() {
        return Err(with_identity(error(
            "request_redirected",
            format!("Cursor 账号接口返回 HTTP {} 重定向，未转发登录凭据。", status.as_u16()),
        )));
    }
    if status.as_u16() != 200 {
        return Err(with_identity(error("request_failed", format!("Cursor 账号接口返回 HTTP {}。", status.as_u16()))));
    }
    let body: Value = response
        .json()
        .await
        .map_err(|_| with_identity(error("invalid_response", "Cursor 账号接口返回的不是 JSON。")))?;
    let windows = cursor_windows(&body).map_err(with_identity)?;
    Ok((windows, Some(account)))
}

fn loose_number(value: &Value) -> Option<f64> {
    number(value).or_else(|| value.as_str().and_then(|s| s.trim().parse::<f64>().ok()).filter(|n| n.is_finite()))
}

pub(crate) fn cursor_windows(body: &Value) -> Result<Vec<Value>, Fail> {
    if !body.is_object() {
        return Err(error("invalid_response", "Cursor 额度接口返回的结构无法识别。"));
    }
    let plan = &body["planUsage"];
    let resets_at = loose_number(&body["billingCycleEnd"]).filter(|end| *end > 0.0).map(|end| end / 1000.0);
    let windows: Vec<Value> = CURSOR_WINDOWS
        .iter()
        .filter_map(|(id, label, description)| {
            let used = loose_number(&plan[*id])?;
            Some(json!({
                "id": id, "kind": "monthly", "label": label, "description": description,
                "aggregate": *id == "totalPercentUsed", "used_percent": used.clamp(0.0, 100.0),
                "reset_at": resets_at, "window_seconds": null,
            }))
        })
        .collect();
    if windows.is_empty() {
        return Err(unsupported("limits_unsupported", "这个 Cursor 账号没有返回月度额度。"));
    }
    Ok(windows)
}

#[derive(Clone)]
struct Account {
    id: String,
    engine: String,
    label: String,
    stored: Option<Value>,
    revision: String,
    revision_error: Option<String>,
}

/// Whether a stored account has login material, and whether that material is
/// only an API key (which carries no subscription windows).
pub(crate) fn stored_material(state_dir: &FsPath, id: &str, row: &Value) -> (bool, bool) {
    let base = state_dir.join("imported-credentials/accounts").join(id);
    let text = |key: &str| row[key].as_str().is_some_and(|s| !s.trim().is_empty());
    let login = ["codex-home", "grok-home", "kimi-home"].iter().any(|d| base.join(d).is_dir())
        || base.join("CLAUDE_CODE_OAUTH_TOKEN").is_file()
        || text("codex_auth_json")
        || text("imported_home");
    let key = text("secret") || base.join("API_KEY").is_file() || base.join("CURSOR_API_KEY").is_file();
    (login || key, key && !login)
}

fn stat_entry(path: &FsPath) -> std::io::Result<Value> {
    match std::fs::metadata(path) {
        Ok(meta) => {
            let modified = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_nanos() as u64);
            Ok(json!([path.to_string_lossy(), modified, meta.len()]))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(json!([path.to_string_lossy(), null, null])),
        Err(e) => Err(e),
    }
}

/// Invalidates stale host quota when the host switches credential material.
fn system_revision(engine: &str, state_dir: &FsPath) -> std::io::Result<String> {
    if !host_login::discovery_enabled() {
        return Ok("host_discovery_disabled".into());
    }
    let env = Env::default();
    let root = match engine {
        "codex" => Some(env.root("CODEX_HOME", ".codex")),
        "claude" => Some(env.root("CLAUDE_CONFIG_DIR", ".claude")),
        "grok" => Some(env.root("GROK_HOME", ".grok")),
        "opencode" => Some(env.root("XDG_DATA_HOME", ".local/share").join("opencode")),
        _ => None,
    };
    let files: Vec<PathBuf> = match (engine, root) {
        ("cursor", _) => vec![cursor_auth_file(&env), state_dir.join("_usage_settings.json")],
        (_, Some(root)) => ["auth.json", ".credentials.json", "settings.json", "config.toml"]
            .iter()
            .map(|name| root.join(name))
            .collect(),
        _ => Vec::new(),
    };
    let stats = files.iter().map(|path| stat_entry(path)).collect::<std::io::Result<Vec<_>>>()?;
    let vars: Vec<Value> = PROBE_ENV.iter().map(|k| json!([k, std::env::var(k).ok()])).collect();
    Ok(fingerprint(engine, &json!([vars, stats]).to_string()))
}

fn accounts(s: &AppState) -> Vec<Account> {
    let state_dir = native_chat::state_dir(s);
    let mut rows = Vec::new();
    for row in s.store.list("credential_accounts").unwrap_or_default() {
        let engine = ["worker_engine", "engine"]
            .iter()
            .filter_map(|key| row[*key].as_str())
            .chain(row["details"]["target_engine"].as_str())
            .map(|e| e.trim().to_ascii_lowercase())
            .find(|e| SUPPORTED.contains(&e.as_str()) || UNSUPPORTED.contains(&e.as_str()));
        let Some(engine) = engine else { continue };
        let Some(id) = row["account_id"].as_str().or_else(|| row["id"].as_str()).map(str::to_owned) else {
            continue;
        };
        let base = state_dir.join("imported-credentials/accounts").join(&id);
        let material = stat_entry(&base).unwrap_or(Value::Null);
        rows.push(Account {
            id: format!("account:{id}"),
            engine,
            label: id,
            revision: sha(&json!([row, material]).to_string()),
            stored: Some(row),
            revision_error: None,
        });
    }
    for engine in SUPPORTED.iter().chain(UNSUPPORTED.iter()) {
        let (revision, revision_error) = match system_revision(engine, &state_dir) {
            Ok(revision) => (revision, None),
            Err(e) => (format!("unreadable:{:?}", e.kind()), Some(format!("{:?}", e.kind()))),
        };
        rows.push(Account {
            id: format!("system:{engine}"),
            engine: (*engine).into(),
            label: format!("{} · 本机登录", engine_name(engine)),
            stored: None,
            revision,
            revision_error,
        });
    }
    rows
}

fn now() -> f64 {
    chrono::Utc::now().timestamp_millis() as f64 / 1000.0
}

async fn probe(s: &AppState, account: &Account) -> Result<Probe, Fail> {
    let engine = account.engine.as_str();
    if UNSUPPORTED.contains(&engine) {
        return Err(unsupported("provider_not_supported", "暂未支持额度查询；Cost 和 Tokens 仍统计此引擎。"));
    }
    if account.stored.is_none() && !host_login::discovery_enabled() {
        return Err(unauthenticated("host_discovery_disabled", "此服务未启用本机登录发现。"));
    }
    if let Some(kind) = &account.revision_error {
        return Err(error("credential_metadata_unreadable", format!("无法读取此账户的登录文件元数据（{kind}）。")));
    }
    let state_dir = native_chat::state_dir(s);
    let mut env = Env::default();
    let stored_id = account.id.strip_prefix("account:");
    if let (Some(row), Some(id)) = (&account.stored, stored_id) {
        if matches!(row["connection"].as_str(), Some("custom_endpoint" | "custom")) {
            return Err(unsupported("custom_endpoint", "自定义端点暂不提供订阅额度。"));
        }
        let (present, api_key) = stored_material(&state_dir, id, row);
        if api_key && engine != "opencode" {
            return Err(unsupported("api_key", "API key 账户不提供订阅额度。"));
        }
        if !present {
            return Err(unauthenticated("credential_missing", "账户登录凭据缺失。"));
        }
        let descriptor = descriptors::find(engine)
            .ok_or_else(|| unsupported("provider_not_supported", "暂未支持额度查询。"))?;
        let resolved = credentials::resolve(&s.store, &state_dir, descriptor, &account.id, None)
            .await
            .map_err(|e| unauthenticated("credential_missing", e.message.clone()))?;
        env.overlay = resolved.env;
    }
    match engine {
        "codex" => codex(&env).await,
        "claude" => claude(&mut env, stored_id, &state_dir).await,
        "grok" => grok(&env, account.stored.is_some()).await,
        "opencode" => opencode(&env, account.stored.as_ref()).await,
        "cursor" => cursor(&env, account.stored.is_some(), &state_dir).await,
        _ => Err(unsupported("provider_not_supported", "暂未支持额度查询。")),
    }
}

#[derive(Clone)]
struct Snapshot {
    at: Instant,
    row: Value,
    fingerprint: Option<String>,
    revision: String,
}

#[derive(Default)]
struct Inner {
    cached: Option<Value>,
    cached_at: Option<Instant>,
    completed_at: Option<Instant>,
    signature: Vec<(String, String)>,
    results: HashMap<String, Snapshot>,
    last_ok: HashMap<String, Snapshot>,
}

#[derive(Default)]
pub struct Limits {
    inner: tokio::sync::Mutex<Inner>,
}

#[derive(Debug)]
pub struct UnknownCredential;

fn fresh(at: Instant, requested: Instant, refresh: bool) -> bool {
    at >= requested || (!refresh && at.elapsed().as_secs_f64() < CACHE_SECONDS)
}

impl Limits {
    async fn read_one(&self, s: &AppState, account: &Account, inner_last: Option<Snapshot>) -> Snapshot {
        let mut row = json!({
            "id": account.id, "engine": account.engine, "label": account.label,
            "status": "ok", "windows": [], "updated_at": null, "error": null,
            "error_code": null, "stale": false, "checked_at": now(),
        });
        let mut fp = None;
        match probe(s, account).await {
            Ok((windows, fingerprint)) => {
                row["windows"] = json!(windows);
                row["updated_at"] = json!(now());
                fp = fingerprint;
            }
            Err(failure) => {
                row["status"] = json!(failure.status);
                row["error_code"] = json!(failure.code);
                row["error"] = json!(failure.message);
                if let Some(previous) = inner_last
                    && failure.status == "error"
                    && previous.revision == account.revision
                    && failure.identity.as_ref().is_none_or(|id| Some(id) == previous.fingerprint.as_ref())
                {
                    row["windows"] = previous.row["windows"].clone();
                    row["updated_at"] = previous.row["updated_at"].clone();
                    row["stale"] = json!(true);
                    fp = previous.fingerprint.clone();
                }
            }
        }
        Snapshot { at: Instant::now(), row, fingerprint: fp, revision: account.revision.clone() }
    }

    /// Read every credential's windows, or one credential's with `credential_id`.
    pub async fn read(&self, s: &AppState, refresh: bool, credential_id: Option<&str>) -> Result<Value, UnknownCredential> {
        let requested = Instant::now();
        let mut inner = self.inner.lock().await;
        let accounts = accounts(s);
        let signature: Vec<(String, String)> = accounts.iter().map(|a| (a.id.clone(), a.revision.clone())).collect();
        if signature != inner.signature {
            inner.cached = None;
            inner.signature = signature;
        }
        if let Some(id) = credential_id
            && !accounts.iter().any(|a| a.id == id)
        {
            return Err(UnknownCredential);
        }
        if let (Some(cached), Some(cached_at)) = (&inner.cached, inner.cached_at)
            && (inner.completed_at.is_some_and(|done| done >= requested)
                || (!refresh && cached_at.elapsed().as_secs_f64() < CACHE_SECONDS))
        {
            let mut result = cached.clone();
            if let Some(id) = credential_id {
                let rows: Vec<Value> = result["accounts"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter(|row| row["credential_ids"].as_array().is_some_and(|ids| ids.iter().any(|v| v == id)))
                    .cloned()
                    .collect();
                result["accounts"] = json!(rows);
            }
            return Ok(result);
        }
        let selected: Vec<Account> = accounts
            .into_iter()
            .filter(|a| credential_id.is_none_or(|id| a.id == id))
            .collect();
        let mut pending = Vec::new();
        let mut snapshots: Vec<Option<Snapshot>> = vec![None; selected.len()];
        for (index, account) in selected.iter().enumerate() {
            match inner.results.get(&account.id) {
                Some(cached) if cached.revision == account.revision && fresh(cached.at, requested, refresh) => {
                    snapshots[index] = Some(cached.clone());
                }
                _ => pending.push((index, inner.last_ok.get(&account.id).cloned())),
            }
        }
        let probed: Vec<(usize, Snapshot)> = stream::iter(pending)
            .map(|(index, last)| {
                let account = &selected[index];
                async move { (index, self.read_one(s, account, last).await) }
            })
            .buffer_unordered(5)
            .collect()
            .await;
        for (index, snapshot) in probed {
            let id = selected[index].id.clone();
            if snapshot.row["status"] == "ok" {
                inner.last_ok.insert(id.clone(), snapshot.clone());
            }
            inner.results.insert(id, snapshot.clone());
            snapshots[index] = Some(snapshot);
        }
        let result = json!({
            "as_of": now(),
            "accounts": merge(snapshots.into_iter().flatten().collect()),
            "supported_engines": SUPPORTED,
            "cache_seconds": CACHE_SECONDS,
        });
        if credential_id.is_none() {
            inner.cached_at = selected.iter().filter_map(|a| inner.results.get(&a.id).map(|r| r.at)).min();
            inner.cached = Some(result.clone());
            inner.completed_at = Some(Instant::now());
        } else {
            inner.cached = None;
        }
        Ok(result)
    }
}

/// Pool credentials that authenticate as the same provider account.
fn merge(snapshots: Vec<Snapshot>) -> Vec<Value> {
    let mut order: Vec<String> = Vec::new();
    let mut groups: HashMap<String, Value> = HashMap::new();
    for snapshot in snapshots {
        let mut row = snapshot.row;
        let id = row["id"].as_str().unwrap_or("").to_owned();
        let group = match &snapshot.fingerprint {
            Some(fp) => format!("{}:{fp}", row["engine"].as_str().unwrap_or("")),
            None => id.clone(),
        };
        match groups.get_mut(&group) {
            None => {
                row["credential_ids"] = json!([id]);
                order.push(group.clone());
                groups.insert(group, row);
            }
            Some(previous) => {
                let mut ids = previous["credential_ids"].as_array().cloned().unwrap_or_default();
                ids.push(json!(id));
                let newer = row["updated_at"].as_f64().unwrap_or(0.0) > previous["updated_at"].as_f64().unwrap_or(0.0);
                if row["status"] == "ok" && (previous["status"] != "ok" || newer) {
                    row["id"] = previous["id"].clone();
                    row["label"] = previous["label"].clone();
                    *previous = row;
                }
                previous["credential_ids"] = json!(ids);
            }
        }
    }
    order.into_iter().filter_map(|group| groups.remove(&group)).collect()
}

fn legacy_entry(account: &Value, credential_id: &str) -> Value {
    let windows = account["windows"].as_array().cloned().unwrap_or_default();
    let tightest = windows
        .iter()
        .max_by(|a, b| {
            let a = a["used_percent"].as_f64().unwrap_or(0.0);
            let b = b["used_percent"].as_f64().unwrap_or(0.0);
            a.partial_cmp(&b).unwrap_or(std::cmp::Ordering::Equal)
        });
    let status = account["status"].as_str().unwrap_or("");
    json!({
        "credential_id": credential_id,
        "account_id": credential_id.strip_prefix("account:").unwrap_or(""),
        "engine": account["engine"], "label": account["label"],
        "quota_type": if account["error_code"] == "api_key" { "api_key" } else { "subscription" },
        "status": if status == "ok" && tightest.is_some() { "ok" } else if status == "unsupported" { "not_supported" } else { "unknown" },
        "remaining": tightest.and_then(|w| w["used_percent"].as_f64()).map(|used| 100.0 - used),
        "total": tightest.map(|_| 100),
        "window_seconds": tightest.map_or(Value::Null, |w| w["window_seconds"].clone()),
        "reset_at": tightest.map_or(Value::Null, |w| w["reset_at"].clone()),
        "updated_at": account["updated_at"], "unknown_reason": account["error"],
        "windows": windows, "stale": account["stale"].as_bool().unwrap_or(false),
    })
}

fn credential_ids(account: &Value) -> Vec<String> {
    let ids: Vec<String> = account["credential_ids"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|v| v.as_str().map(str::to_owned))
        .collect();
    if ids.is_empty() { vec![account["id"].as_str().unwrap_or("").to_owned()] } else { ids }
}

fn canonical_id(value: &str) -> Option<String> {
    let text = value.trim();
    let text = text.strip_prefix("secret://credential-accounts/").unwrap_or(text);
    if let Some(engine) = text.strip_prefix("system:") {
        let engine = engine.trim().to_ascii_lowercase();
        return (SUPPORTED.contains(&engine.as_str()) || UNSUPPORTED.contains(&engine.as_str()))
            .then(|| format!("system:{engine}"));
    }
    let id = text.strip_prefix("account:").unwrap_or(text).trim();
    let valid = id.chars().next().is_some_and(|c| c.is_ascii_alphanumeric())
        && id.len() <= 64
        && id.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'));
    valid.then(|| format!("account:{id}"))
}

type ApiResult = Result<Json<Value>, (StatusCode, Json<Value>)>;

fn not_found() -> (StatusCode, Json<Value>) {
    (StatusCode::NOT_FOUND, Json(json!({"detail":"额度账户不存在"})))
}

pub(crate) async fn limits(State(s): State<Arc<AppState>>, Query(query): Query<HashMap<String, String>>) -> ApiResult {
    let refresh = query.get("refresh").is_some_and(|v| v == "true" || v == "1");
    s.limits.read(&s, refresh, None).await.map(Json).map_err(|_| not_found())
}

pub(crate) async fn quota(State(s): State<Arc<AppState>>) -> ApiResult {
    let result = s.limits.read(&s, false, None).await.map_err(|_| not_found())?;
    let rows: Vec<Value> = result["accounts"]
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|account| credential_ids(account).into_iter().map(move |id| legacy_entry(account, &id)))
        .collect();
    Ok(Json(json!({"as_of": result["as_of"], "quota": rows})))
}

pub(crate) async fn refresh_quota(State(s): State<Arc<AppState>>, Path(id): Path<String>) -> ApiResult {
    let id = canonical_id(&id)
        .ok_or_else(|| (StatusCode::BAD_REQUEST, Json(json!({"detail":"凭据标识无效"}))))?;
    let result = s.limits.read(&s, true, Some(&id)).await.map_err(|_| not_found())?;
    let account = result["accounts"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|account| credential_ids(account).iter().any(|c| *c == id))
        .ok_or_else(not_found)?;
    Ok(Json(json!({"ok": account["status"] == "ok", "credential_id": id, "entry": legacy_entry(account, &id)})))
}

fn uses_keychain() -> bool {
    let env = Env::default();
    cfg!(target_os = "macos")
        && !matches!(env.get("AGENT_CLI_CREDENTIAL_STORE").as_deref(), Some("file" | "memory"))
        && env.get("CURSOR_AUTH_TOKEN").is_none()
        && env.get("CURSOR_API_KEY").is_none()
}

/// Whether the Cursor CLI login item exists, from attributes only so the
/// check never prompts for Keychain access.
async fn keychain_login_present() -> Result<bool, Value> {
    let output = tokio::time::timeout(
        Duration::from_secs(10),
        crate::host_process::command("/usr/bin/security")
            .args(["find-generic-password", "-s", "cursor-access-token"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .status(),
    )
    .await;
    let failed = |message: String| json!({"code": "keychain_failed", "message": message});
    match output {
        Ok(Ok(status)) if status.success() => Ok(true),
        Ok(Ok(status)) if status.code() == Some(44) => Ok(false),
        Ok(Ok(status)) => Err(failed(format!("无法检查 macOS 钥匙串（exit {}）。", status.code().unwrap_or(-1)))),
        Ok(Err(e)) => Err(failed(format!("无法启动 macOS 钥匙串读取程序（{:?}）。", e.kind()))),
        Err(_) => Err(json!({"code": "keychain_timeout", "message": "Cursor 钥匙串读取超时，其他功能可继续使用。"})),
    }
}

async fn settings_view(state_dir: &FsPath) -> Value {
    let keychain = uses_keychain();
    let (present, error) = if keychain {
        match keychain_login_present().await {
            Ok(present) => (json!(present), Value::Null),
            Err(error) => (Value::Null, error),
        }
    } else {
        (Value::Null, Value::Null)
    };
    json!({
        "cursor_account_usage_enabled": cursor_settings_enabled(state_dir),
        "host_account_discovery_supported": host_login::discovery_enabled(),
        "keychain": {"applies": keychain, "login_present": present, "error": error},
        "auth_file": if keychain { Value::Null } else { json!(cursor_auth_file(&Env::default()).to_string_lossy()) },
    })
}

pub(crate) fn write_usage_settings(state_dir: &FsPath, enabled: bool) -> std::io::Result<()> {
    let path = state_dir.join("_usage_settings.json");
    let mut settings = std::fs::read_to_string(&path)
        .ok()
        .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
        .filter(Value::is_object)
        .unwrap_or_else(|| json!({}));
    settings["cursor_account_usage_enabled"] = json!(enabled);
    std::fs::create_dir_all(state_dir)?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(&settings).unwrap_or_default())?;
    std::fs::rename(tmp, path)
}

pub(crate) async fn usage_settings(State(s): State<Arc<AppState>>) -> Json<Value> {
    Json(settings_view(&native_chat::state_dir(&s)).await)
}

pub(crate) async fn put_usage_settings(State(s): State<Arc<AppState>>, Json(body): Json<Value>) -> ApiResult {
    let Some(enabled) = body["cursor_account_usage_enabled"].as_bool() else {
        return Err((
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(json!({"detail": "cursor_account_usage_enabled 必须是布尔值"})),
        ));
    };
    let state_dir = native_chat::state_dir(&s);
    write_usage_settings(&state_dir, enabled).map_err(|e| {
        (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"detail": format!("无法保存用量设置（{:?}）", e.kind())})))
    })?;
    cursor_account::clear_cache(&state_dir);
    Ok(Json(settings_view(&state_dir).await))
}

pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/usage/limits", get(limits))
        .route("/api/usage/quota", get(quota))
        .route("/api/usage/quota/{id}/refresh", post(refresh_quota))
        .route("/api/usage/settings", get(usage_settings).put(put_usage_settings))
        .route("/api/usage/cursor-account", get(cursor_account::read))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codex_prefers_the_codex_bucket_and_maps_window_lengths() {
        let response = json!({
            "rateLimits": {"limitId": "codex_other", "primary": {"usedPercent": 1}},
            "rateLimitsByLimitId": {"codex": {"limitId": "codex", "planType": "pro",
                "primary": {"usedPercent": 88, "windowDurationMins": 10080, "resetsAt": 1788762144},
                "secondary": null}},
        });
        let windows = codex_windows(&response);
        assert_eq!(windows.len(), 1);
        assert_eq!(windows[0]["kind"], "weekly");
        assert_eq!(windows[0]["used_percent"], 88.0);
        assert_eq!(windows[0]["reset_at"], 1788762144.0);
        assert_eq!(windows[0]["window_seconds"], 604800.0);

        let free = json!({"rateLimits": {"planType": "free", "primary": {"usedPercent": 140}}});
        let windows = codex_windows(&free);
        assert_eq!(windows[0]["kind"], "monthly");
        assert_eq!(windows[0]["used_percent"], 100.0);
        assert!(codex_windows(&json!({"rateLimits": {"limitId": "spark", "primary": {"usedPercent": 1}}})).is_empty());
    }

    #[test]
    fn claude_reads_fixed_and_model_scoped_windows() {
        let response = json!({"rate_limits_available": true, "rate_limits": {
            "five_hour": {"utilization": 94, "resets_at": "2026-09-04T21:20:00.5+00:00"},
            "seven_day": {"utilization": 49, "resets_at": "2026-09-05T15:00:00Z"},
            "model_scoped": [{"display_name": "Opus 4.1 (Max)", "utilization": 79}],
        }});
        let windows = claude_windows(&response).unwrap();
        assert_eq!(windows.len(), 3);
        assert_eq!(windows[0]["kind"], "session");
        assert_eq!(windows[0]["reset_at"], 1788556800.5);
        assert_eq!(windows[2]["id"], "seven_day_opus_4_1_max_");
        assert_eq!(windows[2]["label"], "周额度 · Opus 4.1 (Max)");
        let none = claude_windows(&json!({"rate_limits_available": false})).unwrap_err();
        assert_eq!((none.status, none.code), ("unsupported", "no_subscription_limits"));
    }

    #[test]
    fn grok_opencode_and_cursor_windows_follow_the_original_shapes() {
        assert!(grok_windows(&json!({"config": {}})).is_empty());
        let grok = grok_windows(&json!({"config": {"creditUsagePercent": 12.5,
            "currentPeriod": {"type": "USAGE_PERIOD_TYPE_MONTHLY", "end": "2026-11-01T00:00:00Z"}}}));
        assert_eq!(grok[0]["kind"], "monthly");
        assert_eq!(grok[0]["label"], "月额度");

        let usage = json!({"usage": {
            "rolling": {"percent": 10, "resetsAt": 100}, "weekly": {"percent": 20, "resetsAt": 200},
            "monthly": {"percent": 30, "resetsAt": 300}}});
        let windows = opencode_windows(&usage).unwrap();
        assert_eq!(windows.iter().map(|w| w["id"].clone()).collect::<Vec<_>>(), [json!("go_rolling"), json!("go_weekly"), json!("go_monthly")]);
        assert_eq!(windows[2]["window_seconds"], Value::Null);
        assert_eq!(opencode_windows(&json!({"usage": {}})).unwrap_err().code, "invalid_response");

        let cursor = cursor_windows(&json!({"billingCycleEnd": "1790000000000",
            "planUsage": {"totalPercentUsed": 40, "apiPercentUsed": "55.5"}})).unwrap();
        assert_eq!(cursor.len(), 2);
        assert_eq!(cursor[0]["aggregate"], true);
        assert_eq!(cursor[1]["used_percent"], 55.5);
        assert_eq!(cursor[1]["reset_at"], 1790000000.0);
        assert_eq!(cursor_windows(&json!({"planUsage": {}})).unwrap_err().code, "limits_unsupported");
    }

    #[test]
    fn jwt_subjects_must_name_a_user() {
        use base64::Engine;
        let encode = |claims: Value| {
            let body = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(claims.to_string());
            format!("h.{body}.s")
        };
        assert_eq!(jwt_subject(&encode(json!({"sub": "auth0|user_1"}))).unwrap(), "auth0|user_1");
        assert_eq!(jwt_subject(&encode(json!({"sub": "auth0|"}))).unwrap_err().code, "token_invalid");
        assert_eq!(jwt_subject("not-a-jwt").unwrap_err().code, "token_invalid");
    }

    fn snapshot(id: &str, engine: &str, status: &str, updated: f64, fp: Option<&str>) -> Snapshot {
        Snapshot {
            at: Instant::now(),
            row: json!({"id": id, "engine": engine, "label": id, "status": status, "updated_at": updated, "windows": []}),
            fingerprint: fp.map(str::to_owned),
            revision: String::new(),
        }
    }

    #[test]
    fn credentials_for_one_provider_account_are_pooled() {
        let rows = merge(vec![
            snapshot("account:a", "codex", "error", 0.0, Some("f1")),
            snapshot("system:codex", "codex", "ok", 5.0, Some("f1")),
            snapshot("system:claude", "claude", "ok", 1.0, None),
        ]);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0]["id"], "account:a");
        assert_eq!(rows[0]["status"], "ok");
        assert_eq!(rows[0]["credential_ids"], json!(["account:a", "system:codex"]));
        let entry = legacy_entry(&json!({"engine": "codex", "label": "x", "status": "ok",
            "windows": [{"used_percent": 20.0, "window_seconds": 1, "reset_at": 9}, {"used_percent": 70.0, "window_seconds": 2, "reset_at": 8}]}), "account:a");
        assert_eq!(entry["remaining"], 30.0);
        assert_eq!(entry["account_id"], "a");
        assert_eq!(entry["reset_at"], 8);
        assert_eq!(canonical_id("system:Codex").as_deref(), Some("system:codex"));
        assert_eq!(canonical_id("abc").as_deref(), Some("account:abc"));
        assert_eq!(canonical_id("../x"), None);
    }

    #[tokio::test]
    async fn unsupported_engines_and_api_key_accounts_report_typed_status() {
        let dir = tempfile::tempdir().unwrap();
        let store = crate::store::Store::open(&dir.path().join("muteki.db")).unwrap();
        store.put("credential_accounts", "k1", &json!({"account_id": "k1", "engine": "codex", "secret": "sk-test"})).unwrap();
        let s = AppState::new(store);
        let result = s.limits.read(&s, false, None).await.unwrap();
        let rows = result["accounts"].as_array().unwrap();
        let by_id = |id: &str| rows.iter().find(|r| r["id"] == id).cloned().unwrap();
        assert_eq!(by_id("account:k1")["error_code"], "api_key");
        assert_eq!(by_id("account:k1")["status"], "unsupported");
        assert_eq!(by_id("system:pi")["error_code"], "provider_not_supported");
        assert_eq!(result["supported_engines"], json!(SUPPORTED));
        assert!(s.limits.read(&s, false, Some("account:nope")).await.is_err());
    }
}
