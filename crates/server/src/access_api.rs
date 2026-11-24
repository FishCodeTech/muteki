//! Service-owned password settings, audience-bound sessions and one-use stream tickets.
//! Construct once from the target state root, merge `router(access.clone())`, and apply
//! `middleware::from_fn_with_state(access, gate)` around all API routes.
use axum::{
    Json, Router,
    body::Body,
    extract::{ConnectInfo, Request, State},
    http::{HeaderMap, HeaderValue, Method, StatusCode, header},
    middleware::Next,
    response::{IntoResponse, Response},
    routing::{get, post},
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD as B64};
use futures_util::{StreamExt, stream};
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    io::Write,
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

type HmacSha256 = Hmac<Sha256>;
type Failure = (StatusCode, Json<Value>);
type Result<T> = std::result::Result<T, Failure>;

#[derive(Clone, Serialize, Deserialize)]
struct PrivateConfig {
    version: u32,
    mode: String,
    password_hash: String,
    environment_hash: String,
    signing_key: String,
    revision: String,
    revoked: HashMap<String, i64>,
    secret_fingerprint: String,
}
#[derive(Clone, Serialize, Deserialize)]
struct Payload {
    v: u32,
    aud: String,
    rev: String,
    iat: i64,
    exp: i64,
    jti: String,
}
struct Ticket {
    expires: i64,
    token: Option<String>,
    revision: String,
}
struct Inner {
    config: PrivateConfig,
    tickets: HashMap<String, Ticket>,
    failures: HashMap<String, Vec<Instant>>,
}
/// Does not load or migrate credentials from any other installation.
pub struct Access {
    path: PathBuf,
    service_id: String,
    explicit_secret: String,
    ttl: i64,
    origins: Vec<String>,
    /// The service or its UI listens beyond loopback, so a password must stay set.
    exposed: bool,
    inner: Mutex<Inner>,
}
/// Added only for a verified session. Public share handlers must check this when
/// the stored share uses `access_mode=authenticated`.
#[derive(Clone)]
pub struct AuthenticatedSession {
    pub expires_at: i64,
}

