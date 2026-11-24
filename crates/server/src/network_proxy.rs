use crate::{AppState, native_chat::node_state_dir, settings_metadata::{redact_secrets, redact_url_userinfo}};
use axum::{Json, Router, extract::State, http::StatusCode, routing::{get, post}};
use serde_json::{Map, Value, json};
use std::{path::{Path, PathBuf}, sync::{Arc, RwLock}, time::{Duration, Instant}};

type ApiResult = Result<Json<Value>, (StatusCode, Json<Value>)>;

const STORE_COLLECTION: &str = "settings";
const STORE_ID: &str = "network_proxy";
const AUTH_FILE: &str = "network_proxy_auth.json";
const LEGACY_FIELD: &str = "worker_model_proxy";
const LEGACY_AUTH_FILE: &str = "worker_model_proxy_auth.json";
const REQUIRED_NO_PROXY: &str = "127.0.0.1,localhost,::1,host.docker.internal";
const DEFAULT_NO_PROXY: &str = "10.0.0.0/8,172.16.0.0/12,192.168.0.0/16";
const TRACE_URL: &str = "https://www.cloudflare.com/cdn-cgi/trace";
const PROBE_TIMEOUT: Duration = Duration::from_secs(15);

pub(crate) const PROXY_ENV_KEYS: [&str; 8] = [
    "HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY", "NO_PROXY",
    "http_proxy", "https_proxy", "all_proxy", "no_proxy",
];
const PROXY_URL_KEYS: [&str; 6] = ["HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY", "http_proxy", "https_proxy", "all_proxy"];
const NODE_ENV_PROXY_KEY: &str = "NODE_USE_ENV_PROXY";

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ResolvedProxy {
    pub mode: String,
    pub proxy_url: Option<String>,
    pub redacted_url: Option<String>,
    pub no_proxy: String,
    pub secrets: Vec<String>,
}

impl ResolvedProxy {
    fn inherit() -> Self {
        Self { mode: "inherit".to_owned(), proxy_url: None, redacted_url: None, no_proxy: String::new(), secrets: Vec::new() }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum PasswordOp { Keep, Set(String), Clear }

static ACTIVE: RwLock<Option<ResolvedProxy>> = RwLock::new(None);

fn invalid(detail: impl Into<String>) -> (StatusCode, Json<Value>) {
    (StatusCode::UNPROCESSABLE_ENTITY, Json(json!({"detail": detail.into()})))
}
fn storage(error: impl std::fmt::Display) -> (StatusCode, Json<Value>) {
    tracing::error!(%error, "network proxy settings storage failed");
    (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"detail": "无法读写网络代理设置"})))
}

fn default_record() -> Value {
    json!({"mode": "inherit", "url": "", "username": "", "no_proxy": ""})
}

fn valid_scheme(scheme: &str) -> bool {
    matches!(scheme, "http" | "https" | "socks5" | "socks5h")
}

fn auth_path(state: &AppState) -> PathBuf {
    node_state_dir(state).join("runtime").join(AUTH_FILE)
}

fn read_auth(path: &Path) -> Map<String, Value> {
    std::fs::read(path).ok()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
        .and_then(|value| value.as_object().cloned())
        .unwrap_or_default()
}

fn saved_password(state: &AppState) -> Option<String> {
    read_auth(&auth_path(state)).get("password").and_then(Value::as_str)
        .filter(|value| !value.is_empty()).map(str::to_owned)
}

fn remove_file(path: &Path) -> Result<(), String> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.to_string()),
    }
}

fn write_password(state: &AppState, password: Option<&str>) -> Result<(), String> {
    let path = auth_path(state);
    match password {
        Some(secret) => {
            let encoded = serde_json::to_vec_pretty(&json!({"password": secret})).map_err(|e| e.to_string())?;
            crate::private_files::write(&node_state_dir(state), &path, &encoded)
        }
        None => remove_file(&path),
    }
}