fn failure(status: StatusCode, code: &str, message: &str) -> Failure {
    (
        status,
        Json(json!({"error":{"code":code,"message":message}})),
    )
}
fn unavailable(error: impl std::fmt::Display) -> Failure {
    tracing::error!(%error, "Access configuration unavailable");
    failure(
        StatusCode::SERVICE_UNAVAILABLE,
        "auth.config_unavailable",
        "访问配置不可用，请修复后重启服务。",
    )
}
fn now() -> i64 {
    chrono::Utc::now().timestamp()
}
fn random(size: usize) -> anyhow::Result<String> {
    let mut bytes = vec![0; size];
    getrandom::getrandom(&mut bytes)
        .map_err(|e| anyhow::anyhow!("OS randomness unavailable: {e}"))?;
    Ok(B64.encode(bytes))
}
fn fingerprint(secret: &str) -> String {
    if secret.is_empty() {
        String::new()
    } else {
        format!("{:x}", Sha256::digest(secret.as_bytes()))
    }
}
fn hash_parts(hash: &str) -> Option<(Vec<u8>, Vec<u8>)> {
    let parts: Vec<_> = hash.split('$').collect();
    if parts.len() != 3 || parts[0] != "scrypt-v1" {
        return None;
    }
    let salt = B64.decode(parts[1]).ok()?;
    let digest = B64.decode(parts[2]).ok()?;
    (salt.len() == 16 && digest.len() == 64).then_some((salt, digest))
}
fn password_hash(password: &str) -> anyhow::Result<String> {
    let salt = random(16)?;
    let mut digest = [0u8; 64];
    scrypt::scrypt(
        password.as_bytes(),
        &B64.decode(&salt)?,
        &scrypt::Params::new(15, 8, 1, 64)?,
        &mut digest,
    )?;
    Ok(format!("scrypt-v1${salt}${}", B64.encode(digest)))
}
fn matches(hash: &str, password: &str) -> bool {
    if password.len() > 1024 {
        return false;
    }
    let Some((salt, digest)) = hash_parts(hash) else {
        return false;
    };
    let mut actual = [0u8; 64];
    let Ok(params) = scrypt::Params::new(15, 8, 1, 64) else {
        return false;
    };
    if scrypt::scrypt(password.as_bytes(), &salt, &params, &mut actual).is_err() {
        return false;
    }
    let mut expected = HmacSha256::new_from_slice(b"muteki password comparison").expect("HMAC key");
    expected.update(&digest);
    let tag = expected.finalize().into_bytes();
    let mut candidate =
        HmacSha256::new_from_slice(b"muteki password comparison").expect("HMAC key");
    candidate.update(&actual);
    candidate.verify_slice(&tag).is_ok()
}
fn effective(config: &PrivateConfig) -> &str {
    if config.mode == "settings" {
        &config.password_hash
    } else {
        &config.environment_hash
    }
}
fn read_private(path: &Path) -> anyhow::Result<PrivateConfig> {
    let cfg: PrivateConfig = serde_json::from_slice(&std::fs::read(path)?)?;
    anyhow::ensure!(
        cfg.version == 1
            && matches!(cfg.mode.as_str(), "settings" | "environment")
            && B64.decode(&cfg.signing_key)?.len() == 32
            && cfg.revision.len() >= 32
            && [&cfg.password_hash, &cfg.environment_hash]
                .iter()
                .all(|s| s.is_empty() || hash_parts(s).is_some())
            && (cfg.mode != "settings" || !cfg.password_hash.is_empty()),
        "invalid access configuration"
    );
    Ok(cfg)
}
fn file_lock(path: &Path) -> anyhow::Result<std::fs::File> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("missing parent"))?;
    std::fs::create_dir_all(parent)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))?;
    }
    let mut options = std::fs::OpenOptions::new();
    options.read(true).write(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options.open(path.with_extension("lock"))?;
    fs2::FileExt::lock_exclusive(&file)?;
    Ok(file)
}
fn write_private(path: &Path, config: &PrivateConfig) -> anyhow::Result<()> {
    let mut temporary = tempfile::NamedTempFile::new_in(path.parent().unwrap())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        temporary
            .as_file()
            .set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    serde_json::to_writer(&mut temporary, config)?;
    temporary.flush()?;
    temporary.as_file().sync_all()?;
    temporary.persist(path).map_err(|e| e.error)?;
    Ok(())
}
impl Access {
    /// Call only at ordinary service startup, using this target's installation ID.
    /// A missing password leaves authentication disabled, matching ordinary local use.
    pub fn from_env(
        state_root: &Path,
        service_id: String,
        exposed: bool,
    ) -> anyhow::Result<Arc<Self>> {
        let path = state_root.join("_secrets/web-access.json");
        let _file = file_lock(&path)?;
        let explicit_secret = std::env::var("MUTEKI_WEB_AUTH_SECRET")
            .unwrap_or_default()
            .trim()
            .to_owned();
        let password = std::env::var("MUTEKI_WEB_PASSWORD")
            .unwrap_or_default()
            .trim()
            .to_owned();
        let existed = path.exists();
        let mut config = if existed {
            read_private(&path)?
        } else {
            PrivateConfig {
                version: 1,
                mode: "environment".into(),
                password_hash: String::new(),
                environment_hash: String::new(),
                signing_key: random(32)?,
                revision: random(36)?,
                revoked: HashMap::new(),
                secret_fingerprint: String::new(),
            }
        };
        let changed = password.is_empty() != config.environment_hash.is_empty()
            || (!password.is_empty() && !matches(&config.environment_hash, &password));
        let secret_changed = config.secret_fingerprint != fingerprint(&explicit_secret);
        if changed {
            config.environment_hash = if password.is_empty() {
                String::new()
            } else {
                password_hash(&password)?
            };
        }
        if (changed && config.mode == "environment") || secret_changed {
            config.revision = random(36)?;
            config.revoked.clear();
        }
        config.secret_fingerprint = fingerprint(&explicit_secret);
        if !existed || changed || secret_changed {
            write_private(&path, &config)?;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
        }
        let ttl = std::env::var("MUTEKI_WEB_TOKEN_TTL")
            .ok()
            .and_then(|s| s.parse::<i64>().ok())
            .filter(|v| *v > 0)
            .unwrap_or(43200);
        let port = std::env::var("MUTEKI_UI_PORT").unwrap_or_else(|_| "3001".into());
        let mut origins = vec![
            format!("http://127.0.0.1:{port}"),
            format!("http://localhost:{port}"),
            format!("http://[::1]:{port}"),
        ];
        let desktop_origin =
            std::env::var("MUTEKI_UI_ORIGIN").unwrap_or_else(|_| "http://127.0.0.1:3101".into());
        if let Ok(url) = reqwest::Url::parse(&desktop_origin) {
            if url.scheme() == "http" && url.host_str() == Some("127.0.0.1") {
                origins.push(url.origin().ascii_serialization());
            }
        }
        origins.extend(
            std::env::var("MUTEKI_WEB_ORIGINS")
                .unwrap_or_default()
                .split(',')
                .map(|s| s.trim().trim_end_matches('/').to_owned())
                .filter(|s| !s.is_empty()),
        );
        Ok(Arc::new(Self {
            path,
            service_id,
            explicit_secret,
            ttl,
            origins,
            exposed,
            inner: Mutex::new(Inner {
                config,
                tickets: HashMap::new(),
                failures: HashMap::new(),
            }),
        }))
    }
    pub fn is_exposed(&self) -> bool {
        self.exposed
    }
    /// A service reachable beyond loopback must not start without a password.
    pub fn check_exposure(&self, bind: &str) -> anyhow::Result<()> {
        if self.exposed
            && !self
                .enabled()
                .map_err(|(_, Json(error))| anyhow::anyhow!("{error}"))?
        {
            anyhow::bail!(
                "Refusing to start: non-loopback bind {bind} requires an access password. Set MUTEKI_WEB_PASSWORD, or configure a password in Settings on loopback first."
            );
        }
        Ok(())
    }
    fn refresh(&self, inner: &mut Inner) -> Result<()> {
        let config = read_private(&self.path).map_err(unavailable)?;
        if config.secret_fingerprint != fingerprint(&self.explicit_secret) {
            return Err(unavailable("signing environment changed; restart required"));
        }
        inner.config = config;
        Ok(())
    }
    fn key(&self, cfg: &PrivateConfig) -> Option<Vec<u8>> {
        let mut mac = HmacSha256::new_from_slice(&B64.decode(&cfg.signing_key).ok()?).ok()?;
        mac.update(self.explicit_secret.as_bytes());
        Some(mac.finalize().into_bytes().to_vec())
    }
    fn verify(&self, cfg: &PrivateConfig, token: &str) -> Option<Payload> {
        if token.len() > 4096 {
            return None;
        }
        let (body, signature) = token.split_once('.')?;
        let bytes = B64.decode(body).ok()?;
        let mut mac = HmacSha256::new_from_slice(&self.key(cfg)?).ok()?;
        mac.update(&bytes);
        mac.verify_slice(&B64.decode(signature).ok()?).ok()?;
        let p: Payload = serde_json::from_slice(&bytes).ok()?;
        (p.v == 2
            && p.aud == self.service_id
            && p.rev == cfg.revision
            && p.exp > now()
            && p.iat <= now().saturating_add(30)
            && !p.jti.is_empty()
            && !cfg.revoked.contains_key(&p.jti))
        .then_some(p)
    }
    fn issue(&self, cfg: &PrivateConfig) -> Result<(String, i64)> {
        let exp = now().saturating_add(self.ttl);
        let bytes = serde_json::to_vec(&Payload {
            v: 2,
            aud: self.service_id.clone(),
            rev: cfg.revision.clone(),
            iat: now(),
            exp,
            jti: random(24).map_err(unavailable)?,
        })
        .map_err(unavailable)?;
        let mut mac = HmacSha256::new_from_slice(
            &self
                .key(cfg)
                .ok_or_else(|| unavailable("invalid signing key"))?,
        )
        .map_err(unavailable)?;
        mac.update(&bytes);
        Ok((
            format!(
                "{}.{}",
                B64.encode(&bytes),
                B64.encode(mac.finalize().into_bytes())
            ),
            exp,
        ))
    }
    fn cookie_name(&self) -> String {
        format!(
            "muteki_session_{:x}",
            Sha256::digest(self.service_id.as_bytes())
        )[..31]
            .to_owned()
    }
    fn settings(&self, cfg: &PrivateConfig) -> Value {
        let enabled = !effective(cfg).is_empty();
        json!({"auth_required":enabled,"source":if enabled {cfg.mode.as_str()} else {"none"},"environment_configured":!cfg.environment_hash.is_empty(),"has_override":cfg.mode=="settings","session_ttl_seconds":self.ttl,"can_disable":!self.exposed,"service_id":self.service_id,"authentication_supported":true,"mutation_supported":true,"local_only":!self.exposed,"exposed":self.exposed})
    }
    pub fn enabled(&self) -> Result<bool> {
        let mut inner = self.inner.lock().map_err(unavailable)?;
        self.refresh(&mut inner)?;
        Ok(!effective(&inner.config).is_empty())
    }
    pub(crate) fn enabled_for_state(state_root: &Path) -> Result<bool> {
        let path = state_root.join("_secrets/web-access.json");
        if !path.exists() {
            return Ok(!std::env::var("MUTEKI_WEB_PASSWORD")
                .unwrap_or_default()
                .trim()
                .is_empty());
        }
        let cfg = read_private(&path).map_err(unavailable)?;
        Ok(!effective(&cfg).is_empty())
    }
    /// Use for authenticated share reads that deliberately bypass the global gate.
    pub fn authenticated(&self, headers: &HeaderMap) -> Result<bool> {
        let mut inner = self.inner.lock().map_err(unavailable)?;
        self.refresh(&mut inner)?;
        Ok(request_token(self, headers).is_some_and(|t| self.verify(&inner.config, &t).is_some()))
    }
    fn identity(&self, enabled: bool) -> Value {
        json!({"auth_required":enabled,"session_protocol":2,"service_id":self.service_id,"identity_id":"operator"})
    }
}
fn header_text<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name)?.to_str().ok()
}
fn bearer(headers: &HeaderMap) -> Option<String> {
    let (scheme, token) = header_text(headers, "authorization")?.split_once(' ')?;
    (scheme.eq_ignore_ascii_case("bearer") && !token.trim().is_empty())
        .then(|| token.trim().to_owned())
}
fn cookie(access: &Access, headers: &HeaderMap) -> Option<String> {
    let name = access.cookie_name();
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .find_map(|part| {
            let (key, value) = part.trim().split_once('=')?;
            (key == name).then(|| value.to_owned())
        })
}
fn request_token(access: &Access, headers: &HeaderMap) -> Option<String> {
    bearer(headers).or_else(|| cookie(access, headers))
}
fn response(value: Value) -> Response {
    let mut response = Json(value).into_response();
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}
fn clear_cookie(access: &Access, response: &mut Response) {
    response.headers_mut().insert(
        header::SET_COOKIE,
        HeaderValue::from_str(&format!(
            "{}=; Path=/api; Max-Age=0; HttpOnly; SameSite=Strict",
            access.cookie_name()
        ))
        .expect("cookie name"),
    );
}
pub fn router<S: Clone + Send + Sync + 'static>(access: Arc<Access>) -> Router<S> {
    Router::new()
        .route("/api/auth/status", get(status))
        .route("/api/auth/login", post(login))
        .route("/api/auth/me", get(me))
        .route("/api/auth/logout", post(logout))
        .route("/api/auth/ticket", post(ticket))
        .route("/api/settings/access", get(settings).put(update))
        .with_state(access)
}
async fn status(State(access): State<Arc<Access>>) -> Result<Response> {
    let mut identity = access.identity(access.enabled()?);
    identity["exposed"] = json!(access.exposed);
    identity["proxy_key_fingerprint"] = json!(std::env::var("MUTEKI_WEB_PROXY_KEY").ok().map(|key| fingerprint(&key)));
    Ok(response(identity))
}
async fn me(State(access): State<Arc<Access>>, headers: HeaderMap) -> Result<Response> {
    let mut inner = access.inner.lock().map_err(unavailable)?;
    access.refresh(&mut inner)?;
    let payload = request_token(&access, &headers).and_then(|t| access.verify(&inner.config, &t));
    let enabled = !effective(&inner.config).is_empty();
    if enabled && payload.is_none() {
        return Err(failure(
            StatusCode::UNAUTHORIZED,
            "auth.unauthorized",
            "登录已失效，请重新登录。",
        ));
    }
    let mut data = access.identity(enabled);
    data["authenticated"] = json!(true);
    data["expires_at"] = json!(payload.map(|p| p.exp));
    data["in_container"] = json!(false);
    Ok(response(data))
}
fn proxy_peer(request: &Request) -> Option<(String, bool)> {
    let peer = request.extensions().get::<ConnectInfo<SocketAddr>>()?.0.ip();
    if !peer.is_loopback() { return None; }
    let key = std::env::var("MUTEKI_WEB_PROXY_KEY").ok().filter(|key| !key.is_empty())?;
    let parts: Vec<_> = request.headers().get("x-muteki-proxy-peer")?.to_str().ok()?.split('|').collect();
    if parts.len() != 4 { return None; }
    let stamp = parts[0].parse::<i64>().ok()?;
    if (now() - stamp).abs() > 60 || !matches!(parts[2], "http" | "https") { return None; }
    let ip = parts[1].parse::<std::net::IpAddr>().ok()?;
    let payload = format!("{}\n{}\n{}\n{}\n{}", parts[0], parts[1], parts[2], request.method(), request.uri().path());
    let mut mac = HmacSha256::new_from_slice(key.as_bytes()).ok()?;
    mac.update(payload.as_bytes());
    if parts[3].len() != 64 || !parts[3].bytes().all(|b| b.is_ascii_hexdigit()) { return None; }
    let signature: Vec<u8> = (0..64).step_by(2).map(|i| u8::from_str_radix(&parts[3][i..i+2],16)).collect::<std::result::Result<_,_>>().ok()?;
    mac.verify_slice(&signature).ok()?;
    Some((ip.to_string(), parts[2] == "https"))
}
async fn login(State(access): State<Arc<Access>>, request: Request) -> Result<Response> {
    let (peer, secure) = proxy_peer(&request).unwrap_or_else(|| (
        request.extensions().get::<ConnectInfo<SocketAddr>>().map(|p| p.0.ip().to_string()).unwrap_or_else(|| "local".into()),
        request.uri().scheme_str() == Some("https"),
    ));
    let bytes = axum::body::to_bytes(request.into_body(), 16 * 1024)
        .await
        .map_err(|_| {
            failure(
                StatusCode::UNPROCESSABLE_ENTITY,
                "auth.body_invalid",
                "Invalid login body",
            )
        })?;
    let body: Value = if bytes.is_empty() {
        json!({})
    } else {
        serde_json::from_slice(&bytes).map_err(|_| {
            failure(
                StatusCode::UNPROCESSABLE_ENTITY,
                "auth.body_invalid",
                "Invalid login body",
            )
        })?
    };
    let client = body.get("client").and_then(Value::as_str).unwrap_or("api");
    if !body.is_object()
        || !matches!(client, "web" | "desktop" | "api")
        || body.get("client").is_some_and(|v| !v.is_string())
        || body.get("remember").is_some_and(|v| !v.is_boolean())
    {
        return Err(failure(
            StatusCode::UNPROCESSABLE_ENTITY,
            "auth.client_invalid",
            "Invalid login client or remember field",
        ));
    }
    tokio::task::spawn_blocking(move || {
        let mut inner = access.inner.lock().map_err(unavailable)?;
        access.refresh(&mut inner)?;
        inner.failures.retain(|_, values| {
            values.retain(|v| v.elapsed() < Duration::from_secs(60));
            !values.is_empty()
        });
        if inner.failures.get(&peer).is_some_and(|v| v.len() >= 10) || inner.failures.len() >= 2048
        {
            let mut response = failure(
                StatusCode::TOO_MANY_REQUESTS,
                "auth.rate_limited",
                "尝试次数过多，请一分钟后重试。",
            )
            .into_response();
            response
                .headers_mut()
                .insert(header::RETRY_AFTER, HeaderValue::from_static("60"));
            return Ok(response);
        }
        let enabled = !effective(&inner.config).is_empty();
        if enabled
            && !body
                .get("password")
                .and_then(Value::as_str)
                .is_some_and(|p| matches(effective(&inner.config), p))
        {
            inner.failures.entry(peer).or_default().push(Instant::now());
            return Err(failure(
                StatusCode::UNAUTHORIZED,
                "auth.unauthorized",
                "invalid password",
            ));
        }
        let (token, exp) = if enabled {
            let (t, e) = access.issue(&inner.config)?;
            (t, Some(e))
        } else {
            (String::new(), None)
        };
        let client = body.get("client").and_then(Value::as_str).unwrap_or("api");
        let mut data = access.identity(enabled);
        data["ok"] = json!(true);
        data["expires_at"] = json!(exp);
        data["session_owner"] = json!(if client == "web" { "cookie" } else { client });
        if client != "web" {
            data["token"] = json!(token);
        }
        let mut result = response(data);
        if client == "web" && enabled {
            let age = if body
                .get("remember")
                .and_then(Value::as_bool)
                .unwrap_or(true)
            {
                format!("; Max-Age={}", access.ttl)
            } else {
                String::new()
            };
            let value = format!(
                "{}={}; Path=/api; HttpOnly; SameSite=Strict{}{}",
                access.cookie_name(),
                token,
                age,
                if secure { "; Secure" } else { "" }
            );
            result.headers_mut().insert(
                header::SET_COOKIE,
                HeaderValue::from_str(&value).map_err(unavailable)?,
            );
        }
        Ok(result)
    })
    .await
    .map_err(unavailable)?
}
pub(crate) async fn settings(State(access): State<Arc<Access>>) -> Result<Response> {
    let mut inner = access.inner.lock().map_err(unavailable)?;
    access.refresh(&mut inner)?;
    Ok(response(access.settings(&inner.config)))
}
pub(crate) async fn update(State(access): State<Arc<Access>>, Json(body): Json<Value>) -> Result<Response> {
    if !body.is_object()
        || ["action", "current_password", "new_password"]
            .iter()
            .any(|k| body.get(k).is_some_and(|v| !v.is_string()))
    {
        return Err(failure(
            StatusCode::UNPROCESSABLE_ENTITY,
            "auth.body_invalid",
            "Access fields must be strings",
        ));
    }
    tokio::task::spawn_blocking(move || {
        let mut inner = access.inner.lock().map_err(unavailable)?;
        let _file = file_lock(&access.path).map_err(unavailable)?;
        access.refresh(&mut inner)?;
        let field_error = |status, code, message, field| {
            let (s, Json(mut value)) = failure(status, code, message);
            value["error"]["field"] = json!(field);
            value["error"]["recovery_hint"] = json!("修正字段后重试。");
            (s, Json(value))
        };
        if !effective(&inner.config).is_empty()
            && !matches(
                effective(&inner.config),
                body["current_password"].as_str().unwrap_or(""),
            )
        {
            return Err(field_error(
                StatusCode::FORBIDDEN,
                "auth.current_password_invalid",
                "当前密码不正确。",
                "current_password",
            ));
        }
        let mut candidate = inner.config.clone();
        match body["action"].as_str().unwrap_or("") {
            "set" => {
                let password = body["new_password"].as_str().unwrap_or("");
                if password.chars().count() < 8 || password.len() > 1024 {
                    return Err(field_error(
                        StatusCode::UNPROCESSABLE_ENTITY,
                        "auth.password_invalid",
                        "新密码需为 8 个字符以上，且不超过 1024 字节。",
                        "new_password",
                    ));
                }
                candidate.password_hash = password_hash(password).map_err(unavailable)?;
                candidate.mode = "settings".into();
            }
            "inherit" => {
                if access.exposed && candidate.environment_hash.is_empty() {
                    return Err(field_error(
                        StatusCode::CONFLICT,
                        "auth.password_required",
                        "当前服务对外开放，恢复环境变量前需先配置环境变量密码。",
                        "action",
                    ));
                }
                candidate.password_hash.clear();
                candidate.mode = "environment".into();
            }
            _ => {
                return Err(field_error(
                    StatusCode::UNPROCESSABLE_ENTITY,
                    "auth.action_invalid",
                    "请选择设置密码或恢复环境变量。",
                    "action",
                ));
            }
        }
        candidate.revision = random(36).map_err(unavailable)?;
        candidate.revoked.clear();
        write_private(&access.path, &candidate).map_err(unavailable)?;
        inner.config = candidate;
        inner.tickets.clear();
        let mut data = access.settings(&inner.config);
        data["ok"] = json!(true);
        data["reauthenticate"] = json!(!effective(&inner.config).is_empty());
        let mut result = response(data);
        clear_cookie(&access, &mut result);
        Ok(result)
    })
    .await
    .map_err(unavailable)?
}
async fn logout(State(access): State<Arc<Access>>, headers: HeaderMap) -> Result<Response> {
    tokio::task::spawn_blocking(move || {
        let mut inner = access.inner.lock().map_err(unavailable)?;
        let _file = file_lock(&access.path).map_err(unavailable)?;
        access.refresh(&mut inner)?;
        if let Some(payload) =
            request_token(&access, &headers).and_then(|t| access.verify(&inner.config, &t))
        {
            let mut candidate = inner.config.clone();
            candidate.revoked.retain(|_, exp| *exp > now());
            candidate.revoked.insert(payload.jti, payload.exp);
            write_private(&access.path, &candidate).map_err(unavailable)?;
            inner.config = candidate;
        }
        inner.tickets.clear();
        let mut result = response(json!({"ok":true}));
        clear_cookie(&access, &mut result);
        Ok(result)
    })
    .await
    .map_err(unavailable)?
}
async fn ticket(State(access): State<Arc<Access>>, headers: HeaderMap) -> Result<Response> {
    let mut inner = access.inner.lock().map_err(unavailable)?;
    access.refresh(&mut inner)?;
    let token = request_token(&access, &headers);
    if !effective(&inner.config).is_empty()
        && !token
            .as_ref()
            .is_some_and(|t| access.verify(&inner.config, t).is_some())
    {
        return Err(failure(
            StatusCode::UNAUTHORIZED,
            "auth.unauthorized",
            "登录已失效，请重新登录。",
        ));
    }
    inner.tickets.retain(|_, t| t.expires > now());
    let ticket = random(32).map_err(unavailable)?;
    let revision = inner.config.revision.clone();
    inner.tickets.insert(
        ticket.clone(),
        Ticket {
            expires: now() + 30,
            token,
            revision,
        },
    );
    Ok(response(json!({"ticket":ticket})))
}
fn public_share(method: &Method, path: &str) -> bool {
    if method != Method::GET && method != Method::HEAD {
        return false;
    }
    let parts: Vec<_> = path.trim_start_matches('/').split('/').collect();
    parts.len() >= 3
        && parts[0] == "api"
        && parts[1] == "conversation-shares"
        && !parts[2].is_empty()
        && (parts.len() == 3
            || (parts.len() == 5 && parts[3] == "attachments" && !parts[4].is_empty()))
}
fn stream_path(path: &str) -> bool {
    let p: Vec<_> = path.trim_start_matches('/').split('/').collect();
    let events = p.len() == 4
        && p[0] == "api"
        && matches!(p[1], "threads" | "runs")
        && !p[2].is_empty()
        && p[3] == "events";
    let terminal = p.len() == 8
        && p[..2] == ["api", "threads"]
        && !p[2].is_empty()
        && p[3..6] == ["workspace", "terminal", "sessions"]
        && !p[6].is_empty()
        && p[7] == "stream";
    events || terminal
}
fn query_ticket(uri: &axum::http::Uri) -> Option<String> {
    reqwest::Url::parse(&format!("http://localhost{}", uri))
        .ok()?
        .query_pairs()
        .find(|(k, _)| k == "ticket")
        .map(|(_, v)| v.into_owned())
}
/// Protect every API route, preserve public share reads, bind SSE lifetime to the
/// session, and require the ordinary cookie CSRF header on browser mutations.
pub async fn gate(State(access): State<Arc<Access>>, mut request: Request, next: Next) -> Response {
    match authorize(&access, &mut request) {
        Err(error) => {
            let mut response = error.into_response();
            response
                .headers_mut()
                .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
            response
        }
        Ok(session) => {
            if let Some((token, revision)) = &session {
                request.extensions_mut().insert(SessionBinding {
                    token: token.clone(),
                    revision: revision.clone(),
                });
            }
            let mut result = next.run(request).await;
            if result
                .headers()
                .get(header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .is_some_and(|v| v.starts_with("text/event-stream"))
            {
                if let Some((token, revision)) = session {
                    let (parts, body) = result.into_parts();
                    let data = body.into_data_stream();
                    let interval = tokio::time::interval(Duration::from_secs(1));
                    let guarded = stream::unfold(
                        (data, interval, access, token, revision),
                        |(mut data, mut interval, access, token, revision)| async move {
                            loop {
                                if !access.still_valid(&token, &revision) {
                                    return None;
                                }
                                tokio::select! {
                                    chunk=data.next()=>return chunk.map(|chunk|(chunk,(data,interval,access,token,revision))),
                                    _=interval.tick()=>{}
                                }
                            }
                        },
                    );
                    result = Response::from_parts(parts, Body::from_stream(guarded));
                }
            }
            result
        }
    }
}
/// The credential a request was admitted with, so a long-lived connection
/// can end when that session is revoked or the password changes.
#[derive(Clone, Debug)]
pub struct SessionBinding {
    pub token: Option<String>,
    pub revision: String,
}
#[derive(Clone, Debug)]
pub(crate) struct GatewayCandidate;

impl Access {
    pub fn still_valid(&self, token: &Option<String>, revision: &str) -> bool {
        self.inner.lock().ok().is_some_and(|mut inner| {
            self.refresh(&mut inner).is_ok()
                && inner.config.revision == revision
                && (effective(&inner.config).is_empty()
                    || token
                        .as_ref()
                        .is_some_and(|t| self.verify(&inner.config, t).is_some()))
        })
    }
}

fn authorize(access: &Access, request: &mut Request) -> Result<Option<(Option<String>, String)>> {
    let path = request.uri().path();
    if !path.starts_with("/api/") || request.method() == Method::OPTIONS {
        return Ok(None);
    }
    if crate::capability_gateway::gateway_path(path) {
        return Ok(None);
    }
    if crate::system_api::desktop_control_path(path)
        && crate::system_api::desktop_control(request.headers())
    {
        return Ok(None);
    }
    let headers = request.headers();
    if !matches!(
        *request.method(),
        Method::GET | Method::HEAD | Method::OPTIONS
    ) {
        let origin = header_text(headers, "origin");
        let own = header_text(headers, "host")
            .map(|host| format!("{}://{host}", request.uri().scheme_str().unwrap_or("http")));
        if origin.is_some_and(|v| {
            Some(v) != own.as_deref() && !access.origins.iter().any(|allowed| allowed == v)
        }) {
            return Err(failure(
                StatusCode::FORBIDDEN,
                "auth.origin_invalid",
                "请求来源不受信任，请从工作台页面重试。",
            ));
        }
        if bearer(headers).is_none()
            && (cookie(access, headers).is_some() || origin.is_some())
            && header_text(headers, "x-muteki-csrf") != Some("1")
        {
            return Err(failure(
                StatusCode::FORBIDDEN,
                "auth.origin_invalid",
                "缺少访问校验，请刷新工作台后重试。",
            ));
        }
    }
    let mut inner = access.inner.lock().map_err(unavailable)?;
    access.refresh(&mut inner)?;
    let enabled = !effective(&inner.config).is_empty();
    let mut token = request_token(access, headers);
    let mut payload = token.as_ref().and_then(|t| access.verify(&inner.config, t));
    if request.method() == Method::GET && stream_path(path) {
        if let Some(ticket) = query_ticket(request.uri()).and_then(|t| inner.tickets.remove(&t)) {
            if ticket.expires > now() && ticket.revision == inner.config.revision {
                if let Some(ticket_payload) = ticket
                    .token
                    .as_ref()
                    .and_then(|t| access.verify(&inner.config, t))
                {
                    token = ticket.token;
                    payload = Some(ticket_payload);
                }
            }
        }
    }
    let public = matches!(path, "/api/auth/login" | "/api/auth/status" | "/api/health")
        || public_share(request.method(), path);
    if enabled && !public && payload.is_none() {
        if matches!(
            path,
            "/api/commands" | "/api/queries" | "/api/events/read" | "/api/events/wait"
        ) && bearer(headers).is_some()
        {
            request.extensions_mut().insert(GatewayCandidate);
            return Ok(None);
        }
        return Err(failure(
            StatusCode::UNAUTHORIZED,
            "auth.unauthorized",
            "登录已失效，请重新登录。",
        ));
    }
    if let Some(payload) = payload {
        request.extensions_mut().insert(AuthenticatedSession {
            expires_at: payload.exp,
        });
    }
    Ok(Some((token, inner.config.revision.clone())))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> (tempfile::TempDir, Arc<Access>) {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("_secrets/web-access.json");
        let _lock = file_lock(&path).unwrap();
        let config = PrivateConfig {
            version: 1,
            mode: "settings".into(),
            password_hash: password_hash("sample password").unwrap(),
            environment_hash: String::new(),
            signing_key: random(32).unwrap(),
            revision: random(36).unwrap(),
            revoked: HashMap::new(),
            secret_fingerprint: String::new(),
        };
        write_private(&path, &config).unwrap();
        let access = Arc::new(Access {
            path,
            service_id: "installation-a".into(),
            explicit_secret: String::new(),
            ttl: 43200,
            origins: vec!["http://localhost:3001".into()],
            exposed: false,
            inner: Mutex::new(Inner {
                config,
                tickets: HashMap::new(),
                failures: HashMap::new(),
            }),
        });
        (directory, access)
    }
    fn request(method: &str, path: &str) -> Request {
        Request::builder()
            .method(method)
            .uri(path)
            .body(Body::empty())
            .unwrap()
    }
    #[test]
    fn an_exposed_service_keeps_its_password() {
        let (directory, open) = fixture();
        let exposed = Arc::new(Access {
            path: open.path.clone(),
            service_id: open.service_id.clone(),
            explicit_secret: String::new(),
            ttl: open.ttl,
            origins: Vec::new(),
            exposed: true,
            inner: Mutex::new(Inner {
                config: open.inner.lock().unwrap().config.clone(),
                tickets: HashMap::new(),
                failures: HashMap::new(),
            }),
        });
        assert!(exposed.check_exposure("0.0.0.0").is_ok());
        let settings = exposed.settings(&exposed.inner.lock().unwrap().config);
        assert_eq!(
            (
                settings["can_disable"].clone(),
                settings["local_only"].clone()
            ),
            (json!(false), json!(false))
        );
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let body = json!({"action": "inherit", "current_password": "sample password"});
        let refused = runtime
            .block_on(update(State(exposed.clone()), Json(body.clone())))
            .unwrap_err();
        assert_eq!(
            (refused.0, refused.1["error"]["code"].clone()),
            (StatusCode::CONFLICT, json!("auth.password_required"))
        );
        assert!(
            runtime
                .block_on(update(State(open.clone()), Json(body)))
                .is_ok()
        );
        assert!(
            exposed.check_exposure("0.0.0.0").is_err(),
            "the shared password file no longer has a password"
        );
        drop(directory);
    }
    #[test]
    fn passwords_sessions_and_configuration_fail_closed() {
        let (_directory, access) = fixture();
        let mut inner = access.inner.lock().unwrap();
        assert!(matches(&inner.config.password_hash, "sample password"));
        assert!(!matches(&inner.config.password_hash, "wrong password"));
        let (token, _) = access.issue(&inner.config).unwrap();
        let payload = access.verify(&inner.config, &token).unwrap();
        let mut foreign = inner.config.clone();
        foreign.revision = random(36).unwrap();
        assert!(access.verify(&foreign, &token).is_none());
        let mut altered = token.clone();
        altered.push('x');
        assert!(access.verify(&inner.config, &altered).is_none());
        inner.config.revoked.insert(payload.jti, payload.exp);
        assert!(access.verify(&inner.config, &token).is_none());
        std::fs::write(&access.path, b"{}").unwrap();
        assert!(access.refresh(&mut inner).is_err());
    }
    #[test]
    fn gate_requires_csrf_and_one_use_session_bound_tickets() {
        let (_directory, access) = fixture();
        assert!(authorize(&access, &mut request("GET", "/api/settings/access")).is_err());
        assert!(authorize(&access, &mut request("GET", "/api/conversation-shares/a")).is_ok());
        assert!(authorize(&access, &mut request("POST", "/api/conversation-shares/a")).is_err());
        let (token, _) = {
            let inner = access.inner.lock().unwrap();
            access.issue(&inner.config).unwrap()
        };
        let mut mutation = request("PUT", "/api/settings/access");
        mutation.headers_mut().insert(
            header::COOKIE,
            HeaderValue::from_str(&format!("{}={token}", access.cookie_name())).unwrap(),
        );
        assert_eq!(
            authorize(&access, &mut mutation).unwrap_err().0,
            StatusCode::FORBIDDEN
        );
        mutation
            .headers_mut()
            .insert("x-muteki-csrf", HeaderValue::from_static("1"));
        assert!(authorize(&access, &mut mutation).is_ok());
        mutation.headers_mut().insert(
            header::ORIGIN,
            HeaderValue::from_static("http://localhost:9999"),
        );
        assert_eq!(
            authorize(&access, &mut mutation).unwrap_err().0,
            StatusCode::FORBIDDEN
        );
        {
            let mut inner = access.inner.lock().unwrap();
            let revision = inner.config.revision.clone();
            inner.tickets.insert(
                "one-use".into(),
                Ticket {
                    expires: now() + 30,
                    token: Some(token),
                    revision,
                },
            );
        }
        assert!(
            authorize(
                &access,
                &mut request("GET", "/api/threads/a/events?ticket=one-use")
            )
            .is_ok()
        );
        assert!(
            authorize(
                &access,
                &mut request("GET", "/api/threads/a/events?ticket=one-use")
            )
            .is_err()
        );
    }
}