fn normalize(raw: &Value, saved_password: Option<&str>) -> Result<(Value, PasswordOp, bool), String> {
    let obj = raw.as_object().ok_or("网络代理设置必须是对象")?;
    let mode = obj.get("mode").and_then(Value::as_str).unwrap_or("inherit").trim().to_ascii_lowercase();
    if !matches!(mode.as_str(), "inherit" | "direct" | "custom") {
        return Err("代理模式必须是 inherit、direct 或 custom".to_owned());
    }
    let raw_url = obj.get("url").and_then(Value::as_str).unwrap_or("").trim();
    let (url, embedded_user, embedded_pass) = if raw_url.is_empty() {
        (String::new(), None, None)
    } else {
        let mut parsed = url::Url::parse(raw_url)
            .map_err(|_| "代理地址格式无效，请使用 http://、https://、socks5:// 或 socks5h://".to_owned())?;
        if !valid_scheme(parsed.scheme()) || parsed.host_str().is_none_or(|host| host.trim().is_empty()) {
            return Err("代理地址必须包含有效协议 (http/https/socks5/socks5h) 与主机名".to_owned());
        }
        let user = (!parsed.username().is_empty()).then(|| parsed.username().to_owned());
        let pass = parsed.password().filter(|value| !value.is_empty()).map(str::to_owned);
        let _ = parsed.set_username("");
        let _ = parsed.set_password(None);
        reqwest::Proxy::all(parsed.as_str()).map_err(|error| format!("代理地址无法使用：{error}"))?;
        (parsed.to_string(), user, pass)
    };
    if mode == "custom" && url.is_empty() {
        return Err("自定义代理需要填写代理地址".to_owned());
    }
    let username = obj.get("username").and_then(Value::as_str).map(str::trim)
        .filter(|value| !value.is_empty()).map(str::to_owned).or(embedded_user).unwrap_or_default();
    let no_proxy = obj.get("no_proxy").and_then(Value::as_str).unwrap_or("")
        .split(',').map(str::trim).filter(|entry| !entry.is_empty()).collect::<Vec<_>>().join(",");
    let supplied = obj.get("password").and_then(Value::as_str).filter(|value| !value.is_empty())
        .map(str::to_owned).or(embedded_pass);
    let (op, has_password) = if obj.get("clear_password").and_then(Value::as_bool) == Some(true) {
        (PasswordOp::Clear, false)
    } else if let Some(secret) = supplied {
        (PasswordOp::Set(secret), true)
    } else {
        (PasswordOp::Keep, saved_password.is_some())
    };
    Ok((json!({"mode": mode, "url": url, "username": username, "no_proxy": no_proxy}), op, has_password))
}

fn effective_no_proxy(user: &str) -> String {
    let extra = if user.trim().is_empty() { DEFAULT_NO_PROXY } else { user };
    let mut entries: Vec<&str> = Vec::new();
    for entry in REQUIRED_NO_PROXY.split(',').chain(extra.split(',')).map(str::trim) {
        if !entry.is_empty() && !entries.iter().any(|seen| seen.eq_ignore_ascii_case(entry)) {
            entries.push(entry);
        }
    }
    entries.join(",")
}

fn resolve_from(record: &Value, password: Option<&str>, rewrite_localhost_for_bridge: bool) -> ResolvedProxy {
    let mode = record["mode"].as_str().unwrap_or("inherit");
    let mode = if matches!(mode, "inherit" | "direct" | "custom") { mode } else { "inherit" };
    let mut resolved = ResolvedProxy { mode: mode.to_owned(), ..ResolvedProxy::inherit() };
    if mode != "custom" {
        return resolved;
    }
    resolved.no_proxy = effective_no_proxy(record["no_proxy"].as_str().unwrap_or(""));
    let Ok(mut parsed) = url::Url::parse(record["url"].as_str().unwrap_or("").trim()) else {
        return resolved;
    };
    if !valid_scheme(parsed.scheme()) || parsed.host_str().is_none_or(|host| host.trim().is_empty()) {
        return resolved;
    }
    if rewrite_localhost_for_bridge && matches!(parsed.host_str(), Some("127.0.0.1" | "localhost" | "[::1]" | "::1")) {
        let _ = parsed.set_host(Some("host.docker.internal"));
    }
    let username = record["username"].as_str().map(str::trim).filter(|value| !value.is_empty());
    if let Some(username) = username {
        let _ = parsed.set_username(username);
    }
    if let Some(secret) = password {
        resolved.secrets.push(secret.to_owned());
        let _ = parsed.set_password(Some(secret));
    }
    let full = parsed.to_string();
    if reqwest::Proxy::all(&full).is_err() {
        tracing::warn!(proxy = %redact_url_userinfo(&full), "saved network proxy is unusable");
        return resolved;
    }
    let redacted = redact_url_userinfo(&full);
    if redacted != full {
        resolved.secrets.push(full.clone());
    }
    resolved.redacted_url = Some(redacted);
    resolved.proxy_url = Some(full);
    resolved
}

fn migrate_legacy(state: &AppState) -> Result<Value, String> {
    let templates = state.store.get(STORE_COLLECTION, "worker_templates").map_err(|e| e.to_string())?;
    let legacy_auth = node_state_dir(state).join("runtime").join(LEGACY_AUTH_FILE);
    let legacy_password = read_auth(&legacy_auth).get("global").and_then(Value::as_str)
        .filter(|value| !value.is_empty()).map(str::to_owned);
    let legacy = templates.as_ref().and_then(|row| row.get(LEGACY_FIELD)).filter(|value| value.is_object());
    let record = match legacy.map(|value| normalize(value, None)) {
        Some(Ok((record, _, _))) => record,
        Some(Err(error)) => {
            tracing::warn!(%error, "legacy Worker model proxy could not be migrated; using defaults");
            default_record()
        }
        None => default_record(),
    };
    if legacy_password.is_some() && !record["url"].as_str().unwrap_or("").is_empty() {
        write_password(state, legacy_password.as_deref())?;
    }
    state.store.put(STORE_COLLECTION, STORE_ID, &record).map_err(|e| e.to_string())?;
    if let Some(mut row) = templates.filter(|row| row.get(LEGACY_FIELD).is_some()) {
        if let Some(obj) = row.as_object_mut() {
            obj.remove(LEGACY_FIELD);
        }
        state.store.put(STORE_COLLECTION, "worker_templates", &row).map_err(|e| e.to_string())?;
    }
    remove_file(&legacy_auth)?;
    Ok(record)
}

fn stored(state: &AppState) -> Result<Value, String> {
    match state.store.get(STORE_COLLECTION, STORE_ID).map_err(|e| e.to_string())? {
        Some(record) if record.is_object() => Ok(record),
        _ => migrate_legacy(state),
    }
}

pub(crate) fn resolve(state: &AppState, rewrite_localhost_for_bridge: bool) -> ResolvedProxy {
    match stored(state) {
        Ok(record) => resolve_from(&record, saved_password(state).as_deref(), rewrite_localhost_for_bridge),
        Err(error) => {
            tracing::error!(%error, "network proxy settings unavailable; inheriting process environment");
            ResolvedProxy::inherit()
        }
    }
}

pub(crate) fn active() -> ResolvedProxy {
    ACTIVE.read().unwrap_or_else(|poison| poison.into_inner()).clone().unwrap_or_else(ResolvedProxy::inherit)
}

pub fn refresh(state: &AppState) {
    let resolved = resolve(state, false);
    *ACTIVE.write().unwrap_or_else(|poison| poison.into_inner()) = Some(resolved);
    crate::host_process::sync_host_environment();
}

pub(crate) fn env_pairs(resolved: &ResolvedProxy) -> Vec<(String, String)> {
    match resolved.mode.as_str() {
        "custom" => {
            let Some(url) = resolved.proxy_url.as_ref() else { return Vec::new() };
            let mut env: Vec<(String, String)> = PROXY_URL_KEYS.iter().map(|key| ((*key).to_owned(), url.clone())).collect();
            env.push(("NO_PROXY".to_owned(), resolved.no_proxy.clone()));
            env.push(("no_proxy".to_owned(), resolved.no_proxy.clone()));
            if url.starts_with("http://") || url.starts_with("https://") {
                env.push((NODE_ENV_PROXY_KEY.to_owned(), "1".to_owned()));
            }
            env
        }
        "direct" => PROXY_ENV_KEYS.iter().map(|key| ((*key).to_owned(), String::new())).collect(),
        _ => Vec::new(),
    }
}

pub fn host_env_overlay() -> Vec<(String, String)> {
    env_pairs(&active())
}

#[allow(clippy::disallowed_methods)]
fn base_client_builder() -> reqwest::ClientBuilder {
    reqwest::Client::builder()
}

fn apply_to_builder(builder: reqwest::ClientBuilder, resolved: &ResolvedProxy) -> reqwest::ClientBuilder {
    match resolved.mode.as_str() {
        "custom" => match resolved.proxy_url.as_deref().map(reqwest::Proxy::all) {
            Some(Ok(proxy)) => builder.proxy(proxy.no_proxy(reqwest::NoProxy::from_string(&resolved.no_proxy))),
            _ => builder,
        },
        "direct" => builder.no_proxy(),
        _ => builder,
    }
}

pub fn http_client_builder() -> reqwest::ClientBuilder {
    apply_to_builder(base_client_builder(), &active())
}

pub fn direct_client_builder() -> reqwest::ClientBuilder {
    base_client_builder().no_proxy()
}

fn view(state: &AppState) -> Result<Value, String> {
    let record = stored(state)?;
    let resolved = resolve_from(&record, saved_password(state).as_deref(), false);
    Ok(json!({
        "mode": record["mode"],
        "url": record["url"],
        "username": record["username"],
        "no_proxy": record["no_proxy"],
        "has_password": saved_password(state).is_some(),
        "effective_no_proxy": effective_no_proxy(record["no_proxy"].as_str().unwrap_or("")),
        "required_no_proxy": REQUIRED_NO_PROXY,
        "default_no_proxy": DEFAULT_NO_PROXY,
        "proxy_display": resolved.redacted_url,
        "test_url": TRACE_URL,
    }))
}

pub(crate) fn save(state: &AppState, body: &Value) -> Result<Value, (StatusCode, Json<Value>)> {
    let saved = saved_password(state);
    let (record, op, _) = normalize(body, saved.as_deref()).map_err(invalid)?;
    stored(state).map_err(storage)?;
    match op {
        PasswordOp::Set(secret) => write_password(state, Some(&secret)),
        PasswordOp::Clear => write_password(state, None),
        PasswordOp::Keep => Ok(()),
    }.map_err(storage)?;
    state.store.put(STORE_COLLECTION, STORE_ID, &record).map_err(storage)?;
    view(state).map_err(storage)
}

fn parse_trace(body: &str) -> (Option<String>, Option<String>) {
    let field = |name: &str| body.lines().find_map(|line| line.strip_prefix(name)?.strip_prefix('=').map(|v| v.trim().to_owned()))
        .filter(|value| !value.is_empty());
    (field("ip"), field("loc"))
}

fn error_chain(error: &dyn std::error::Error) -> String {
    let mut text = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        text.push_str("：");
        text.push_str(&cause.to_string());
        source = cause.source();
    }
    text
}

fn leg(started: Instant, outcome: Result<String, String>, secrets: &[&str]) -> Value {
    let elapsed_ms = started.elapsed().as_millis() as u64;
    match outcome {
        Ok(body) => match parse_trace(&body) {
            (Some(ip), loc) => json!({"ok": true, "ip": ip, "loc": loc, "elapsed_ms": elapsed_ms}),
            (None, _) => json!({"ok": false, "error": "出口检测地址没有返回 IP", "elapsed_ms": elapsed_ms}),
        },
        Err(error) => json!({"ok": false, "error": redact_secrets(&error, secrets), "elapsed_ms": elapsed_ms}),
    }
}

async fn probe_client(builder: reqwest::ClientBuilder, secrets: &[&str]) -> Value {
    let started = Instant::now();
    let outcome = async {
        let client = builder.timeout(PROBE_TIMEOUT).connect_timeout(Duration::from_secs(10)).build()
            .map_err(|error| format!("HTTP 客户端不可用：{error}"))?;
        let response = client.get(TRACE_URL).send().await.map_err(|error| format!("请求失败：{}", error_chain(&error)))?;
        let status = response.status();
        let body = response.text().await.map_err(|error| format!("读取响应失败：{error}"))?;
        if !status.is_success() {
            return Err(format!("出口检测地址返回 HTTP {status}"));
        }
        Ok(body)
    }.await;
    leg(started, outcome, secrets)
}

async fn probe_process(resolved: &ResolvedProxy, secrets: &[&str]) -> Value {
    let started = Instant::now();
    let max_time = PROBE_TIMEOUT.as_secs().to_string();
    let mut command = crate::host_process::command("curl");
    command.args(["-sS", "--max-time", max_time.as_str(), TRACE_URL])
        .stdin(std::process::Stdio::null()).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    for key in PROXY_ENV_KEYS.iter().chain([&NODE_ENV_PROXY_KEY]) {
        match std::env::var_os(key) {
            Some(value) => { command.env(key, value); }
            None => { command.env_remove(key); }
        }
    }
    for (key, value) in env_pairs(resolved) {
        if value.is_empty() { command.env_remove(&key); } else { command.env(&key, &value); }
    }
    let outcome = match tokio::time::timeout(PROBE_TIMEOUT + Duration::from_secs(5), command.output()).await {
        Err(_) => Err("子进程请求超时".to_owned()),
        Ok(Err(error)) => Err(format!("无法启动 curl：{error}")),
        Ok(Ok(output)) if output.status.success() => Ok(String::from_utf8_lossy(&output.stdout).into_owned()),
        Ok(Ok(output)) => Err(format!("curl 退出码 {}：{}", output.status.code().map_or("signal".to_owned(), |code| code.to_string()),
            String::from_utf8_lossy(&output.stderr).trim())),
    };
    leg(started, outcome, secrets)
}

fn verdict(mode: &str, direct: &Value, server: &Value, process: &Value) -> &'static str {
    let ok = |leg: &Value| leg["ok"] == true;
    if !ok(server) || !ok(process) {
        return "failed";
    }
    match mode {
        "custom" if server["ip"] != process["ip"] => "mismatch",
        "custom" if ok(direct) && direct["ip"] == server["ip"] => "same_exit",
        "custom" => "proxied",
        "direct" => "direct",
        _ => "inherit",
    }
}

async fn test_proxy(State(state): State<Arc<AppState>>, Json(body): Json<Value>) -> ApiResult {
    let saved = saved_password(&state);
    let draft = if body.as_object().is_some_and(|obj| !obj.is_empty()) { body } else { stored(&state).map_err(storage)? };
    let (record, op, _) = normalize(&draft, saved.as_deref()).map_err(invalid)?;
    let password = match op { PasswordOp::Set(secret) => Some(secret), PasswordOp::Clear => None, PasswordOp::Keep => saved };
    let resolved = resolve_from(&record, password.as_deref(), false);
    let secrets: Vec<&str> = resolved.secrets.iter().map(String::as_str).collect();
    let (direct, server, process) = tokio::join!(
        probe_client(direct_client_builder(), &secrets),
        probe_client(apply_to_builder(base_client_builder(), &resolved), &secrets),
        probe_process(&resolved, &secrets),
    );
    let status = verdict(&resolved.mode, &direct, &server, &process);
    Ok(Json(json!({
        "ok": matches!(status, "proxied" | "direct" | "inherit"),
        "status": status,
        "mode": resolved.mode,
        "proxy_display": resolved.redacted_url,
        "test_url": TRACE_URL,
        "legs": {"direct": direct, "server": server, "process": process},
    })))
}

async fn get_proxy(State(state): State<Arc<AppState>>) -> ApiResult {
    view(&state).map(Json).map_err(storage)
}

async fn put_proxy(State(state): State<Arc<AppState>>, Json(body): Json<Value>) -> ApiResult {
    let saved = save(&state, &body)?;
    refresh(&state);
    Ok(Json(saved))
}

pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/settings/network-proxy", get(get_proxy).put(put_proxy))
        .route("/api/settings/network-proxy/test", post(test_proxy))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_state() -> (tempfile::TempDir, AppState) {
        let dir = tempfile::tempdir().unwrap();
        let state = AppState::new(crate::store::Store::open(":memory:").unwrap());
        state.store.put("system", "state_dir", &json!(dir.path())).unwrap();
        state.store.put("system", "node_state_dir", &json!(dir.path())).unwrap();
        (dir, state)
    }

    #[test]
    fn normalize_strips_userinfo_and_tracks_password_ops() {
        let (record, op, has_password) = normalize(&json!({
            "mode": "custom", "url": "http://user1:secret1@127.0.0.1:7890", "no_proxy": " localhost, ,10.0.0.0/8 "
        }), None).unwrap();
        assert_eq!(record["url"], "http://127.0.0.1:7890/");
        assert_eq!(record["username"], "user1");
        assert_eq!(record["no_proxy"], "localhost,10.0.0.0/8");
        assert!(has_password);
        assert_eq!(op, PasswordOp::Set("secret1".to_owned()));

        let (_, kept, kept_has) = normalize(&json!({"mode": "custom", "url": "socks5h://127.0.0.1:1080"}), Some("old")).unwrap();
        assert_eq!(kept, PasswordOp::Keep);
        assert!(kept_has);
        let (_, cleared, cleared_has) = normalize(&json!({"mode": "custom", "url": "http://p:1", "clear_password": true}), Some("old")).unwrap();
        assert_eq!(cleared, PasswordOp::Clear);
        assert!(!cleared_has);

        assert!(normalize(&json!({"mode": "custom", "url": "ftp://127.0.0.1:21"}), None).is_err());
        assert!(normalize(&json!({"mode": "custom", "url": ""}), None).is_err());
        assert!(normalize(&json!({"mode": "proxy"}), None).is_err());
        assert!(normalize(&json!({"mode": "direct", "url": "http://127.0.0.1:7890"}), None).is_ok());
    }

    #[test]
    fn resolve_injects_credentials_rewrites_bridge_and_keeps_loopback_direct() {
        let record = json!({"mode": "custom", "url": "http://127.0.0.1:7890/", "username": "alice", "no_proxy": ""});
        let host = resolve_from(&record, Some("pw-value"), false);
        assert_eq!(host.proxy_url.as_deref(), Some("http://alice:pw-value@127.0.0.1:7890/"));
        assert_eq!(host.redacted_url.as_deref(), Some("http://***:***@127.0.0.1:7890/"));
        assert!(host.secrets.contains(&"pw-value".to_owned()));
        assert_eq!(host.no_proxy, format!("{REQUIRED_NO_PROXY},{DEFAULT_NO_PROXY}"));
        let bridge = resolve_from(&record, Some("pw-value"), true);
        assert_eq!(bridge.proxy_url.as_deref(), Some("http://alice:pw-value@host.docker.internal:7890/"));

        let custom_bypass = resolve_from(&json!({"mode": "custom", "url": "http://p:1", "no_proxy": "localhost,.corp"}), None, false);
        assert_eq!(custom_bypass.no_proxy, format!("{REQUIRED_NO_PROXY},.corp"));

        assert_eq!(resolve_from(&json!({"mode": "direct", "url": "http://p:1"}), None, true).proxy_url, None);
        assert_eq!(resolve_from(&json!({"mode": "bogus"}), None, false).mode, "inherit");
    }

    #[test]
    fn env_pairs_cover_all_modes() {
        let custom = resolve_from(&json!({"mode": "custom", "url": "http://p:1/", "no_proxy": ""}), None, false);
        let env = env_pairs(&custom);
        assert!(env.contains(&("HTTPS_PROXY".to_owned(), "http://p:1/".to_owned())));
        assert!(env.contains(&("no_proxy".to_owned(), custom.no_proxy.clone())));
        assert!(env.contains(&(NODE_ENV_PROXY_KEY.to_owned(), "1".to_owned())));
        let socks = resolve_from(&json!({"mode": "custom", "url": "socks5h://p:1"}), None, false);
        assert!(!env_pairs(&socks).iter().any(|(key, _)| key == NODE_ENV_PROXY_KEY));
        let direct = env_pairs(&resolve_from(&json!({"mode": "direct"}), None, false));
        assert_eq!(direct.len(), PROXY_ENV_KEYS.len());
        assert!(direct.iter().all(|(_, value)| value.is_empty()));
        assert!(env_pairs(&ResolvedProxy::inherit()).is_empty());
    }

    #[test]
    fn legacy_worker_proxy_moves_to_global_setting_once() {
        let (dir, state) = test_state();
        state.store.put("settings", "worker_templates", &json!({
            "max_workers": 3,
            "worker_model_proxy": {"mode": "custom", "url": "http://127.0.0.1:7890", "username": "u", "no_proxy": ".corp"}
        })).unwrap();
        let runtime = dir.path().join("runtime");
        std::fs::create_dir_all(&runtime).unwrap();
        crate::private_files::write(dir.path(), &runtime.join(LEGACY_AUTH_FILE), br#"{"global":"pw"}"#).unwrap();

        let migrated = view(&state).unwrap();
        assert_eq!(migrated["mode"], "custom");
        assert_eq!(migrated["url"], "http://127.0.0.1:7890/");
        assert_eq!(migrated["no_proxy"], ".corp");
        assert_eq!(migrated["has_password"], true);
        let templates = state.store.get("settings", "worker_templates").unwrap().unwrap();
        assert!(templates.get(LEGACY_FIELD).is_none());
        assert_eq!(templates["max_workers"], 3);
        assert!(!runtime.join(LEGACY_AUTH_FILE).exists());
        assert_eq!(resolve(&state, false).proxy_url.as_deref(), Some("http://u:pw@127.0.0.1:7890/"));

        save(&state, &json!({"mode": "direct", "url": "http://127.0.0.1:7890", "clear_password": true})).unwrap();
        let after = view(&state).unwrap();
        assert_eq!(after["mode"], "direct");
        assert_eq!(after["has_password"], false);
    }

    #[test]
    fn trace_parsing_and_verdicts() {
        assert_eq!(parse_trace("fl=1\nip=203.0.113.9\nloc=JP\n"), (Some("203.0.113.9".to_owned()), Some("JP".to_owned())));
        let ok = |ip: &str| json!({"ok": true, "ip": ip});
        let failed = json!({"ok": false});
        assert_eq!(verdict("custom", &ok("1"), &ok("2"), &ok("2")), "proxied");
        assert_eq!(verdict("custom", &ok("1"), &ok("1"), &ok("1")), "same_exit");
        assert_eq!(verdict("custom", &failed, &ok("2"), &ok("3")), "mismatch");
        assert_eq!(verdict("custom", &ok("1"), &failed, &ok("2")), "failed");
        assert_eq!(verdict("direct", &ok("1"), &ok("1"), &ok("1")), "direct");
    }
}
